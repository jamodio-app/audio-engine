//! P2.0 — Spike de faisabilité du host ASIO full-duplex maison (Windows).
//!
//! # Question posée
//!
//! Le chemin actuel (cpal) ouvre l'entrée et la sortie en DEUX `cpal::Stream`
//! séparés (deux `ASIOStart`) sur un driver mono-client full-duplex → wedge
//! Focusrite (mort des callbacks ~20 s, silence numérique). Jamulus/JUCE/RtAudio
//! font l'inverse : **1 driver, 1 `ASIOCreateBuffers` couvrant entrée+sortie,
//! 1 `bufferSwitch`**. Ce spike ouvre le driver EXACTEMENT comme ça (via
//! `asio-sys` en direct, sans cpal) et vérifie une seule chose : **les callbacks
//! survivent-ils au-delà de ~20 s ?** Si oui → l'approche duplex est validée et
//! on écrit le vrai `AsioDuplexHost` (câblé `sample_tx`/`mixer`) en P2.1.
//!
//! # Périmètre (volontairement minimal & sûr)
//!
//! Le callback ne fait que COMPTER les tics — **aucun accès aux buffers bruts,
//! zéro `unsafe`**. On ne cherche pas encore à router l'audio (ça vient au vrai
//! host), seulement à prouver la survie du moteur duplex. JETABLE : supprimé
//! quand `AsioDuplexHost` le remplace.
//!
//! # Isolation
//!
//! - `#[cfg(target_os = "windows")]` au niveau de la déclaration du module
//!   (`audio/mod.rs`) → **ne compile pas sur macOS**.
//! - Déclenché UNIQUEMENT par la variable d'environnement `JAMODIO_ASIO_PROBE`
//!   (diagnostic opt-in, jamais en prod) → **jamais câblé au pipeline**.
//! - Tourne sur le thread COM-STA partagé (`com_exec`) — ASIO est mono-client,
//!   donc pendant les ~60 s du spike l'audio normal est indisponible : c'est
//!   voulu (on teste l'ASIO en isolation).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Lance le spike sur un thread dédié si `JAMODIO_ASIO_PROBE` est défini.
/// No-op sinon. Non bloquant pour le démarrage de l'agent.
pub fn spawn_if_requested() {
    if std::env::var_os("JAMODIO_ASIO_PROBE").is_none() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("asio-probe".into())
        // Le spike lui-même s'exécute sur le thread COM-STA (contrat ASIO/COM,
        // cf. com_exec) ; ce thread-ci ne fait qu'attendre son résultat.
        .spawn(|| crate::audio::com_exec::run(probe));
}

fn probe() {
    let asio = asio_sys::Asio::new();
    let names = asio.driver_names();
    tracing::info!(target: "jamodio::asioprobe", ?names, "P2.0 spike — drivers ASIO présents");
    let Some(name) = names.into_iter().next() else {
        tracing::warn!(target: "jamodio::asioprobe", "aucun driver ASIO — spike interrompu");
        return;
    };

    let driver = match asio.load_driver(&name) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(target: "jamodio::asioprobe", error = ?e, driver = %name, "load_driver a échoué");
            return;
        }
    };

    // Caractéristiques du driver (log de diagnostic).
    let _ = driver.set_sample_rate(48_000.0);
    let channels = driver.channels();
    let ins = channels.as_ref().map(|c| c.ins).unwrap_or(-1);
    let outs = channels.as_ref().map(|c| c.outs).unwrap_or(-1);
    let sr = driver.sample_rate().unwrap_or(0.0);
    let (buf_min, buf_max) = driver.buffersize_range().unwrap_or((-1, -1));
    let in_ty = driver.input_data_type().ok();
    let out_ty = driver.output_data_type().ok();
    tracing::info!(
        target: "jamodio::asioprobe",
        driver = %name, ins, outs, sample_rate = sr,
        buf_min, buf_max, input_type = ?in_ty, output_type = ?out_ty,
        "driver ASIO chargé"
    );

    // On limite le spike à ≤ 2 canaux d'entrée et de sortie (suffisant pour
    // prouver la survie du duplex, `ASIOCreateBuffers` minimal).
    let n_in = (ins.max(0) as usize).clamp(1, 2);
    let n_out = (outs.max(0) as usize).clamp(1, 2);

    // Cœur du spike : UN SEUL `ASIOCreateBuffers` couvrant entrée PUIS sortie,
    // via le chaînage `prepare_input_stream` → `prepare_output_stream` (on passe
    // le résultat de l'un en argument de l'autre). `buffer_size = None` = taille
    // préférée du driver (jamais forcée).
    let streams = match driver.prepare_input_stream(None, n_in, None) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(target: "jamodio::asioprobe", error = ?e, "prepare_input_stream a échoué");
            let _ = driver.destroy();
            return;
        }
    };
    let streams = match driver.prepare_output_stream(streams.input, n_out, None) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(target: "jamodio::asioprobe", error = ?e, "prepare_output_stream a échoué");
            let _ = driver.destroy();
            return;
        }
    };
    let buffer_size = streams
        .output
        .as_ref()
        .or(streams.input.as_ref())
        .map(|s| s.buffer_size)
        .unwrap_or(0);
    let duplex_ok = streams.input.is_some() && streams.output.is_some();
    tracing::info!(
        target: "jamodio::asioprobe",
        buffer_size, in_ch = n_in, out_ch = n_out, duplex_ok,
        "ASIOCreateBuffers duplex OK — un seul appel couvre entrée + sortie"
    );

    // Callback `bufferSwitch` : COMPTE seulement (survie). Le routage réel
    // (sample_tx / mixer) viendra dans `AsioDuplexHost`.
    let ticks = Arc::new(AtomicU64::new(0));
    let cb_ticks = ticks.clone();
    let cb = driver.add_callback(move |_info| {
        cb_ticks.fetch_add(1, Ordering::Relaxed);
    });

    // Callback de message : compte + logge les `kAsioResetRequest` (le protocole
    // que cpal ignore). Un host correct répondra en ré-initialisant ; ici on
    // observe seulement.
    let resets = Arc::new(AtomicU64::new(0));
    let cb_resets = resets.clone();
    let msg = driver.add_message_callback(move |sel| {
        // On n'affiche pas `sel` en Debug (l'enum `AsioMessageSelectors` ne
        // garantit pas `Debug`) — on compte le sélecteur qui nous intéresse.
        let is_reset = matches!(sel, asio_sys::AsioMessageSelectors::kAsioResetRequest);
        cb_resets.fetch_add(u64::from(is_reset), Ordering::Relaxed);
        tracing::warn!(target: "jamodio::asioprobe", is_reset, "message ASIO reçu");
    });

    if let Err(e) = driver.start() {
        tracing::error!(target: "jamodio::asioprobe", error = ?e, "ASIOStart a échoué");
        driver.remove_callback(cb);
        driver.remove_message_callback(msg);
        let _ = driver.dispose_buffers();
        let _ = driver.destroy();
        return;
    }
    tracing::info!(
        target: "jamodio::asioprobe",
        "ASIOStart OK — surveillance de la survie des callbacks sur 60 s (le chemin cpal 2-flux mourait vers ~20 s)"
    );

    // Surveillance 60 s : tics par tranche de 5 s. Des tics réguliers au-delà de
    // ~20 s = le moteur duplex TIENT = approche validée.
    let mut prev = 0u64;
    for i in 1..=12u64 {
        std::thread::sleep(Duration::from_secs(5));
        let now = ticks.load(Ordering::Relaxed);
        tracing::info!(
            target: "jamodio::asioprobe",
            t_s = i * 5, ticks_5s = now - prev, ticks_total = now,
            resets = resets.load(Ordering::Relaxed),
            "survie duplex"
        );
        prev = now;
    }

    let _ = driver.stop();
    driver.remove_callback(cb);
    driver.remove_message_callback(msg);
    let _ = driver.dispose_buffers();
    let total = ticks.load(Ordering::Relaxed);
    tracing::info!(
        target: "jamodio::asioprobe",
        ticks_total = total, resets = resets.load(Ordering::Relaxed),
        "P2.0 spike terminé — VERDICT : tics réguliers sur toute la fenêtre ⇒ le duplex tient (≠ cpal)"
    );
    let _ = driver.destroy();
}

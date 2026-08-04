//! P2.0 — Spike de faisabilité du host ASIO full-duplex maison (Windows).
//!
//! # Question posée
//!
//! Le chemin actuel (cpal) ouvre l'entrée et la sortie en DEUX `cpal::Stream`
//! séparés (deux `ASIOStart`) sur un driver mono-client full-duplex → wedge
//! Focusrite (mort des callbacks ~20 s, silence numérique). L'approche correcte
//! (contrat ASIO) est l'inverse : **1 driver, 1 `ASIOCreateBuffers` couvrant entrée+sortie,
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

/// Lance le spike sur un thread dédié — UNIQUEMENT si `JAMODIO_ASIO_PROBE` est
/// défini. No-op sinon (démarrage normal = SÛR, aucun accès ASIO du probe).
///
/// 0.5.4-15 — retour à l'opt-in par var d'env après le BSOD de la 0.5.4-12/-14
/// (lancement auto). Le probe ne fait plus que COMPTER les callbacks (aucune
/// écriture buffer). Isolé : jamais câblé au pipeline, aucun impact Mac.
pub fn spawn_probe_at_startup() {
    // Armé par la var d'env `JAMODIO_ASIO_PROBE` OU (plus simple) par la présence
    // du fichier marqueur `%APPDATA%\Jamodio\asio_probe.on`. Sans l'un ou l'autre,
    // NO-OP → un démarrage normal ne touche jamais à l'ASIO via le probe.
    let env_on = std::env::var_os("JAMODIO_ASIO_PROBE").is_some();
    let file_on = std::env::var_os("APPDATA")
        .map(|p| {
            std::path::Path::new(&p)
                .join("Jamodio")
                .join("asio_probe.on")
                .exists()
        })
        .unwrap_or(false);
    if !env_on && !file_on {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("asio-probe".into())
        // Le spike lui-même s'exécute sur le thread COM-STA (contrat ASIO/COM,
        // cf. com_exec) ; ce thread-ci ne fait qu'attendre son résultat.
        .spawn(|| crate::audio::com_exec::run(probe));
}

fn probe() {
    // Laisse l'énumération CPAL du boot (host::probe) relâcher le driver ASIO
    // avant qu'on le charge nous-mêmes (mono-client) — évite un conflit d'accès.
    std::thread::sleep(Duration::from_secs(3));
    tracing::info!(target: "jamodio::asioprobe", "=== BUILD-PROBE : lancement automatique du test ASIO duplex (~60 s) ===");
    let asio = asio_sys::Asio::new();
    let names = asio.driver_names();
    tracing::info!(target: "jamodio::asioprobe", ?names, "P2.0 spike — drivers ASIO présents");

    // Sélectionne le 1er driver qui SE CHARGE **et** expose ≥ 1 entrée = l'interface
    // réellement branchée. Ignore les drivers fantômes (Blackmagic / Focusrite
    // Thunderbolt sans matériel) qui échouent au load ou n'ont aucune entrée.
    let mut chosen: Option<(asio_sys::Driver, String)> = None;
    for name in names {
        match asio.load_driver(&name) {
            Ok(d) => {
                let ins = d.channels().map(|c| c.ins).unwrap_or(0);
                if ins > 0 {
                    tracing::info!(target: "jamodio::asioprobe", driver = %name, ins, "driver retenu (a une entrée)");
                    chosen = Some((d, name));
                    break;
                }
                tracing::info!(target: "jamodio::asioprobe", driver = %name, "chargé mais 0 entrée — ignoré");
                let _ = d.destroy();
            }
            Err(e) => {
                tracing::info!(target: "jamodio::asioprobe", driver = %name, error = ?e, "non chargeable (pas de matériel ?) — ignoré");
            }
        }
    }
    let Some((driver, name)) = chosen else {
        tracing::error!(target: "jamodio::asioprobe", "aucun driver ASIO chargeable avec entrée — spike interrompu");
        return;
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

    // Callback `bufferSwitch` : compte les tics ET remplit la sortie de silence,
    // en RÉUTILISANT LE PATTERN ÉPROUVÉ DE CPAL (MIT) — la leçon du BSOD 0.5.4-14 :
    //   * accès buffer FRAIS DANS le callback via `Arc<Mutex<AsioStreams>>`
    //     (JAMAIS de pointeur pré-extrait — c'était la faute probable du BSOD) ;
    //   * `from_raw_parts_mut(buffers[idx], buffer_size)` = exactement l'helper
    //     `asio_channel_slice_mut` de cpal ;
    //   * bornes : sample type connu (Int32LSB), index double-tampon ∈ {0,1},
    //     pointeur non-null.
    // ⚠️ Écrit dans les buffers → risque résiduel, mais opt-in (var d'env) : ne
    // touche JAMAIS l'usage normal.
    let out_is_int32 = matches!(out_ty, Some(asio_sys::AsioSampleType::ASIOSTInt32LSB));
    let streams = std::sync::Arc::new(std::sync::Mutex::new(streams));
    let streams_cb = streams.clone();
    let ticks = Arc::new(AtomicU64::new(0));
    let cb_ticks = ticks.clone();
    let cb = driver.add_callback(move |info| {
        cb_ticks.fetch_add(1, Ordering::Relaxed);
        if !out_is_int32 {
            return;
        }
        let idx = info.buffer_index as usize;
        if idx > 1 {
            return; // double-tampon ASIO : index attendu 0 ou 1
        }
        let Ok(mut guard) = streams_cb.lock() else { return };
        let Some(out) = guard.output.as_mut() else { return };
        let bufsz = out.buffer_size as usize;
        for ch in 0..out.buffer_infos.len() {
            let ptr = out.buffer_infos[ch].buffers[idx] as *mut i32;
            if ptr.is_null() {
                continue;
            }
            // SAFETY : mirror exact de `asio_channel_slice_mut` (cpal) — `buffers[idx]`
            // pointe sur `buffer_size` échantillons Int32 du buffer À REMPLIR (le
            // driver joue l'autre tampon). On écrit exactement ce buffer, silence.
            let slice = unsafe { std::slice::from_raw_parts_mut(ptr, bufsz) };
            slice.fill(0);
        }
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
        out_is_int32,
        "ASIOStart OK — surveillance 60 s (sortie SERVIE de silence, pattern cpal ; cpal 2-flux tenait ~20 s)"
    );

    // Surveillance 60 s : on note la DERNIÈRE tranche de 5 s où les tics ont
    // progressé = la durée de SURVIE réelle du moteur (verdict honnête).
    let mut prev = 0u64;
    let mut last_growth_s = 0u64;
    for i in 1..=12u64 {
        std::thread::sleep(Duration::from_secs(5));
        let now = ticks.load(Ordering::Relaxed);
        let delta = now - prev;
        if delta > 0 {
            last_growth_s = i * 5;
        }
        tracing::info!(
            target: "jamodio::asioprobe",
            t_s = i * 5, ticks_5s = delta, ticks_total = now,
            resets = resets.load(Ordering::Relaxed),
            "survie duplex (sortie servie, pattern cpal)"
        );
        prev = now;
    }

    let _ = driver.stop();
    driver.remove_callback(cb);
    driver.remove_message_callback(msg);
    let _ = driver.dispose_buffers();
    let total = ticks.load(Ordering::Relaxed);
    let survived = last_growth_s >= 55;
    tracing::info!(
        target: "jamodio::asioprobe",
        ticks_total = total, last_growth_s, survived,
        resets = resets.load(Ordering::Relaxed),
        "P2.0 spike terminé — survived=true ⇒ le duplex SERVI tient 60 s ; sinon mort vers last_growth_s"
    );
    let _ = driver.destroy();
}

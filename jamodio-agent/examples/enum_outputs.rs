//! Diagnostic Lot A (sortie mode agent) — compare l'énumération CPAL brute
//! (`host.devices()`, TOUS les devices) à `host.output_devices()` (que CPAL
//! PRÉ-FILTRE sur `supported_output_configs().next().is_some()`).
//!
//! But : quand un device virtuel « enregistreur » (ex. MJAudioRecorder) est la
//! sortie PAR DÉFAUT macOS, les haut-parleurs intégrés disparaissent de la liste
//! de l'agent. Ce diag tranche la cause :
//!   - HP présents dans `devices()` mais ABSENTS de `output_devices()` →
//!     c'est le pré-filtre CPAL (`supported_output_configs` échoue) →
//!     RÉCUPÉRABLE par une énumération tolérante (Lot B agent).
//!   - HP absents des DEUX → macOS/le pilote les cache réellement (agrégat qui
//!     possède le device) → NON récupérable côté agent (rien à coder).
//!
//! Usage (à lancer AVEC MJAudioRecorder réglé en sortie par défaut macOS) :
//!   cargo run -p jamodio-agent --example enum_outputs

use cpal::traits::{DeviceTrait, HostTrait};

fn main() {
    let host = cpal::default_host();
    println!("=== HOST: {:?} ===\n", host.id());

    let default_out = host
        .default_output_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_else(|| "<aucun>".into());
    println!("Sortie par DÉFAUT système : {default_out}\n");

    // 1) TOUS les devices (aucun pré-filtre CPAL).
    println!("── host.devices() — TOUS les devices (non filtrés) ──");
    match host.devices() {
        Ok(devs) => {
            for (i, d) in devs.enumerate() {
                let name = d.name().unwrap_or_else(|_| "<sans nom>".into());
                // Un device est « sortie » s'il expose au moins une config de sortie.
                let out_cfgs = d
                    .supported_output_configs()
                    .map(|it| it.count())
                    .unwrap_or(0);
                let out_cfgs_err = d.supported_output_configs().is_err();
                let default_out_cfg = match d.default_output_config() {
                    Ok(c) => format!("{}ch @ {}Hz", c.channels(), c.sample_rate().0),
                    Err(e) => format!("ERR({e})"),
                };
                println!(
                    "[{i}] {name}\n     supported_output_configs: {}{} · default_output_config: {}",
                    out_cfgs,
                    if out_cfgs_err { " (ERR)" } else { "" },
                    default_out_cfg,
                );
            }
        }
        Err(e) => println!("host.devices() a échoué : {e}"),
    }

    // 2) La liste que CPAL considère comme « sorties » (= ce que l'agent envoie).
    println!("\n── host.output_devices() — filtré CPAL (= liste agent) ──");
    match host.output_devices() {
        Ok(devs) => {
            let mut any = false;
            for (i, d) in devs.enumerate() {
                any = true;
                let name = d.name().unwrap_or_else(|_| "<sans nom>".into());
                println!("[{i}] {name}");
            }
            if !any {
                println!("(aucune sortie listée)");
            }
        }
        Err(e) => println!("host.output_devices() a échoué : {e}"),
    }

    // 3) Test décisif : tente d'OUVRIR chaque device en forçant 48k/2ch (SANS
    //    jouer de son — build seul, pas de play()). Si un device dont
    //    `supported_output_configs` échoue s'ouvre quand même → on POURRAIT le
    //    proposer (ex. port jack inactif « Écouteurs externes »). Sinon → non
    //    ouvrable, exclusion CPAL justifiée.
    println!("\n── test d'ouverture forcée 48k/2ch (build sans play) ──");
    if let Ok(devs) = host.devices() {
        let config = cpal::StreamConfig {
            channels: 2,
            sample_rate: cpal::SampleRate(48_000),
            buffer_size: cpal::BufferSize::Default,
        };
        for (i, d) in devs.enumerate() {
            let name = d.name().unwrap_or_else(|_| "<sans nom>".into());
            let res = d.build_output_stream(
                &config,
                move |data: &mut [f32], _| data.fill(0.0), // silence
                move |err| eprintln!("stream err: {err}"),
                None,
            );
            match res {
                Ok(_stream) => println!("[{i}] {name} → OUVRABLE (48k/2ch) ✅"),
                Err(e) => println!("[{i}] {name} → non ouvrable : {e}"),
            }
        }
    }

    // 4) Réplique de la NOUVELLE énumération tolérante macOS (list_outputs) :
    //    host.devices() + inclut si config queryable OU build-probe ok.
    println!("\n── NOUVELLE liste tolérante (= list_outputs macOS après fix) ──");
    let default = host
        .default_output_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_default();
    if let Ok(devs) = host.devices() {
        let probe_cfg = cpal::StreamConfig {
            channels: 2,
            sample_rate: cpal::SampleRate(48_000),
            buffer_size: cpal::BufferSize::Default,
        };
        for (idx, d) in devs.enumerate() {
            let Ok(name) = d.name() else { continue };
            let (keep, how) = if d.default_output_config().is_ok() {
                (true, "config")
            } else if d
                .build_output_stream(&probe_cfg, |x: &mut [f32], _| x.fill(0.0), |_| {}, None)
                .is_ok()
            {
                (true, "probe")
            } else {
                (false, "-")
            };
            if keep {
                let def = if name == default { " (défaut)" } else { "" };
                println!("  {idx}:{name}{def}  [{how}]");
            }
        }
    }
    println!(
        "\n=> Compare : un device présent en (1) mais ABSENT de (2) est écarté par \
         le pré-filtre CPAL. La liste (4) le RÉCUPÈRE si build-probe ok."
    );
}

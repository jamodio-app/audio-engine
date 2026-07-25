//! Diagnostic Lot A (sortie mode agent) — compare l'énumération CPAL brute
//! (`host.devices()`, TOUS les devices) à `host.output_devices()` (que CPAL
//! PRÉ-FILTRE sur `supported_output_configs().next().is_some()`).
//!
//! But : quand un device virtuel « enregistreur » (ex. MJAudioRecorder) est la
//! sortie PAR DÉFAUT macOS, les haut-parleurs intégrés disparaissent de la liste
//! de l'agent. Ce diag tranche la cause :
//!   - HP présents dans `devices()` mais ABSENTS de `output_devices()`
//!       → c'est le pré-filtre CPAL (`supported_output_configs` échoue) →
//!         RÉCUPÉRABLE par une énumération tolérante (Lot B agent).
//!   - HP absents des DEUX → macOS/le pilote les cache réellement (agrégat qui
//!         possède le device) → NON récupérable côté agent (rien à coder).
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

    println!(
        "\n=> Compare : un device présent en (1) mais ABSENT de (2) est écarté par \
         le pré-filtre CPAL (récupérable). Absent des DEUX = caché par macOS."
    );
}

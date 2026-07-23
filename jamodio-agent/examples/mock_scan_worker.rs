//! Worker de scan FACTICE — outil de test du coordinateur uniquement.
//!
//! Rejoue le contrat NDJSON du vrai worker (plugin_scan::protocol) mais sans
//! toucher au moindre plugin : il simule les pathologies (crash, hang) de
//! façon déterministe et portable, pour tester la boucle crash/respawn du
//! coordinateur sans dépendre d'un plugin réellement défectueux.
//!
//! Oracle INDÉPENDANT : émet le JSON à la main (pas via les types partagés)
//! — si le format wire dérive, le test le voit. Piloté par `JMO_MOCK_SCENARIO` :
//! - `clean`      : tous les items OK, 1 plugin chacun ;
//! - `crash-on-X` : émet `begin X` puis exit 101 (pas de `end`) → crash natif ;
//! - `hang-on-X`  : émet `begin X` puis dort → déclenche le watchdog.
//!
//! Ce binaire n'est jamais bundlé (Tauri ne package que l'app nommée) ; il
//! n'existe que pour `cargo test` (chemin exposé via `CARGO_BIN_EXE_*`).

use std::io::{BufRead, Write};

fn main() {
    let scenario = std::env::var("JMO_MOCK_SCENARIO").unwrap_or_else(|_| "clean".into());
    let crash_on = scenario.strip_prefix("crash-on-").map(str::to_string);
    let hang_on = scenario.strip_prefix("hang-on-").map(str::to_string);

    let stdin = std::io::stdin();
    let mut out = std::io::stdout();

    for line in stdin.lock().lines() {
        let item = match line {
            Ok(l) => l.trim().to_string(),
            Err(_) => break,
        };
        if item.is_empty() {
            continue;
        }

        emit(&mut out, &format!(r#"{{"event":"begin","item":{}}}"#, json_str(&item)));

        if crash_on.as_deref() == Some(item.as_str()) {
            std::process::exit(101); // crash simulé : pas de plugin, pas de end
        }
        if hang_on.as_deref() == Some(item.as_str()) {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }

        emit(&mut out, &plugin_line(&item));
        emit(&mut out, &format!(r#"{{"event":"end","item":{}}}"#, json_str(&item)));
    }
}

fn emit(out: &mut impl Write, line: &str) {
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Ligne `plugin` au format PluginInfo camelCase (cf. plugin_host.rs).
fn plugin_line(item: &str) -> String {
    let name = json_str(&format!("plug-{item}"));
    format!(
        r#"{{"event":"plugin","info":{{"name":{name},"manufacturer":"Mock",{r}"latencySamples":0,"hasEditor":false,"incompatible":false,"hasInputBus":true,"isInstrument":false}}}}"#,
        r = format_args!(r#""pluginRef":{{"format":"vst3","path":{},"uid":"00"}},"#, json_str(item)),
    )
}

/// Échappe une String en littéral JSON (guillemets inclus). Suffisant pour les
/// items de test (pas de contrôle exotique).
fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            _ => o.push(c),
        }
    }
    o.push('"');
    o
}

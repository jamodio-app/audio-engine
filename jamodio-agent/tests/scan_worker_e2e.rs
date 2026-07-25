//! E2E du worker de scan out-of-process, contre le VRAI binaire agent.
//!
//! Les tests unitaires ne peuvent pas spawner `agent --plugin-scan-worker`
//! (`current_exe()` = binaire de test). Les tests d'intégration, eux, ont
//! `CARGO_BIN_EXE_jamodio-agent` → on pilote le vrai worker exactement comme
//! le coordinateur en production : items sur stdin, NDJSON sur stdout.
//!
//! Complète les tests coordinateur (crash/hang/respawn contre le mock) : ici
//! on valide le binaire réel + la vraie probe plugin + le format wire.

use std::io::Write;
use std::process::{Command, Stdio};

/// Lance le vrai worker sur `items`, rend les lignes stdout (NDJSON).
fn run_worker(items: &[&str]) -> Vec<String> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_jamodio-agent"))
        .arg("--plugin-scan-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn worker");

    {
        let mut stdin = child.stdin.take().unwrap();
        for it in items {
            writeln!(stdin, "{it}").unwrap();
        }
        // Drop stdin → EOF → le worker épuise la liste et sort proprement.
    }

    let out = child.wait_with_output().expect("wait worker");
    assert!(out.status.success(), "worker exit non nul: {:?}", out.status);
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Compte les events par type dans les lignes NDJSON. Utilisé uniquement par
/// les tests macOS (probe AU réelle) → gaté pour ne pas être du code mort sur
/// Windows (où `-D warnings` en CI transformerait le warning en erreur).
#[cfg(target_os = "macos")]
fn count_events(lines: &[String]) -> (usize, usize, usize) {
    let (mut b, mut p, mut e) = (0, 0, 0);
    for l in lines {
        let v: serde_json::Value = serde_json::from_str(l).expect("ligne NDJSON valide");
        match v["event"].as_str() {
            Some("begin") => b += 1,
            Some("plugin") => p += 1,
            Some("end") => e += 1,
            other => panic!("event inattendu: {other:?}"),
        }
    }
    (b, p, e)
}

#[test]
fn empty_input_exits_clean() {
    let lines = run_worker(&[]);
    assert!(lines.is_empty(), "aucune sortie attendue: {lines:?}");
}

/// macOS : le worker probe de vrais AudioUnits Apple natifs (présents partout).
#[cfg(target_os = "macos")]
#[test]
fn probes_real_apple_audiounits() {
    // Effet, EQ, instrument DLS — identités 4-CC stables des AU Apple.
    let items = ["au:aufx/mrev/appl", "au:aufx/nbeq/appl", "au:aumu/dls /appl"];
    let lines = run_worker(&items);
    let (b, p, e) = count_events(&lines);
    assert_eq!(b, 3, "un begin par item");
    assert_eq!(e, 3, "un end par item");
    assert_eq!(p, 3, "un plugin par AU Apple probé");

    // Le format wire est bien camelCase + les métadonnées réelles y sont.
    let mrev = lines
        .iter()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["event"] == "plugin" && v["info"]["name"] == "AUMatrixReverb")
        .expect("AUMatrixReverb probé");
    assert_eq!(mrev["info"]["manufacturer"], "Apple");
    assert!(mrev["info"]["latencySamples"].is_number(), "camelCase attendu");
    assert_eq!(mrev["info"]["isInstrument"], false);

    // DLSMusicDevice est un instrument (aumu) sans bus d'entrée.
    let dls = lines
        .iter()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["event"] == "plugin" && v["info"]["name"] == "DLSMusicDevice")
        .expect("DLSMusicDevice probé");
    assert_eq!(dls["info"]["isInstrument"], true);
}

/// macOS : un item AU inconnu (désinstallé) → begin/end sans plugin, pas de
/// crash. Modélise un composant disparu entre l'énumération et la probe.
#[cfg(target_os = "macos")]
#[test]
fn unknown_au_item_yields_begin_end_no_plugin() {
    let lines = run_worker(&["au:aufx/zzzz/zzzz"]);
    let (b, p, e) = count_events(&lines);
    assert_eq!((b, p, e), (1, 0, 1));
}

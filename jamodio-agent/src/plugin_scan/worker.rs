//! Process worker de scan — point d'entrée du mode `--plugin-scan-worker`.
//!
//! Process enfant JETABLE : il charge et instancie réellement les plugins
//! (y compris tiers — c'est tout l'intérêt : un crash natif tue CE process,
//! pas l'agent). Aucun état Tauri, pas de single-instance, pas de port 9876,
//! pas de tray : `main()` court-circuite vers [`run`] avant tout ça.
//!
//! Contrat I/O (cf. protocol.rs) :
//! - stdin  : un item par ligne (path .vst3 Windows, `au:…` macOS) ;
//! - stdout : NDJSON `begin`/`plugin`/`end`, flush après CHAQUE ligne
//!   (contractuel — désignation du coupable au crash) ;
//! - stderr : logs tracing (relayés par le coordinateur dans le log agent).
//!
//! Un panic Rust pendant un scan = abort du worker (exit ≠ 0) : traité par le
//! coordinateur exactement comme un crash natif, l'item courant est
//! blocklisté. C'est voulu — pas de catch_unwind ici, le process EST le filet.

use std::io::{BufRead, Write};

use jamodio_audio_core::plugin_host::PluginInfo;

use super::protocol::WorkerEvent;

/// Point d'entrée du worker. Ne retourne jamais.
pub fn run() -> ! {
    // macOS : le worker partage le binaire (donc l'Info.plist « app Regular »)
    // de l'agent → sans ça son icône rebondit dans le Dock le temps du scan.
    // On le déclare process d'arrière-plan AVANT tout usage AppKit/AVFoundation.
    // (Windows : le worker est déjà invisible via CREATE_NO_WINDOW au spawn.)
    #[cfg(target_os = "macos")]
    jamodio_au_host::suppress_dock_for_helper();

    init_stderr_tracing();
    tracing::info!(
        target: "jamodio::scan-worker",
        version = env!("CARGO_PKG_VERSION"),
        "plugin scan worker starting"
    );
    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    let code = run_loop(stdin, stdout, scan_item);
    std::process::exit(code);
}

/// Boucle du worker, factorisée pour les tests (I/O et scan injectés).
///
/// Retourne le code de sortie : 0 = liste épuisée proprement, 1 = stdout
/// cassé (coordinateur mort — plus personne à qui parler, on s'arrête).
fn run_loop(
    input: impl BufRead,
    mut out: impl Write,
    scan: impl Fn(&str) -> Vec<PluginInfo>,
) -> i32 {
    for line in input.lines() {
        let line = match line {
            Ok(l) => l,
            // stdin fermé/corrompu = fin de liste côté coordinateur.
            Err(_) => break,
        };
        let item = line.trim();
        if item.is_empty() {
            continue;
        }

        if emit(&mut out, &WorkerEvent::Begin { item: item.to_string() }).is_err() {
            return 1;
        }

        // C'est ICI que le process peut mourir (crash natif du plugin) —
        // le `begin` flushé ci-dessus désigne alors l'item coupable.
        let infos = scan(item);

        for info in infos {
            if emit(&mut out, &WorkerEvent::Plugin { info }).is_err() {
                return 1;
            }
        }
        if emit(&mut out, &WorkerEvent::End { item: item.to_string() }).is_err() {
            return 1;
        }
    }
    tracing::info!(target: "jamodio::scan-worker", "item list exhausted — worker done");
    0
}

/// Sérialise + écrit + flush UNE ligne NDJSON. Le flush par ligne est
/// contractuel (cf. module doc).
fn emit(out: &mut impl Write, event: &WorkerEvent) -> std::io::Result<()> {
    // Un WorkerEvent est toujours sérialisable (types owned, pas de map à clé
    // non-string) — un échec ici serait un bug interne, pas une donnée externe.
    let json = serde_json::to_string(event).expect("WorkerEvent serialization");
    writeln!(out, "{json}")?;
    out.flush()
}

/// Subscriber tracing stderr-only : le worker n'écrit PAS dans le log rolling
/// de l'agent (deux writers sur le même fichier = interleaving) — le
/// coordinateur relaie stderr dans le log agent sous `jamodio::scan-worker`.
fn init_stderr_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,jamodio_agent=debug"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .compact()
        .init();
}

// ---------- Scan d'un item (par OS) ----------

/// Windows : un item = un path `.vst3`. La probe réelle (dlopen + factory +
/// instanciation + setup) vit dans jamodio-vst3-host ; elle exige le thread
/// vst3-main STA — spawné lazily DANS ce process par `main_thread`.
#[cfg(target_os = "windows")]
fn scan_item(item: &str) -> Vec<PluginInfo> {
    jamodio_vst3_host::scan_file(std::path::Path::new(item))
}

/// macOS : un item = un composant AU (`au:type/subtype/manuf`). La probe
/// instancie le composant — Y COMPRIS les tiers : la mitigation v0.2.25
/// (« ne prober que les Apple natives ») n'a plus lieu d'être dans un process
/// jetable, on retrouve la vraie latence et le vrai has_input_bus.
#[cfg(target_os = "macos")]
fn scan_item(item: &str) -> Vec<PluginInfo> {
    let Some(au) = super::protocol::AuItem::decode(item) else {
        tracing::warn!(target: "jamodio::scan-worker", item, "item AU invalide — ignoré");
        return Vec::new();
    };
    jamodio_au_host::scan_component(&au.au_type, &au.subtype, &au.manufacturer)
        .into_iter()
        .collect()
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn scan_item(_item: &str) -> Vec<PluginInfo> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamodio_audio_core::plugin_host::{PluginInfo, PluginRef};

    fn fake_info(name: &str) -> PluginInfo {
        PluginInfo {
            name: name.into(),
            manufacturer: "Test".into(),
            plugin_ref: PluginRef::Vst3 { path: format!("/x/{name}.vst3"), uid: "00".into() },
            latency_samples: 0,
            has_editor: false,
            incompatible: false,
            has_input_bus: true,
            is_instrument: false,
        }
    }

    /// Décode les lignes NDJSON produites par run_loop.
    fn events(out: &[u8]) -> Vec<WorkerEvent> {
        String::from_utf8(out.to_vec())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn run_loop_emits_begin_plugins_end_per_item() {
        let input = "/a.vst3\n/b.vst3\n";
        let mut out = Vec::new();
        let code = run_loop(input.as_bytes(), &mut out, |item| {
            if item == "/a.vst3" {
                vec![fake_info("A1"), fake_info("A2")]
            } else {
                Vec::new() // .vst3 sans classe audio → begin/end sans plugin
            }
        });
        assert_eq!(code, 0);
        let ev = events(&out);
        assert_eq!(ev.len(), 6);
        assert_eq!(ev[0], WorkerEvent::Begin { item: "/a.vst3".into() });
        assert!(matches!(&ev[1], WorkerEvent::Plugin { info } if info.name == "A1"));
        assert!(matches!(&ev[2], WorkerEvent::Plugin { info } if info.name == "A2"));
        assert_eq!(ev[3], WorkerEvent::End { item: "/a.vst3".into() });
        assert_eq!(ev[4], WorkerEvent::Begin { item: "/b.vst3".into() });
        assert_eq!(ev[5], WorkerEvent::End { item: "/b.vst3".into() });
    }

    #[test]
    fn run_loop_skips_blank_lines_and_trims() {
        let input = "\n  /a.vst3  \n\n";
        let mut out = Vec::new();
        let code = run_loop(input.as_bytes(), &mut out, |_| Vec::new());
        assert_eq!(code, 0);
        let ev = events(&out);
        assert_eq!(
            ev,
            vec![
                WorkerEvent::Begin { item: "/a.vst3".into() },
                WorkerEvent::End { item: "/a.vst3".into() },
            ]
        );
    }

    #[test]
    fn run_loop_stops_with_code_1_when_stdout_closed() {
        /// Writer qui échoue dès la première écriture (coordinateur mort).
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _b: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let code = run_loop("/a.vst3\n".as_bytes(), Broken, |_| Vec::new());
        assert_eq!(code, 1);
    }
}

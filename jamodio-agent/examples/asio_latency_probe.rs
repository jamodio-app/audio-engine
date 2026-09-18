//! Sonde ASIO — latences DÉCLARÉES par chaque pilote installé.
//!
//! Lot 0 du chantier « infobulle latence et qualité du lien » (dépôt web :
//! `internal-docs/plans/PLAN-INFOBULLE-LATENCE-2026-09.md`). Aujourd'hui l'agent
//! compte la latence matérielle comme « taille du buffer + 2 ms » ; cette sonde
//! dit ce que chaque pilote déclare vraiment, avant qu'on s'appuie dessus.
//!
//! Pour chaque pilote ASIO, elle affiche :
//!   - les canaux, la fréquence courante, et si 48 kHz est accepté ;
//!   - la grille de buffer du pilote (`ASIOGetBufferSize` : min / max / préférée /
//!     granularité) ;
//!   - `ASIOGetLatencies` (entrée / sortie, en échantillons et en ms) AVANT puis
//!     APRÈS la création des buffers — à la taille préférée, puis à 64 si le pilote
//!     l'accepte (taille demandée par l'agent, cf. `audio::buffer_policy::LOW`) ;
//!   - la part « au-delà du buffer » : pilote + convertisseurs + transport, que
//!     l'agent ne compte pas aujourd'hui.
//!
//! Elle ne DÉMARRE aucun flux (pas d'`ASIOStart`) : aucun son ne sort. Si le pilote
//! n'est pas en 48 kHz, elle l'y règle le temps de la mesure (comme l'agent) puis
//! remet sa fréquence d'origine.
//!
//! Sortie en ASCII sans accents : lisible dans n'importe quelle console Windows.
//!
//! Usage (PC Windows, console « x64 Native Tools Command Prompt for VS ») : quitter
//! d'abord l'agent Jamodio et tout logiciel audio — beaucoup de pilotes ASIO
//! n'acceptent qu'un seul client à la fois — puis copier la sortie complète (les
//! lignes `CSV;` suffisent pour le tableau du protocole de banc).
//!
//! ```text
//! cargo run --release -p jamodio-agent --example asio_latency_probe
//! cargo run --release -p jamodio-agent --example asio_latency_probe -- "Focusrite USB ASIO"
//! ```
//!
//! La seconde forme ne sonde qu'un pilote (nom exact, tel qu'affiché dans la liste).

#[cfg(not(windows))]
fn main() {
    eprintln!("asio_latency_probe : sonde Windows uniquement (ASIO).");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    probe::run();
}

#[cfg(windows)]
mod probe {
    use asio_sys as sys;
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
    };

    // Fonctions du SDK ASIO compilées dans asio-sys mais non exposées par le crate
    // (même technique que `audio::asio_host` pour `ASIOGetBufferSize`). Noms décorés
    // MSVC x64 de `long f(long*, …)`.
    extern "C" {
        #[link_name = "?ASIOGetBufferSize@@YAJPEAJ000@Z"]
        fn ASIOGetBufferSize(min: *mut i32, max: *mut i32, pref: *mut i32, gran: *mut i32) -> i32;
        #[link_name = "?ASIOGetLatencies@@YAJPEAJ0@Z"]
        fn ASIOGetLatencies(input: *mut i32, output: *mut i32) -> i32;
    }

    /// `ASE_OK` du SDK ASIO.
    const ASE_OK: i32 = 0;
    /// Fréquence imposée par Jamodio (48 kHz natif, aucun rééchantillonnage).
    const TARGET_RATE: f64 = 48_000.0;
    /// Taille demandée par l'agent quand le pilote l'accepte (`audio::buffer_policy::LOW`).
    const AGENT_LOW: i32 = 64;

    struct BufferSizes {
        min: i32,
        max: i32,
        pref: i32,
        gran: i32,
    }

    fn buffer_sizes() -> Result<BufferSizes, i32> {
        let (mut min, mut max, mut pref, mut gran) = (0, 0, 0, 0);
        // SAFETY : appelé seulement avec un pilote chargé et initialisé (`ASIOInit`
        // fait par `load_driver`) ; les quatre pointeurs visent des locales vivantes.
        let rc = unsafe { ASIOGetBufferSize(&mut min, &mut max, &mut pref, &mut gran) };
        if rc == ASE_OK {
            Ok(BufferSizes {
                min,
                max,
                pref,
                gran,
            })
        } else {
            Err(rc)
        }
    }

    fn latencies() -> Result<(i32, i32), i32> {
        let (mut input, mut output) = (0, 0);
        // SAFETY : mêmes garanties que `buffer_sizes`.
        let rc = unsafe { ASIOGetLatencies(&mut input, &mut output) };
        if rc == ASE_OK {
            Ok((input, output))
        } else {
            Err(rc)
        }
    }

    /// `size` appartient-il à la grille légale du pilote ? On ne « rapproche » jamais
    /// une taille : on ne mesure qu'une taille que le pilote accepte telle quelle.
    fn is_legal(size: i32, b: &BufferSizes) -> bool {
        if size < b.min || size > b.max {
            return false;
        }
        match b.gran {
            // Granularité -1 : puissances de 2 uniquement.
            -1 => size.count_ones() == 1,
            g if g > 0 => (size - b.min) % g == 0,
            // Granularité 0 : taille unique (min == max).
            _ => size == b.min,
        }
    }

    fn ms(samples: i32, rate: f64) -> String {
        format!("{:.2} ms", f64::from(samples) * 1000.0 / rate)
    }

    /// Part déclarée au-delà du buffer (pilote + convertisseurs + transport).
    fn beyond_buffer(latency: i32, buffer: i32, rate: f64) -> String {
        match latency - buffer {
            d if d > 0 => format!("{d} ech ({})", ms(d, rate)),
            0 => "0 (le pilote ne declare que son buffer : a verifier au banc)".into(),
            _ => "negatif (declare < buffer : valeur suspecte)".into(),
        }
    }

    pub fn run() {
        // SAFETY : thread principal d'un processus neuf, sans COM initialisé ; appel
        // équilibré par `CoUninitialize` en fin de `run`.
        unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };

        let filter = std::env::args().nth(1);
        let asio = sys::Asio::new();
        let names = asio.driver_names();

        println!("=== Sonde ASIO : latences declarees par les pilotes ===");
        println!("Pilotes installes : {}", names.len());
        for name in &names {
            println!("  - {name}");
        }

        let selected: Vec<&String> = names
            .iter()
            .filter(|n| filter.as_deref().is_none_or(|f| n.as_str() == f))
            .collect();
        if selected.is_empty() {
            match &filter {
                Some(f) => println!("\nAucun pilote nomme exactement \"{f}\"."),
                None => println!("\nAucun pilote ASIO installe."),
            }
        }
        for name in selected {
            probe_driver(&asio, name);
        }

        // SAFETY : même thread que `CoInitializeEx` ; tous les pilotes (objets COM)
        // ont été détruits dans `probe_driver`.
        unsafe { CoUninitialize() };
    }

    fn probe_driver(asio: &sys::Asio, name: &str) {
        println!("\n----------------------------------------");
        println!("PILOTE : {name}");
        let driver = match asio.load_driver(name) {
            Ok(driver) => driver,
            Err(e) => {
                println!("  ERREUR chargement : {e:?}");
                println!("  (pilote utilise par un autre logiciel, ou interface debranchee ?)");
                return;
            }
        };
        report(&driver);
        // ASIO ne charge qu'un pilote à la fois : il faut libérer celui-ci avant le suivant.
        match driver.destroy() {
            Ok(true) => {}
            Ok(false) => println!("  ATTENTION : pilote non libere (une autre reference existe)"),
            Err(e) => println!("  ATTENTION : liberation du pilote en erreur : {e:?}"),
        }
    }

    fn report(driver: &sys::Driver) {
        let channels = match driver.channels() {
            Ok(channels) => channels,
            Err(e) => {
                println!("  ERREUR ASIOGetChannels : {e:?}");
                return;
            }
        };
        println!(
            "  Canaux : {} entrees, {} sorties",
            channels.ins, channels.outs
        );

        let original_rate = driver.sample_rate().ok();
        match original_rate {
            Some(r) => println!("  Frequence actuelle : {r:.0} Hz"),
            None => println!("  Frequence actuelle : illisible"),
        }
        let can_48k = driver.can_sample_rate(TARGET_RATE);
        match &can_48k {
            Ok(true) => println!("  48 kHz accepte : oui"),
            Ok(false) => println!("  48 kHz accepte : NON (Jamodio refusera ce pilote)"),
            Err(e) => println!("  48 kHz accepte : erreur {e:?}"),
        }

        let mut rate = original_rate.unwrap_or(TARGET_RATE);
        if matches!(can_48k, Ok(true)) && (rate - TARGET_RATE).abs() > 1.0 {
            match driver.set_sample_rate(TARGET_RATE) {
                Ok(()) => {
                    rate = driver.sample_rate().unwrap_or(rate);
                    println!("  Frequence reglee a {rate:.0} Hz pour la mesure (comme l'agent)");
                }
                Err(e) => println!(
                    "  ATTENTION : passage en 48 kHz refuse ({e:?}), mesure a {rate:.0} Hz"
                ),
            }
        }

        measure(driver, &channels, rate, driver.name());
        restore_rate(driver, original_rate, rate);
    }

    fn measure(driver: &sys::Driver, channels: &sys::Channels, rate: f64, name: &str) {
        let sizes = match buffer_sizes() {
            Ok(sizes) => sizes,
            Err(rc) => {
                println!("  ERREUR ASIOGetBufferSize (code {rc})");
                return;
            }
        };
        println!(
            "  Grille de buffer : min {} / max {} / prefere {} / granularite {}",
            sizes.min, sizes.max, sizes.pref, sizes.gran
        );

        match latencies() {
            Ok((input, output)) => println!(
                "  ASIOGetLatencies AVANT creation des buffers : entree {input} ech ({}), sortie {output} ech ({})",
                ms(input, rate),
                ms(output, rate)
            ),
            Err(rc) => println!("  ASIOGetLatencies AVANT creation des buffers : code {rc}"),
        }

        // Comme l'agent : toutes les entrées, la première paire de sortie.
        let n_in = channels.ins.max(0) as usize;
        let n_out = (channels.outs.max(0) as usize).min(2);
        if n_in == 0 || n_out == 0 {
            println!(
                "  Pas d'entree ou pas de sortie : mesure apres creation des buffers impossible"
            );
            return;
        }

        with_buffers(driver, n_in, n_out, sizes.pref, rate, name, "prefere");
        if AGENT_LOW != sizes.pref {
            if is_legal(AGENT_LOW, &sizes) {
                with_buffers(driver, n_in, n_out, AGENT_LOW, rate, name, "agent-64");
            } else {
                println!("  64 echantillons : hors grille du pilote, non mesure");
            }
        }
    }

    fn with_buffers(
        driver: &sys::Driver,
        n_in: usize,
        n_out: usize,
        size: i32,
        rate: f64,
        name: &str,
        label: &str,
    ) {
        let streams = driver
            .prepare_input_stream(None, n_in, Some(size))
            .and_then(|s| driver.prepare_output_stream(s.input, n_out, Some(size)));
        let streams = match streams {
            Ok(streams) => streams,
            Err(e) => {
                println!("  [{label}] ERREUR ASIOCreateBuffers({size}) : {e:?}");
                return;
            }
        };
        let created = streams
            .input
            .as_ref()
            .or(streams.output.as_ref())
            .map(|s| s.buffer_size)
            .unwrap_or(size);

        match latencies() {
            Ok((input, output)) => {
                println!(
                    "  [{label}] buffer cree : {created} ech ({})",
                    ms(created, rate)
                );
                println!(
                    "    entree : {input} ech ({}) ; au-dela du buffer : {}",
                    ms(input, rate),
                    beyond_buffer(input, created, rate)
                );
                println!(
                    "    sortie : {output} ech ({}) ; au-dela du buffer : {}",
                    ms(output, rate),
                    beyond_buffer(output, created, rate)
                );
                println!("CSV;{name};{rate:.0};{label};{created};{input};{output}");
            }
            Err(rc) => {
                println!("  [{label}] ASIOGetLatencies apres creation des buffers : code {rc}")
            }
        }

        if let Err(e) = driver.dispose_buffers() {
            println!("  [{label}] ATTENTION : liberation des buffers en erreur : {e:?}");
        }
        drop(streams);
    }

    fn restore_rate(driver: &sys::Driver, original: Option<f64>, current: f64) {
        let Some(original) = original else { return };
        if (original - current).abs() <= 1.0 {
            return;
        }
        match driver.set_sample_rate(original) {
            Ok(()) => println!("  Frequence d'origine remise : {original:.0} Hz"),
            Err(e) => println!("  ATTENTION : impossible de remettre {original:.0} Hz ({e:?})"),
        }
    }
}

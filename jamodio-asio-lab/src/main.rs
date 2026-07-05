//! Banc de diagnostic ASIO full-duplex (Windows) — JETABLE.
//!
//! But : reproduire le gel ~1 s du Focusrite HORS de l'agent Tauri, dans un
//! binaire minimal (asio-sys direct) qu'on peut lancer/attacher à un debugger
//! en quelques secondes, et comparer plusieurs hypothèses en changeant un seul
//! paramètre à la fois (le « mode », 1er argument CLI).
//!
//! On ouvre le driver EXACTEMENT comme le vrai host duplex cible :
//!   1 `ASIOCreateBuffers` couvrant entrée+sortie (chaînage
//!   prepare_input_stream → prepare_output_stream), 1 `bufferSwitch`.
//!
//! Le marshalling de buffer réutilise le pattern ÉPROUVÉ de cpal (MIT) :
//! accès FRAIS `Arc<Mutex<AsioStreams>>` dans le callback + `from_raw_parts_mut`
//! (helpers `asio_channel_slice`/`_mut` copiés de cpal 0.15.3, licence MIT).
//!
//! Modes (argv[1], défaut `baseline`) :
//!   baseline : 1 callback, remplit la sortie de silence ; le thread principal DORT.
//!              → reproduit le probe (attendu : gel ~1 s).
//!   readin   : idem + LIT l'entrée à chaque tick (teste « le driver exige-t-il
//!              qu'on consomme l'entrée ? »).
//!   pump     : 1 callback (remplit sortie) ; le thread principal POMPE les
//!              messages Win32 au lieu de dormir (teste l'hypothèse STA/pump).
//!   nofill   : 1 callback qui ne fait que COMPTER (ne touche aucun buffer).
//!   nomsg    : baseline SANS message callback (teste si le message cb nuit).
//!
//! Sortie : par seconde, le delta de tics + le total + le thread-id du callback
//! (vs le thread-id principal). Verdict : dernière seconde où les tics ont
//! progressé (durée de survie).

#[cfg(not(windows))]
fn main() {
    eprintln!("jamodio-asio-lab : Windows uniquement (no-op sur cette plateforme).");
}

#[cfg(windows)]
fn main() {
    win::run();
}

#[cfg(windows)]
mod win {
    use asio_sys as sys;
    use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    pub fn run() {
        // Durée de surveillance : argv[2] en secondes (défaut 12).
        let monitor_secs: u64 = std::env::args()
            .nth(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(12);
        // STA sur le thread principal (asio-sys charge le driver via CoCreateInstance
        // sans initialiser COM lui-même — mirror du contrat com_exec de l'agent).
        unsafe {
            let _ = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        }
        let main_tid = unsafe { GetCurrentThreadId() };

        let mode = std::env::args().nth(1).unwrap_or_else(|| "baseline".into());

        // Mode `cpal2` : reproduit le VRAI chemin de production de l'agent —
        // DEUX cpal::Stream séparés (capture + playback = 2 ASIOStart) sur le
        // driver mono-client, comme capture.rs/playback.rs. C'est le chemin qui
        // wedge (~20 s) et que le mono-duplex doit remplacer.
        if mode == "cpal2" {
            run_cpal2(monitor_secs, main_tid);
            return;
        }
        // Mode `latency` : pour chaque taille de buffer candidate, crée les buffers
        // et lit ASIOGetLatencies (latence in+out RÉELLE rapportée par le driver,
        // pipeline USB/hardware inclus — pas juste le buffer). Révèle s'il y a des
        // ms à gagner sous la taille préférée (128) → arbitrage du Niveau 2.
        if mode == "latency" {
            run_latency();
            return;
        }
        // Mode `bufinfo` : dump BRUT de `ASIOGetBufferSize(min,max,pref,granularity)`
        // + la GRILLE de tailles légales que le driver expose réellement, + les
        // latences ASIOGetLatencies. C'est LA donnée décisive du bug cold-start :
        // `asio-sys::create_buffers` ne valide QUE `demandé <= max` (ni min, ni
        // granularité) → il passe un 64 potentiellement ILLÉGAL tel quel à
        // ASIOCreateBuffers. Jamulus/JUCE, eux, refusent une taille hors grille et
        // retombent sur `pref`. Ce mode répond donc : « 64 est-il une taille ASIO
        // LÉGALE sur cette interface, ou est-ce qu'on force une taille interdite ? ».
        if mode == "bufinfo" {
            run_bufinfo();
            return;
        }
        // Mode `robust` : PROTOTYPE du host single-owner robuste, toutes interfaces
        // (invariants Jamulus + JUCE). Séquence : 1 seule instance Asio → load_driver
        // (1 ASIOInit) → set_sample_rate → snap de la taille à la grille LÉGALE du
        // driver → PRIMING JUCE (create dummy → start → sleep 120ms → stop → dispose,
        // « some devices fail if we don't ») → 1 seul ASIOCreateBuffers(in+out) →
        // callback duplex → 1 seul ASIOStart → start-timeout (attend le 1er callback).
        // Mesure ensuite la fraîcheur de l'entrée (doit être VIVANTE dès le départ, sur
        // TOUTE interface). C'est le code destiné à remplacer les 2 streams cpal dans
        // l'agent. `JAMLAB_BUF` = taille désirée (défaut 64) ; `JAMLAB_NO_PRIME=1`
        // désactive le priming (pour A/B l'effet du priming).
        if mode == "robust" {
            run_robust(monitor_secs.max(1), main_tid);
            return;
        }
        // Mode `churn` : ouvre/ferme le driver en boucle rapide (load→create→start
        // →hold→stop→dispose→destroy) — reproduit le cycle ASIOExit/ASIOInit que
        // l'agent faisait à chaque leave/rejoin (cause racine présumée 0.5.4-5,
        // tueur classique des drivers USB ASIO). argv[2] = nb de cycles (défaut 12),
        // JAMLAB_HOLD_MS = durée chaude par cycle (défaut 400 ms).
        if mode == "churn" {
            run_churn(monitor_secs.max(1));
            return;
        }
        // Mode `coldstart` : reproduit le TOUT PREMIER init ASIO à froid EXACTEMENT
        // comme le fait cpal dans l'agent, et instrumente la FRAÎCHEUR de l'entrée
        // (channel 0 = tranche instrument/guitare Focusrite). C'est le banc dédié
        // au bug 2026-07-03 : entrée FIGÉE sur une constante quasi pleine-échelle
        // (~0.99) au 1er démarrage à froid, nettoyée seulement par un ASIOInit frais.
        //
        // Paramétré par variables d'environnement pour comparer un seul facteur à
        // la fois (chaque run = un ASIOInit/ASIOExit frais) :
        //   JAMLAB_BUF   = 64 (défaut) | 128 | 0  (0 ⇒ None ⇒ taille préférée du driver)
        //   JAMLAB_ORDER = agent (défaut) | out-first | single
        //     - agent     : Create(in)·Start·[Stop·Dispose·Create(in+out)]·Start
        //                   — MIROIR EXACT de cpal (build_input puis build_output,
        //                     chacun appelant ASIOStart ; le 2e create stop+dispose).
        //     - out-first : Create(out)·Start·[Stop·Dispose·Create(out+in)]·Start
        //                   — l'entrée n'est JAMAIS démarrée seule puis détruite ;
        //                     son DMA n'existe que dans la config duplex finale.
        //     - single    : Create(in+out)·Start — un seul create, aucun start/stop
        //                   intermédiaire (ce que ferait un hôte single-owner).
        // Verdict : l'entrée est-elle VIVANTE (varie avec les tics) ou FIGÉE/railée
        // (constante bit-exacte re-servie) sur les N premières secondes ?
        if mode == "coldstart" {
            run_coldstart(monitor_secs.max(1), main_tid);
            return;
        }
        // Mode `dualasio` : reproduit LE pattern de l'agent — l'entrée est ouverte via
        // une 1re instance `sys::Asio` (#A) et la sortie via une 2e instance DISTINCTE
        // (#B). Comme un `Asio` neuf a un `Weak` vide, la #B refait un `ASIOInit` COMPLET
        // sur le driver mono-client ALORS QUE l'entrée tourne déjà dessus (+ un
        // `ASIOCreateBuffers` sortie qui peut disposer les buffers d'entrée). Le banc et
        // cpal2, eux, n'utilisent qu'UNE instance. On mesure la fraîcheur de l'entrée
        // AVANT puis APRÈS l'ouverture de la sortie : si elle fige après #B, le mécanisme
        // (2 instances Asio = 2 ASIOInit) est prouvé — potentiellement même à chaud.
        if mode == "dualasio" {
            run_dualasio(monitor_secs.max(1), main_tid);
            return;
        }

        let read_input = mode == "readin";
        let touch_output = mode != "nofill";
        let pump = mode == "pump";
        let with_msg_cb = mode != "nomsg";
        // Mode `stall` : le callback DÉPASSE volontairement l'échéance ASIO
        // (~2,7 ms @128/48k) en spinnant `JAMLAB_STALL_MS` toutes les
        // `JAMLAB_STALL_EVERY` tics — simule un `mixer.lock()` contendu ou un
        // callback trop lourd. Teste si un overrun fait halter le driver USB.
        let stall = mode == "stall";
        let stall_ms: u64 = std::env::var("JAMLAB_STALL_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(6);
        let stall_every: u64 = std::env::var("JAMLAB_STALL_EVERY").ok().and_then(|s| s.parse().ok()).unwrap_or(375);
        if stall {
            println!("[stall] spin {stall_ms}ms toutes les {stall_every} tics (échéance ~2.7ms) ");
        }
        println!(
            "=== jamodio-asio-lab === mode={mode} main_tid={main_tid} \
             (read_input={read_input} touch_output={touch_output} pump={pump} msg_cb={with_msg_cb})"
        );

        // Mode `coexist*` : AVANT d'ouvrir notre duplex, on charge le driver via
        // CPAL exactement comme `host::probe()` au boot de l'agent (énumération
        // ASIO). cpal et nous partageons le MÊME asio-sys (statics globaux) →
        // ça reproduit la coexistence cpal↔asio-sys sur l'état ASIO global.
        //   coexist      : on DROP le host cpal après énumération (fidèle au boot,
        //                  où host::probe() droppe son host local).
        //   coexist-live : on GARDE le host cpal vivant pendant tout le duplex.
        let _cpal_guard = if mode.starts_with("coexist") {
            cpal_enumerate(mode == "coexist-live")
        } else {
            None
        };

        let asio = sys::Asio::new();
        let names = asio.driver_names();
        println!("drivers ASIO présents : {names:?}");

        // 1er driver qui SE CHARGE et expose ≥ 1 entrée (= interface branchée).
        let mut chosen: Option<(sys::Driver, String)> = None;
        for name in names {
            match asio.load_driver(&name) {
                Ok(d) => {
                    let ins = d.channels().map(|c| c.ins).unwrap_or(0);
                    if ins > 0 {
                        println!("driver retenu : {name} (ins={ins})");
                        chosen = Some((d, name));
                        break;
                    }
                    println!("  {name} : chargé mais 0 entrée — ignoré");
                    let _ = d.destroy();
                }
                Err(e) => println!("  {name} : non chargeable ({e:?}) — ignoré"),
            }
        }
        let Some((driver, name)) = chosen else {
            eprintln!("aucun driver ASIO chargeable avec entrée — carte branchée ?");
            return;
        };

        let _ = driver.set_sample_rate(48_000.0);
        let ch = driver.channels().ok();
        let ins = ch.as_ref().map(|c| c.ins).unwrap_or(-1);
        let outs = ch.as_ref().map(|c| c.outs).unwrap_or(-1);
        let sr = driver.sample_rate().unwrap_or(0.0);
        let in_ty = driver.input_data_type().ok();
        let out_ty = driver.output_data_type().ok();
        println!(
            "driver={name} ins={ins} outs={outs} sr={sr} in_ty={in_ty:?} out_ty={out_ty:?}"
        );

        let n_in = (ins.max(0) as usize).clamp(1, 2);
        let n_out = (outs.max(0) as usize).clamp(1, 2);

        // UN SEUL ASIOCreateBuffers couvrant entrée PUIS sortie.
        let streams = match driver.prepare_input_stream(None, n_in, None) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("prepare_input_stream a échoué : {e:?}");
                let _ = driver.destroy();
                return;
            }
        };
        let streams = match driver.prepare_output_stream(streams.input, n_out, None) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("prepare_output_stream a échoué : {e:?}");
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
        println!("ASIOCreateBuffers duplex OK : buffer_size={buffer_size} in={n_in} out={n_out} duplex_ok={duplex_ok}");
        let expected_ticks_per_s = if buffer_size > 0 {
            48_000.0 / buffer_size as f64
        } else {
            0.0
        };
        println!("→ ~{expected_ticks_per_s:.0} bufferSwitch/s attendus (48k / buffer_size)");

        let out_is_int32 = matches!(out_ty, Some(sys::AsioSampleType::ASIOSTInt32LSB));
        let in_is_int32 = matches!(in_ty, Some(sys::AsioSampleType::ASIOSTInt32LSB));

        let streams = Arc::new(Mutex::new(streams));
        let streams_cb = streams.clone();

        let ticks = Arc::new(AtomicU64::new(0));
        let cb_ticks = ticks.clone();
        // Thread-id du callback (0 = pas encore vu) : révèle si les callbacks
        // arrivent sur le thread STA principal ou sur un thread propre au driver.
        let cb_tid = Arc::new(AtomicU32::new(0));
        let cb_tid_w = cb_tid.clone();
        // Somme de contrôle des lectures d'entrée (empêche l'optimiseur de virer la lecture).
        let in_sum = Arc::new(AtomicU64::new(0));
        let in_sum_w = in_sum.clone();
        // Max observé de buffer_index (doit rester 0/1).
        let max_idx = Arc::new(AtomicUsize::new(0));
        let max_idx_w = max_idx.clone();

        let cb = driver.add_callback(move |info| {
            let tid = unsafe { GetCurrentThreadId() };
            cb_tid_w.store(tid, Ordering::Relaxed);
            let n = cb_ticks.fetch_add(1, Ordering::Relaxed) + 1;

            // Overrun volontaire : spin > échéance ASIO (mode `stall`).
            if stall && n.is_multiple_of(stall_every) {
                let end = std::time::Instant::now() + Duration::from_millis(stall_ms);
                while std::time::Instant::now() < end {
                    std::hint::spin_loop();
                }
            }

            let idx = info.buffer_index as usize;
            let prev = max_idx_w.load(Ordering::Relaxed);
            if idx > prev {
                max_idx_w.store(idx, Ordering::Relaxed);
            }
            if idx > 1 {
                return; // double-tampon ASIO : 0 ou 1 attendu
            }

            let Ok(mut guard) = streams_cb.lock() else {
                return;
            };

            // Lecture de l'entrée (mode readin) — mirror asio_channel_slice de cpal.
            if read_input && in_is_int32 {
                if let Some(inp) = guard.input.as_ref() {
                    let bufsz = inp.buffer_size as usize;
                    let mut acc: u64 = 0;
                    for ci in 0..inp.buffer_infos.len() {
                        let ptr = inp.buffer_infos[ci].buffers[idx] as *const i32;
                        if ptr.is_null() {
                            continue;
                        }
                        let slice = unsafe { std::slice::from_raw_parts(ptr, bufsz) };
                        // touche chaque échantillon pour forcer la lecture réelle
                        for &s in slice {
                            acc = acc.wrapping_add(s as i64 as u64);
                        }
                    }
                    in_sum_w.fetch_add(acc, Ordering::Relaxed);
                }
            }

            // Écriture de la sortie (silence) — mirror asio_channel_slice_mut de cpal.
            if touch_output && out_is_int32 {
                if let Some(out) = guard.output.as_mut() {
                    let bufsz = out.buffer_size as usize;
                    for ci in 0..out.buffer_infos.len() {
                        let ptr = out.buffer_infos[ci].buffers[idx] as *mut i32;
                        if ptr.is_null() {
                            continue;
                        }
                        // SAFETY : buffers[idx] = `buffer_size` échantillons Int32 du
                        // tampon À REMPLIR (le driver joue l'autre). Mirror exact de
                        // `asio_channel_slice_mut` (cpal, MIT).
                        let slice = unsafe { std::slice::from_raw_parts_mut(ptr, bufsz) };
                        slice.fill(0);
                    }
                }
            }
        });

        let resets = Arc::new(AtomicU64::new(0));
        let msg = if with_msg_cb {
            let cb_resets = resets.clone();
            Some(driver.add_message_callback(move |sel| {
                let is_reset = matches!(sel, sys::AsioMessageSelectors::kAsioResetRequest);
                cb_resets.fetch_add(u64::from(is_reset), Ordering::Relaxed);
                // PAS de log lourd ici (pas de tracing/IO) — juste un compteur.
            }))
        } else {
            None
        };

        // Mode `enum-during` : ~1 s après ASIOStart, un thread de fond ré-énumère
        // les devices ASIO via CPAL (comme le boot de l'agent quand le browser
        // demande GetDevices) — ce qui RECHARGE le driver mono-client PENDANT que
        // notre duplex streame. Hypothèse : c'est ça qui gèle les callbacks à ~1 s.
        if mode == "enum-during" {
            std::thread::Builder::new()
                .name("cpal-enum".into())
                .spawn(|| {
                    use cpal::traits::HostTrait;
                    unsafe {
                        let _ = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
                    }
                    for i in 1..=5 {
                        std::thread::sleep(Duration::from_millis(1000));
                        match cpal::host_from_id(cpal::HostId::Asio) {
                            Ok(h) => {
                                let ni = h.input_devices().map(|it| it.count()).unwrap_or(0);
                                let no = h.output_devices().map(|it| it.count()).unwrap_or(0);
                                println!("[enum-during] #{i} cpal ASIO enumerate: in={ni} out={no}");
                            }
                            Err(e) => println!("[enum-during] #{i} err: {e}"),
                        }
                    }
                })
                .expect("spawn cpal-enum");
        }

        if let Err(e) = driver.start() {
            eprintln!("ASIOStart a échoué : {e:?}");
            driver.remove_callback(cb);
            if let Some(m) = msg {
                driver.remove_message_callback(m);
            }
            let _ = driver.dispose_buffers();
            let _ = driver.destroy();
            return;
        }
        println!("ASIOStart OK — surveillance {monitor_secs}s (mode={mode})\n");

        // Surveillance : delta de tics par seconde. Selon le mode, on DORT ou on
        // POMPE les messages Win32 sur ce thread (celui qui a fait ASIOStart).
        let mut prev = 0u64;
        let mut last_growth_s = 0u64;
        let mut first_tid_logged = false;
        for s in 1..=monitor_secs {
            wait_one_second(pump);
            let now = ticks.load(Ordering::Relaxed);
            let delta = now - prev;
            if delta > 0 {
                last_growth_s = s;
            }
            let tid = cb_tid.load(Ordering::Relaxed);
            if !first_tid_logged && tid != 0 {
                println!(
                    ">>> callback thread-id = {tid}  (main = {main_tid})  → {}",
                    if tid == main_tid {
                        "MÊME thread (STA) !"
                    } else {
                        "thread PROPRE au driver"
                    }
                );
                first_tid_logged = true;
            }
            println!(
                "t={s:2}s  tics/s={delta:4}  total={now:6}  resets={}  max_idx={}",
                resets.load(Ordering::Relaxed),
                max_idx.load(Ordering::Relaxed),
            );
            prev = now;
        }

        let total = ticks.load(Ordering::Relaxed);
        let survived = last_growth_s >= monitor_secs.saturating_sub(1);
        println!(
            "\n=== VERDICT mode={mode} : total={total} last_growth={last_growth_s}s survived={survived} \
             resets={} in_sum={} ===",
            resets.load(Ordering::Relaxed),
            in_sum.load(Ordering::Relaxed),
        );
        if !survived {
            println!(
                "→ GEL détecté : les callbacks ont cessé de progresser vers t={last_growth_s}s."
            );
        }

        let _ = driver.stop();
        driver.remove_callback(cb);
        if let Some(m) = msg {
            driver.remove_message_callback(m);
        }
        let _ = driver.dispose_buffers();
        let _ = driver.destroy();
    }

    /// Mode `churn` : cycle rapide open→hold→close du driver ASIO, N fois. Après
    /// chaque cycle on vérifie que les callbacks ont bien tourné pendant la phase
    /// chaude. Si à un cycle le driver refuse de (re)démarrer ou que les callbacks
    /// ne tournent plus → churn reproduit le wedge (cause racine 0.5.4-5).
    fn run_churn(cycles: u64) {
        use asio_sys as sys;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        let hold_ms: u64 = std::env::var("JAMLAB_HOLD_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(400);
        println!("[churn] {cycles} cycles, hold={hold_ms}ms/cycle\n");

        for c in 1..=cycles {
            let asio = sys::Asio::new();
            let names = asio.driver_names();
            // 1er driver chargeable avec entrée.
            let mut chosen = None;
            for name in names {
                if let Ok(d) = asio.load_driver(&name) {
                    if d.channels().map(|c| c.ins).unwrap_or(0) > 0 {
                        chosen = Some(d);
                        break;
                    }
                    let _ = d.destroy();
                }
            }
            let Some(driver) = chosen else {
                println!("cycle {c:2} : ÉCHEC load_driver (driver injoignable !) — WEDGE probable");
                return;
            };
            let _ = driver.set_sample_rate(48_000.0);

            let streams = match driver
                .prepare_input_stream(None, 2, None)
                .and_then(|s| driver.prepare_output_stream(s.input, 2, None))
            {
                Ok(s) => s,
                Err(e) => {
                    println!("cycle {c:2} : ÉCHEC create_buffers ({e:?}) — WEDGE reproduit");
                    let _ = driver.destroy();
                    return;
                }
            };
            let _ = streams; // buffers vivants le temps du cycle

            let ticks = Arc::new(AtomicU64::new(0));
            let cbt = ticks.clone();
            let cb = driver.add_callback(move |_| {
                cbt.fetch_add(1, Ordering::Relaxed);
            });

            let start_ok = driver.start().is_ok();
            if !start_ok {
                println!("cycle {c:2} : ÉCHEC ASIOStart — WEDGE reproduit");
                driver.remove_callback(cb);
                let _ = driver.dispose_buffers();
                let _ = driver.destroy();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(hold_ms));
            let t = ticks.load(Ordering::Relaxed);

            let _ = driver.stop();
            driver.remove_callback(cb);
            let _ = driver.dispose_buffers();
            let destroyed = driver.destroy();

            let expected = hold_ms as f64 / 1000.0 * 375.0;
            let alive = t as f64 > expected * 0.5;
            println!(
                "cycle {c:2} : start=OK  tics={t:4} (attendu ~{expected:.0})  {}  destroy={destroyed:?}",
                if alive { "vivant" } else { "!! MUET (callbacks gelés) !!" }
            );
            if !alive {
                println!("→ WEDGE reproduit au cycle {c} : le driver démarre mais ne délivre plus de callbacks.");
                return;
            }
        }
        println!("\n=== VERDICT churn : {cycles} cycles OK, aucun wedge (driver robuste au churn en isolation) ===");
    }

    /// Mode `coldstart` : reproduit le 1er init ASIO à froid comme cpal/l'agent et
    /// mesure la FRAÎCHEUR de l'entrée (channel 0). Cf. le gros commentaire de
    /// dispatch pour les variables d'env (JAMLAB_BUF / JAMLAB_ORDER).
    fn run_coldstart(monitor_secs: u64, main_tid: u32) {
        use asio_sys as sys;
        use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        let buf_env: i32 = std::env::var("JAMLAB_BUF").ok().and_then(|s| s.parse().ok()).unwrap_or(64);
        let buf_req: Option<i32> = if buf_env <= 0 { None } else { Some(buf_env) };
        let order = std::env::var("JAMLAB_ORDER").unwrap_or_else(|_| "agent".into());
        println!(
            "=== coldstart === buf_req={:?} order={order} main_tid={main_tid} monitor={monitor_secs}s",
            buf_req
        );
        println!("(mesure : l'entrée ch0 est-elle VIVANTE ou FIGÉE/railée sur les 1res secondes)\n");

        // --- Charge le 1er driver ASIO avec entrée (= ASIOInit frais) ---
        let asio = sys::Asio::new();
        let mut chosen: Option<(sys::Driver, String)> = None;
        for name in asio.driver_names() {
            if let Ok(d) = asio.load_driver(&name) {
                if d.channels().map(|c| c.ins).unwrap_or(0) > 0 {
                    chosen = Some((d, name));
                    break;
                }
                let _ = d.destroy();
            }
        }
        let Some((driver, name)) = chosen else {
            eprintln!("aucun driver ASIO chargeable avec entrée — carte branchée / libre ?");
            return;
        };
        // Test d'hypothèse : `set_sample_rate` est-il ce qui ARME l'ADC d'entrée à
        // froid ? Le lab le fait toujours (→ entrée vivante) ; cpal le SKIP si le SR
        // est déjà bon (→ entrée figée dans l'agent). `JAMLAB_NO_SR=1` reproduit le
        // comportement de cpal (skip) pour voir si ça reproduit le wedge au banc.
        let skip_sr = std::env::var("JAMLAB_NO_SR").is_ok();
        if skip_sr {
            let cur = driver.sample_rate().unwrap_or(0.0);
            println!("set_sample_rate: SKIP (JAMLAB_NO_SR) — SR courant du driver = {cur}");
        } else {
            let _ = driver.set_sample_rate(48_000.0);
            println!("set_sample_rate: 48000 (appelé, comme le lab par défaut)");
        }
        let in_ty = driver.input_data_type().ok();
        let in_is_int32 = matches!(in_ty, Some(sys::AsioSampleType::ASIOSTInt32LSB));
        let ch = driver.channels().ok();
        let n_in = (ch.as_ref().map(|c| c.ins).unwrap_or(1).max(1) as usize).clamp(1, 2);
        let n_out = (ch.as_ref().map(|c| c.outs).unwrap_or(1).max(1) as usize).clamp(1, 2);
        println!("driver={name} in_ty={in_ty:?} in_int32={in_is_int32} n_in={n_in} n_out={n_out}");
        if !in_is_int32 {
            println!("⚠ entrée non-Int32 : instrumentation d'entrée désactivée (ce banc cible le Focusrite Int32)");
        }

        // --- Séquence d'ouverture selon `order` (reproduit cpal ou une alternative) ---
        // On enregistre le callback AVANT le 1er start pour capter les tout premiers
        // bufferSwitch (là où l'entrée railée apparaît selon la télémétrie).
        let ticks = Arc::new(AtomicU64::new(0));
        let cb_tid = Arc::new(AtomicU32::new(0));
        // Fraîcheur d'entrée ch0 :
        let in_s0 = Arc::new(AtomicI64::new(i64::MIN)); // 1er sample du dernier bloc
        let in_changes = Arc::new(AtomicU64::new(0)); // +1 quand s0 diffère du bloc précédent
        let in_absmax = Arc::new(AtomicI64::new(0)); // |sample| max observé
        let const_blocks = Arc::new(AtomicU64::new(0)); // blocs où TOUS les samples sont égaux
        let read_blocks = Arc::new(AtomicU64::new(0)); // blocs d'entrée effectivement lus
        let max_idx = Arc::new(AtomicUsize::new(0));

        // Les buffers ASIO (create_buffers) sont détenus par ce Mutex, accès frais
        // dans le callback (mirror cpal). On les remplace à chaque (re)création.
        let streams: Arc<Mutex<Option<sys::AsioStreams>>> = Arc::new(Mutex::new(None));

        let cb = {
            let ticks = ticks.clone();
            let cb_tid = cb_tid.clone();
            let in_s0 = in_s0.clone();
            let in_changes = in_changes.clone();
            let in_absmax = in_absmax.clone();
            let const_blocks = const_blocks.clone();
            let read_blocks = read_blocks.clone();
            let max_idx = max_idx.clone();
            let streams = streams.clone();
            driver.add_callback(move |info| {
                cb_tid.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
                ticks.fetch_add(1, Ordering::Relaxed);
                let idx = info.buffer_index as usize;
                if idx > max_idx.load(Ordering::Relaxed) {
                    max_idx.store(idx, Ordering::Relaxed);
                }
                if idx > 1 {
                    return;
                }
                if !in_is_int32 {
                    return;
                }
                let Ok(guard) = streams.lock() else { return };
                let Some(st) = guard.as_ref() else { return };
                let Some(inp) = st.input.as_ref() else { return };
                // Channel 0 uniquement (tranche instrument/guitare).
                let bufsz = inp.buffer_size as usize;
                let ptr = inp.buffer_infos[0].buffers[idx] as *const i32;
                if ptr.is_null() || bufsz == 0 {
                    return;
                }
                let slice = unsafe { std::slice::from_raw_parts(ptr, bufsz) };
                let s0 = slice[0];
                // Bloc constant ? (tous les samples égaux au 1er — signature d'un
                // buffer figé/DC railé).
                let is_const = slice.iter().all(|&s| s == s0);
                if is_const {
                    const_blocks.fetch_add(1, Ordering::Relaxed);
                }
                // |max| du bloc.
                let mut amax = 0i64;
                for &s in slice {
                    let a = (s as i64).abs();
                    if a > amax {
                        amax = a;
                    }
                }
                if amax > in_absmax.load(Ordering::Relaxed) {
                    in_absmax.store(amax, Ordering::Relaxed);
                }
                // Le 1er sample a-t-il changé depuis le bloc précédent ? (vivant vs figé)
                let prev = in_s0.swap(s0 as i64, Ordering::Relaxed);
                if prev != i64::MIN && prev != s0 as i64 {
                    in_changes.fetch_add(1, Ordering::Relaxed);
                }
                read_blocks.fetch_add(1, Ordering::Relaxed);
            })
        };

        // Compteur de resets (comme le vrai host, pour écarter un kAsioResetRequest).
        let resets = Arc::new(AtomicU64::new(0));
        let msg = {
            let r = resets.clone();
            driver.add_message_callback(move |sel| {
                let is_reset = matches!(sel, sys::AsioMessageSelectors::kAsioResetRequest);
                r.fetch_add(u64::from(is_reset), Ordering::Relaxed);
            })
        };

        // Applique une séquence de create/start et publie les streams créés dans le
        // Mutex. Renvoie false si une étape ASIO échoue (setup foireux → on abandonne).
        let publish = |s: sys::AsioStreams| {
            *streams.lock().unwrap() = Some(s);
        };
        let ok = match order.as_str() {
            "single" => {
                // Un seul ASIOCreateBuffers(in+out), un seul ASIOStart.
                match driver
                    .prepare_input_stream(None, n_in, buf_req)
                    .and_then(|s| driver.prepare_output_stream(s.input, n_out, buf_req))
                {
                    Ok(s) => {
                        publish(s);
                        driver.start().is_ok()
                    }
                    Err(e) => {
                        eprintln!("single: create_buffers échoué : {e:?}");
                        false
                    }
                }
            }
            "out-first" => {
                // Sortie créée+démarrée seule, PUIS entrée (recreate in+out) + start.
                // L'entrée n'est jamais démarrée seule puis détruite.
                match driver.prepare_output_stream(None, n_out, buf_req) {
                    Ok(s) => {
                        publish(s);
                        let started = driver.start().is_ok();
                        let out = streams.lock().unwrap().take().and_then(|s| s.output);
                        match driver.prepare_input_stream(out, n_in, buf_req) {
                            Ok(s2) => {
                                publish(s2);
                                started && driver.start().is_ok()
                            }
                            Err(e) => {
                                eprintln!("out-first: prepare_input échoué : {e:?}");
                                false
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("out-first: prepare_output échoué : {e:?}");
                        false
                    }
                }
            }
            _ => {
                // "agent" (défaut) : MIROIR EXACT de cpal.
                // build_input : Create(in) + Start.
                match driver.prepare_input_stream(None, n_in, buf_req) {
                    Ok(s) => {
                        publish(s);
                        let started_in = driver.start().is_ok();
                        // build_output : prepare_output ⇒ create_buffers voit Running
                        // ⇒ Stop + Dispose + Create(in+out), puis Start.
                        let input = streams.lock().unwrap().take().and_then(|s| s.input);
                        match driver.prepare_output_stream(input, n_out, buf_req) {
                            Ok(s2) => {
                                publish(s2);
                                started_in && driver.start().is_ok()
                            }
                            Err(e) => {
                                eprintln!("agent: prepare_output échoué : {e:?}");
                                false
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("agent: prepare_input échoué : {e:?}");
                        false
                    }
                }
            }
        };
        if !ok {
            driver.remove_callback(cb);
            driver.remove_message_callback(msg);
            let _ = driver.stop();
            let _ = driver.dispose_buffers();
            let _ = driver.destroy();
            return;
        }

        let actual_buf = streams
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|s| s.input.as_ref().or(s.output.as_ref()))
            .map(|s| s.buffer_size)
            .unwrap_or(-1);
        let expected_tps = if actual_buf > 0 { 48_000.0 / actual_buf as f64 } else { 0.0 };
        println!(
            "ouverture OK : buffer réel={actual_buf} (~{expected_tps:.0} tics/s attendus)\n"
        );

        // --- Surveillance : par seconde, la fraîcheur de l'entrée ---
        let full_scale = 2_147_483_648.0_f64;
        let mut prev_ticks = 0u64;
        let mut prev_changes = 0u64;
        let mut first_live_s: Option<u64> = None;
        let mut tid_logged = false;
        for s in 1..=monitor_secs {
            std::thread::sleep(Duration::from_secs(1));
            let t = ticks.load(Ordering::Relaxed);
            let c = in_changes.load(Ordering::Relaxed);
            let dt = t - prev_ticks;
            let dc = c - prev_changes;
            let amax = in_absmax.load(Ordering::Relaxed) as f64 / full_scale;
            let cb_blocks = read_blocks.load(Ordering::Relaxed).max(1);
            let const_ratio = const_blocks.load(Ordering::Relaxed) as f64 / cb_blocks as f64;
            // "Vivant" cette seconde = le 1er sample a changé sur ~la majorité des blocs.
            let live = dt > 0 && dc as f64 > dt as f64 * 0.25;
            if live && first_live_s.is_none() {
                first_live_s = Some(s);
            }
            if !tid_logged {
                let tid = cb_tid.load(Ordering::Relaxed);
                if tid != 0 {
                    println!(">>> callback tid={tid} (main={main_tid}) — {}", if tid == main_tid { "STA" } else { "thread driver" });
                    tid_logged = true;
                }
            }
            println!(
                "t={s:2}s tics/s={dt:4} chg/s={dc:4} |max|={amax:.4}({:.2}% FS) const_blk={:.0}% {}",
                amax * 100.0,
                const_ratio * 100.0,
                if live { "VIVANT" } else { "— figé/railé" }
            );
            prev_ticks = t;
            prev_changes = c;
        }

        let total_changes = in_changes.load(Ordering::Relaxed);
        let amax = in_absmax.load(Ordering::Relaxed) as f64 / full_scale;
        // ch0[0] ne varie quasiment jamais = DMA d'entrée figé (buffer re-servi).
        let frozen = total_changes < 4;
        let railed = frozen && amax > 0.90; // figé sur une valeur ~pleine-échelle
        let silent = frozen && amax < 0.01; // figé sur ~0 (buffer non alimenté / 0xFF)
        println!(
            "\n=== VERDICT coldstart buf={actual_buf} order={order} : \
             changes_totales={total_changes} |max|={amax:.4} first_live={:?} resets={} ===",
            first_live_s,
            resets.load(Ordering::Relaxed)
        );
        if railed {
            println!("→ ENTRÉE FIGÉE/RAILÉE reproduite : constante quasi pleine-échelle, ne varie pas (bug 2026-07-03).");
        } else if silent {
            println!(
                "→ ENTRÉE FIGÉE/SILENCE reproduite : buffer d'entrée jamais alimenté par l'ADC \
                 (≈0xFF, changes≈0) — MÊME wedge que l'agent (armement de l'entrée en échec, order={order})."
            );
        } else if first_live_s.map(|s| s <= 2).unwrap_or(false) {
            println!("→ Entrée VIVANTE dès le départ : pas de gel sur cette config (order={order}).");
        } else {
            println!("→ Entrée devenue vivante tardivement (warm-up ?) — cf. first_live.");
        }

        let _ = driver.stop();
        driver.remove_callback(cb);
        driver.remove_message_callback(msg);
        let _ = driver.dispose_buffers();
        let _ = driver.destroy();
    }

    /// Lit `ASIOGetBufferSize(min,max,pref,granularity)` du driver global chargé.
    fn asio_buffer_sizes() -> Option<(i32, i32, i32, i32)> {
        let (mut mn, mut mx, mut pf, mut gr) = (0i32, 0i32, 0i32, 0i32);
        let rc = unsafe { ASIOGetBufferSize(&mut mn, &mut mx, &mut pf, &mut gr) };
        if rc == 0 {
            Some((mn, mx, pf, gr))
        } else {
            None
        }
    }

    /// Snappe une taille de buffer DÉSIRÉE à une taille LÉGALE du driver (algorithme
    /// JUCE/Jamulus) : hors `[min,max]` → taille préférée ; granularité `-1` →
    /// puissance de 2 la plus proche ; `<= 0` → toute taille (on garde le désir) ;
    /// sinon → multiple de `gran` le plus proche. Ne force JAMAIS une taille illégale.
    fn snap_buffer_size(desired: i32, min: i32, max: i32, pref: i32, gran: i32) -> i32 {
        if min <= 0 || max < min {
            return pref.max(1);
        }
        if desired < min || desired > max {
            return pref; // JUCE : hors plage → préférée
        }
        if gran == -1 {
            let mut s = 1i32;
            while s < min {
                s = s.saturating_mul(2);
            }
            let mut best = s;
            while s <= max {
                if (s - desired).abs() < (best - desired).abs() {
                    best = s;
                }
                s = s.saturating_mul(2);
            }
            best
        } else if gran <= 0 {
            desired // granularité 0/<-1 : toute taille légale
        } else {
            let k = ((desired - min) as f64 / gran as f64).round() as i32;
            (min + k * gran).clamp(min, max)
        }
    }

    /// PRIMING (JUCE) : crée des buffers, démarre brièvement, attend, arrête, dispose.
    /// « cubase does this... some devices fail if we don't » — réveille/arme l'ADC des
    /// interfaces qui ne délivrent rien au 1er start à froid. Aucun callback enregistré :
    /// on ne fait que faire tourner l'horloge/DMA le temps de la réchauffe.
    fn prime_driver(driver: &asio_sys::Driver, n_in: usize, n_out: usize, size: i32) {
        match driver
            .prepare_input_stream(None, n_in, Some(size))
            .and_then(|s| driver.prepare_output_stream(s.input, n_out, Some(size)))
        {
            Ok(streams) => {
                let started = driver.start().is_ok();
                std::thread::sleep(std::time::Duration::from_millis(120));
                let _ = driver.stop();
                let _ = driver.dispose_buffers();
                drop(streams);
                println!(
                    "  priming : create+start({})+120ms+stop+dispose OK",
                    if started { "ok" } else { "start ÉCHOUÉ" }
                );
            }
            Err(e) => println!("  priming : prepare a échoué : {e:?}"),
        }
    }

    /// Mode `robust` : prototype du host single-owner robuste (invariants Jamulus+JUCE).
    /// Cf. le gros commentaire de dispatch.
    fn run_robust(monitor_secs: u64, main_tid: u32) {
        use asio_sys as sys;
        use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let buf_env: i32 = std::env::var("JAMLAB_BUF").ok().and_then(|s| s.parse().ok()).unwrap_or(64);
        let no_prime = std::env::var("JAMLAB_NO_PRIME").is_ok();
        println!(
            "=== robust === buf_désiré={buf_env} priming={} main_tid={main_tid} monitor={monitor_secs}s\n\
             (host single-owner : 1 Asio · snap taille · priming JUCE · 1 create(in+out) · 1 start · start-timeout)\n",
            if no_prime { "OFF" } else { "ON" }
        );

        // --- 1 SEULE instance Asio, 1 seul load_driver (1 ASIOInit) ---
        let asio = sys::Asio::new();
        let mut chosen: Option<(sys::Driver, String)> = None;
        for name in asio.driver_names() {
            if let Ok(d) = asio.load_driver(&name) {
                if d.channels().map(|c| c.ins).unwrap_or(0) > 0 {
                    chosen = Some((d, name));
                    break;
                }
                let _ = d.destroy();
            }
        }
        let Some((driver, name)) = chosen else {
            eprintln!("aucun driver ASIO chargeable avec entrée — carte branchée / libre ?");
            return;
        };
        let _ = driver.set_sample_rate(48_000.0);
        let in_is_int32 = matches!(driver.input_data_type().ok(), Some(sys::AsioSampleType::ASIOSTInt32LSB));
        let out_is_int32 = matches!(driver.output_data_type().ok(), Some(sys::AsioSampleType::ASIOSTInt32LSB));
        let ch = driver.channels().ok();
        let n_in = (ch.as_ref().map(|c| c.ins).unwrap_or(1).max(1) as usize).clamp(1, 2);
        let n_out = (ch.as_ref().map(|c| c.outs).unwrap_or(1).max(1) as usize).clamp(1, 2);

        // --- Snap de la taille à la grille légale du driver ---
        let size = match asio_buffer_sizes() {
            Some((mn, mx, pf, gr)) => {
                let s = snap_buffer_size(buf_env, mn, mx, pf, gr);
                println!("driver={name} : grille min={mn} max={mx} pref={pf} gran={gr} → taille retenue={s}");
                s
            }
            None => {
                println!("driver={name} : ASIOGetBufferSize indisponible → taille désirée={buf_env}");
                buf_env
            }
        };

        // --- PRIMING (JUCE) ---
        if no_prime {
            println!("priming : DÉSACTIVÉ (JAMLAB_NO_PRIME)");
        } else {
            println!("priming (JUCE — arme l'ADC des interfaces récalcitrantes) :");
            prime_driver(&driver, n_in, n_out, size);
        }

        // --- Ouverture RÉELLE : 1 seul ASIOCreateBuffers(in+out) ---
        let streams: Arc<Mutex<Option<sys::AsioStreams>>> = match driver
            .prepare_input_stream(None, n_in, Some(size))
            .and_then(|s| driver.prepare_output_stream(s.input, n_out, Some(size)))
        {
            Ok(s) => Arc::new(Mutex::new(Some(s))),
            Err(e) => {
                eprintln!("ouverture réelle : create_buffers a échoué : {e:?}");
                let _ = driver.destroy();
                return;
            }
        };
        let actual = streams
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|s| s.input.as_ref().or(s.output.as_ref()))
            .map(|s| s.buffer_size)
            .unwrap_or(-1);
        println!("ouverture OK : buffer réel={actual} (~{:.0} tics/s)\n", 48_000.0 / actual.max(1) as f64);

        // --- Callback duplex : lit l'entrée ch0 (mesure) + écrit la sortie (silence) ---
        let ticks = Arc::new(AtomicU64::new(0));
        let changes = Arc::new(AtomicU64::new(0));
        let absmax = Arc::new(AtomicI64::new(0));
        let prev0 = Arc::new(AtomicI64::new(i64::MIN));
        let cb_tid = Arc::new(AtomicU32::new(0));
        let cb = {
            let ticks = ticks.clone();
            let changes = changes.clone();
            let absmax = absmax.clone();
            let prev0 = prev0.clone();
            let cb_tid = cb_tid.clone();
            let streams = streams.clone();
            driver.add_callback(move |info| {
                cb_tid.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
                ticks.fetch_add(1, Ordering::Relaxed);
                let idx = info.buffer_index as usize;
                if idx > 1 {
                    return;
                }
                let Ok(mut guard) = streams.lock() else { return };
                let Some(st) = guard.as_mut() else { return };
                // ENTRÉE : fraîcheur ch0.
                if in_is_int32 {
                    if let Some(inp) = st.input.as_ref() {
                        let bufsz = inp.buffer_size as usize;
                        let ptr = inp.buffer_infos[0].buffers[idx] as *const i32;
                        if !ptr.is_null() && bufsz > 0 {
                            let slice = unsafe { std::slice::from_raw_parts(ptr, bufsz) };
                            let s0 = slice[0];
                            let prev = prev0.swap(s0 as i64, Ordering::Relaxed);
                            if prev != i64::MIN && prev != s0 as i64 {
                                changes.fetch_add(1, Ordering::Relaxed);
                            }
                            let mut amax = 0i64;
                            for &s in slice {
                                let a = (s as i64).abs();
                                if a > amax {
                                    amax = a;
                                }
                            }
                            absmax.fetch_max(amax, Ordering::Relaxed);
                        }
                    }
                }
                // SORTIE : silence.
                if out_is_int32 {
                    if let Some(out) = st.output.as_mut() {
                        let bufsz = out.buffer_size as usize;
                        for ci in 0..out.buffer_infos.len() {
                            let ptr = out.buffer_infos[ci].buffers[idx] as *mut i32;
                            if !ptr.is_null() {
                                let sl = unsafe { std::slice::from_raw_parts_mut(ptr, bufsz) };
                                sl.fill(0);
                            }
                        }
                    }
                }
            })
        };

        // --- 1 seul ASIOStart + start-timeout (attend le 1er callback ≤ 2 s) ---
        if driver.start().is_err() {
            eprintln!("ASIOStart a échoué");
            driver.remove_callback(cb);
            let _ = driver.dispose_buffers();
            let _ = driver.destroy();
            return;
        }
        let t0 = Instant::now();
        while ticks.load(Ordering::Relaxed) == 0 && t0.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        if ticks.load(Ordering::Relaxed) == 0 {
            println!("!! START-TIMEOUT : aucun callback en 2 s — driver muet (échec propre côté host).");
        } else {
            println!("1er callback reçu en {} ms — driver vivant.\n", t0.elapsed().as_millis());
        }

        // --- Surveillance de la fraîcheur de l'entrée ---
        let full = 2_147_483_648.0_f64;
        let (mut pt, mut pc) = (0u64, 0u64);
        let mut first_live: Option<u64> = None;
        let mut tid_logged = false;
        for s in 1..=monitor_secs {
            std::thread::sleep(Duration::from_secs(1));
            let (t, c) = (ticks.load(Ordering::Relaxed), changes.load(Ordering::Relaxed));
            let am = absmax.load(Ordering::Relaxed) as f64 / full;
            let (dt, dc) = (t - pt, c - pc);
            let live = dt > 0 && dc as f64 > dt as f64 * 0.25;
            if live && first_live.is_none() {
                first_live = Some(s);
            }
            if !tid_logged {
                let tid = cb_tid.load(Ordering::Relaxed);
                if tid != 0 {
                    println!(">>> callback tid={tid} (main={main_tid}) — {}", if tid == main_tid { "STA" } else { "thread driver" });
                    tid_logged = true;
                }
            }
            println!(
                "t={s:2}s tics/s={dt:4} chg/s={dc:4} |max|={am:.4} {}",
                if live { "VIVANT" } else { "— figé" }
            );
            pt = t;
            pc = c;
        }

        let total_changes = changes.load(Ordering::Relaxed);
        let amax = absmax.load(Ordering::Relaxed) as f64 / full;
        let vivant = first_live.map(|s| s <= 2).unwrap_or(false);
        println!(
            "\n=== VERDICT robust buf={actual} priming={} : changes={total_changes} |max|={amax:.4} first_live={first_live:?} → entrée {} ===",
            if no_prime { "OFF" } else { "ON" },
            if vivant { "VIVANTE dès le départ" } else { "PAS vivante d'emblée" }
        );
        if vivant {
            println!("→ Host robuste OK : l'entrée est armée dès le démarrage (séquence prête à porter dans l'agent).");
        } else {
            println!("→ Entrée non armée d'emblée — cf. first_live / start-timeout (à analyser).");
        }

        let _ = driver.stop();
        driver.remove_callback(cb);
        let _ = driver.dispose_buffers();
        let _ = driver.destroy();
        drop(streams);
    }

    /// Mode `dualasio` : reproduit le pattern 2-instances-Asio de l'agent. Cf. le gros
    /// commentaire de dispatch. Mesure la fraîcheur de l'entrée (instance A) avant puis
    /// après l'ouverture de la sortie (instance B = 2e ASIOInit sur le mono-client).
    fn run_dualasio(monitor_secs: u64, main_tid: u32) {
        use asio_sys as sys;
        use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        let buf_env: i32 = std::env::var("JAMLAB_BUF").ok().and_then(|s| s.parse().ok()).unwrap_or(64);
        let buf_req: Option<i32> = if buf_env <= 0 { None } else { Some(buf_env) };
        let skip_sr = std::env::var("JAMLAB_NO_SR").is_ok();
        println!(
            "=== dualasio === buf_req={buf_req:?} main_tid={main_tid} monitor={monitor_secs}s\n\
             (entrée via Asio#A, sortie via Asio#B = 2 ASIOInit sur le driver mono-client — mime l'agent)\n"
        );

        // --- Instance A : ENTRÉE (1er ASIOInit) ---
        let asio_a = sys::Asio::new();
        let mut chosen: Option<(sys::Driver, String)> = None;
        for name in asio_a.driver_names() {
            if let Ok(d) = asio_a.load_driver(&name) {
                if d.channels().map(|c| c.ins).unwrap_or(0) > 0 {
                    chosen = Some((d, name));
                    break;
                }
                let _ = d.destroy();
            }
        }
        let Some((driver_a, name)) = chosen else {
            eprintln!("aucun driver ASIO chargeable avec entrée — carte branchée / libre ?");
            return;
        };
        if skip_sr {
            println!("Asio#A set_sample_rate: SKIP (JAMLAB_NO_SR)");
        } else {
            let _ = driver_a.set_sample_rate(48_000.0);
        }
        let in_is_int32 = matches!(driver_a.input_data_type().ok(), Some(sys::AsioSampleType::ASIOSTInt32LSB));
        let ch = driver_a.channels().ok();
        let n_in = (ch.as_ref().map(|c| c.ins).unwrap_or(1).max(1) as usize).clamp(1, 2);
        let n_out = (ch.as_ref().map(|c| c.outs).unwrap_or(1).max(1) as usize).clamp(1, 2);

        let streams_a: Arc<Mutex<Option<sys::AsioStreams>>> =
            match driver_a.prepare_input_stream(None, n_in, buf_req) {
                Ok(s) => Arc::new(Mutex::new(Some(s))),
                Err(e) => {
                    eprintln!("Asio#A prepare_input a échoué : {e:?}");
                    let _ = driver_a.destroy();
                    return;
                }
            };

        // Fraîcheur de l'entrée (ch0) mesurée par le callback de A.
        let ticks = Arc::new(AtomicU64::new(0));
        let changes = Arc::new(AtomicU64::new(0));
        let absmax = Arc::new(AtomicI64::new(0));
        let prev0 = Arc::new(AtomicI64::new(i64::MIN));
        let cb_a = {
            let ticks = ticks.clone();
            let changes = changes.clone();
            let absmax = absmax.clone();
            let prev0 = prev0.clone();
            let streams_a = streams_a.clone();
            driver_a.add_callback(move |info| {
                ticks.fetch_add(1, Ordering::Relaxed);
                let idx = info.buffer_index as usize;
                if idx > 1 || !in_is_int32 {
                    return;
                }
                let Ok(guard) = streams_a.lock() else { return };
                let Some(st) = guard.as_ref() else { return };
                let Some(inp) = st.input.as_ref() else { return };
                let bufsz = inp.buffer_size as usize;
                let ptr = inp.buffer_infos[0].buffers[idx] as *const i32;
                if ptr.is_null() || bufsz == 0 {
                    return;
                }
                // SAFETY : buffer d'entrée ch0. NB : si la création de la sortie (Asio#B)
                // a disposé ces buffers, on lit alors une zone potentiellement recyclée —
                // c'est EXACTEMENT ce que fait l'agent, et ce qu'on cherche à observer.
                let slice = unsafe { std::slice::from_raw_parts(ptr, bufsz) };
                let s0 = slice[0];
                let prev = prev0.swap(s0 as i64, Ordering::Relaxed);
                if prev != i64::MIN && prev != s0 as i64 {
                    changes.fetch_add(1, Ordering::Relaxed);
                }
                let mut amax = 0i64;
                for &s in slice {
                    let a = (s as i64).abs();
                    if a > amax {
                        amax = a;
                    }
                }
                absmax.fetch_max(amax, Ordering::Relaxed);
            })
        };
        if driver_a.start().is_err() {
            eprintln!("Asio#A ASIOStart a échoué");
            driver_a.remove_callback(cb_a);
            let _ = driver_a.destroy();
            return;
        }

        let full = 2_147_483_648.0_f64;
        // --- Phase 1 : entrée seule (1 instance Asio), 1,5 s ---
        println!("Asio#A : entrée démarrée (1 seul ASIOInit). Mesure 1,5 s AVANT la sortie…");
        std::thread::sleep(Duration::from_millis(1500));
        let pre_changes = changes.load(Ordering::Relaxed);
        let pre_amax = absmax.load(Ordering::Relaxed) as f64 / full;
        let pre_ticks = ticks.load(Ordering::Relaxed);
        let pre_live = pre_changes as f64 > pre_ticks as f64 * 0.25;
        println!(
            ">>> PRÉ-sortie : ticks={pre_ticks} changes={pre_changes} |max|={pre_amax:.4} → entrée {}\n",
            if pre_live { "VIVANTE" } else { "FIGÉE" }
        );

        // --- Phase 2 : ouvre la SORTIE via une 2e instance Asio (= 2e ASIOInit) ---
        println!(">>> ouverture de la sortie via Asio#B (2e instance, 2e ASIOInit sur le mono-client)…");
        let asio_b = sys::Asio::new();
        let mut _driver_b: Option<sys::Driver> = None;
        let mut _streams_b: Option<Arc<Mutex<Option<sys::AsioStreams>>>> = None;
        let mut cb_b: Option<sys::CallbackId> = None;
        match asio_b.load_driver(&name) {
            Ok(driver_b) => {
                println!("    Asio#B : load_driver + ASIOInit RÉUSSI (2e init).");
                match driver_b.prepare_output_stream(None, n_out, buf_req) {
                    Ok(sb) => {
                        println!("    Asio#B : prepare_output (create OUT) OK — a pu disposer les buffers d'entrée de A.");
                        let out_int32 = matches!(driver_b.output_data_type().ok(), Some(sys::AsioSampleType::ASIOSTInt32LSB));
                        let sb = Arc::new(Mutex::new(Some(sb)));
                        cb_b = Some({
                            let sb = sb.clone();
                            driver_b.add_callback(move |info| {
                                let idx = info.buffer_index as usize;
                                if idx > 1 || !out_int32 {
                                    return;
                                }
                                let Ok(guard) = sb.lock() else { return };
                                let Some(st) = guard.as_ref() else { return };
                                let Some(out) = st.output.as_ref() else { return };
                                let bufsz = out.buffer_size as usize;
                                for ci in 0..out.buffer_infos.len() {
                                    let ptr = out.buffer_infos[ci].buffers[idx] as *mut i32;
                                    if ptr.is_null() {
                                        continue;
                                    }
                                    let sl = unsafe { std::slice::from_raw_parts_mut(ptr, bufsz) };
                                    sl.fill(0);
                                }
                            })
                        });
                        let _ = driver_b.start();
                        _streams_b = Some(sb);
                        _driver_b = Some(driver_b);
                    }
                    Err(e) => println!("    Asio#B : prepare_output ÉCHOUÉ : {e:?}"),
                }
            }
            Err(e) => println!("    Asio#B : load_driver ÉCHOUÉ : {e:?} — 2e ASIOInit REFUSÉ par asio-sys (≠ cpal qui, lui, réussit)."),
        }
        println!();

        // --- Phase 3 : surveillance de la fraîcheur de l'entrée APRÈS la sortie ---
        let mut prev_ticks = ticks.load(Ordering::Relaxed);
        let mut prev_changes = changes.load(Ordering::Relaxed);
        let mut post_frozen_s: Option<u64> = None;
        for s in 1..=monitor_secs {
            std::thread::sleep(Duration::from_secs(1));
            let t = ticks.load(Ordering::Relaxed);
            let c = changes.load(Ordering::Relaxed);
            let am = absmax.load(Ordering::Relaxed) as f64 / full;
            let dt = t - prev_ticks;
            let dc = c - prev_changes;
            let live = dt > 0 && dc as f64 > dt as f64 * 0.25;
            if !live && post_frozen_s.is_none() {
                post_frozen_s = Some(s);
            }
            println!(
                "t={s:2}s (post-sortie) tics/s={dt:4} chg/s={dc:4} |max|={am:.4} {}",
                if live { "VIVANT" } else { "— FIGÉ" }
            );
            prev_ticks = t;
            prev_changes = c;
        }

        // --- Verdict ---
        let post_changes = changes.load(Ordering::Relaxed) - pre_changes;
        println!(
            "\n=== VERDICT dualasio : entrée PRÉ-sortie={} ; POST-sortie={} ===",
            if pre_live { "VIVANTE" } else { "FIGÉE" },
            if post_frozen_s.is_some() && post_changes < 50 { "FIGÉE" } else { "VIVANTE" }
        );
        if pre_live && post_frozen_s.is_some() && post_changes < 50 {
            println!("→ MÉCANISME PROUVÉ : l'entrée était VIVANTE avec 1 instance Asio, puis a FIGÉ dès l'ouverture");
            println!("  de la sortie via une 2e instance Asio (2e ASIOInit). ⇒ le fix = partager UNE instance host/Asio.");
        } else if pre_live {
            println!("→ L'entrée est restée VIVANTE malgré la 2e instance Asio — le double-init n'est pas nocif dans");
            println!("  cet état (chaud ?). À rejouer à froid, ou le mécanisme est ailleurs.");
        } else {
            println!("→ Entrée déjà figée avant la sortie — état inattendu (relancer ; interface libre ?).");
        }

        // --- Cleanup ---
        let _ = driver_a.stop();
        driver_a.remove_callback(cb_a);
        if let (Some(db), Some(id)) = (_driver_b.as_ref(), cb_b) {
            let _ = db.stop();
            db.remove_callback(id);
        }
        let _ = driver_a.dispose_buffers();
        let _ = driver_a.destroy();
        drop(_streams_b);
        drop(streams_a);
    }

    /// Mode `latency` : mesure la latence ASIO réelle (ASIOGetLatencies) à
    /// différentes tailles de buffer. `ASIOGetLatencies` n'est pas wrappé par
    /// asio-sys mais le symbole C est compilé dans le même objet (asio.cpp) que
    /// `ASIOInit` & co → on le déclare nous-mêmes en extern "C". Il opère sur le
    /// driver global chargé par asio-sys (`theAsioDriver`), donc valable après
    /// `load_driver` + `prepare_*_stream`. Latence en samples → ms @48k.
    fn run_latency() {
        use asio_sys as sys;

        let sr = 48_000.0f64;
        // Sous la préférée (128) : tailles usuelles + non-alignées pour sonder la
        // grille du driver (asio-sys ne valide QUE `<= max`, c'est ASIOCreateBuffers
        // qui accepte/refuse selon la granularité réelle).
        let candidates: [i32; 8] = [16, 32, 48, 64, 96, 128, 256, 512];
        println!("=== Latence ASIO contrôlable (buffer) @ {sr} Hz — Focusrite ===");
        println!("RTT bufferisé = 2×buffer/SR (latence in+out du buffering ASIO, hors converter USB fixe).");
        println!("But : voir quelles tailles < 128 le driver ACCEPTE et le gain de RTT associé.\n");
        println!("{:>8}  {:>8}  {:>12}  {:>14}  vs 128", "buf_req", "buf_act", "range", "RTT bufferisé");

        let rtt_128 = 2.0 * 128.0 / sr * 1000.0;
        for &bs in &candidates {
            let asio = sys::Asio::new();
            let mut chosen: Option<sys::Driver> = None;
            for name in asio.driver_names() {
                if let Ok(d) = asio.load_driver(&name) {
                    if d.channels().map(|c| c.ins).unwrap_or(0) > 0 {
                        chosen = Some(d);
                        break;
                    }
                    let _ = d.destroy();
                }
            }
            let Some(driver) = chosen else {
                eprintln!("aucun driver ASIO chargeable — carte branchée / libre (agent arrêté) ?");
                return;
            };
            let _ = driver.set_sample_rate(sr);
            let (bmin, bmax) = driver.buffersize_range().unwrap_or((-1, -1));

            // Ouvre RÉELLEMENT (create + start court) pour vérifier que la taille
            // TIENT, pas juste qu'elle est acceptée à la création.
            let built = driver
                .prepare_input_stream(None, 2, Some(bs))
                .and_then(|s| driver.prepare_output_stream(s.input, 2, Some(bs)));
            match built {
                Ok(streams) => {
                    let actual = streams
                        .output
                        .as_ref()
                        .or(streams.input.as_ref())
                        .map(|s| s.buffer_size)
                        .unwrap_or(bs);
                    // Démarre 300 ms et compte les tics : une taille acceptée mais
                    // instable (callbacks nuls) est disqualifiée.
                    let ticks = std::sync::Arc::new(AtomicU64::new(0));
                    let t = ticks.clone();
                    let cb = driver.add_callback(move |_| {
                        t.fetch_add(1, Ordering::Relaxed);
                    });
                    let started = driver.start().is_ok();
                    std::thread::sleep(Duration::from_millis(300));
                    let n = ticks.load(Ordering::Relaxed);
                    let _ = driver.stop();
                    driver.remove_callback(cb);
                    let rtt = 2.0 * actual as f64 / sr * 1000.0;
                    let stable = started && n as f64 > (0.300 * sr / actual as f64) * 0.5;
                    println!(
                        "{:>8}  {:>8}  {:>12}  {:>9.2} ms   {:+.2} ms  {}  ({} tics/300ms)",
                        bs, actual, format!("{bmin}..{bmax}"), rtt, rtt - rtt_128,
                        if stable { "OK" } else { "INSTABLE" }, n
                    );
                }
                Err(e) => {
                    println!("{bs:>8}  {:>8}  {:>12}  create_buffers REFUSÉ: {e:?}", "-", format!("{bmin}..{bmax}"));
                }
            }
            let _ = driver.dispose_buffers();
            let _ = driver.destroy();
        }
        println!("\nNB : buf_act peut être snappé par le driver sur sa grille. « vs 128 » = gain (−) ou coût (+)");
        println!("de RTT bufferisé face à la taille préférée actuelle. Le converter USB (latence fixe) s'ajoute aux deux.");
    }

    // Fonctions host ASIO NON wrappées par asio-sys, mais compilées dans le même
    // objet (asio.cpp du SDK) et donc résolues au link. Elles opèrent sur le driver
    // global chargé par asio-sys (`theAsioDriver`) → valables après `load_driver`
    // (+ `ASIOCreateBuffers` pour les latences). `long` = i32 sur Windows (LLP64),
    // `ASIOError` = long, 0 = ASE_OK. asio.cpp est compilé en C++ → les symboles
    // sont NAME-MANGLED MSVC (signature `long __cdecl f(long*, …)`) ; on cible le
    // nom mangled exact via `#[link_name]` (convention __cdecl compatible extern"C").
    extern "C" {
        #[link_name = "?ASIOGetBufferSize@@YAJPEAJ000@Z"]
        fn ASIOGetBufferSize(min: *mut i32, max: *mut i32, pref: *mut i32, gran: *mut i32) -> i32;
        #[link_name = "?ASIOGetLatencies@@YAJPEAJ0@Z"]
        fn ASIOGetLatencies(input_latency: *mut i32, output_latency: *mut i32) -> i32;
    }

    /// Mode `bufinfo` : dump de la grille de tailles de buffer ASIO du driver.
    /// Cf. le commentaire de dispatch. Ne crée rien de durable — charge, interroge,
    /// mesure les latences à la taille préférée, puis relâche proprement.
    fn run_bufinfo() {
        use asio_sys as sys;

        let sr = 48_000.0f64;
        let asio = sys::Asio::new();
        let mut chosen: Option<(sys::Driver, String)> = None;
        for name in asio.driver_names() {
            if let Ok(d) = asio.load_driver(&name) {
                if d.channels().map(|c| c.ins).unwrap_or(0) > 0 {
                    chosen = Some((d, name));
                    break;
                }
                let _ = d.destroy();
            }
        }
        let Some((driver, name)) = chosen else {
            eprintln!("aucun driver ASIO chargeable avec entrée — carte branchée / libre (agent arrêté) ?");
            return;
        };
        let _ = driver.set_sample_rate(sr);

        let ch = driver.channels().ok();
        let ins = ch.as_ref().map(|c| c.ins).unwrap_or(-1);
        let outs = ch.as_ref().map(|c| c.outs).unwrap_or(-1);
        let in_ty = driver.input_data_type().ok();
        let out_ty = driver.output_data_type().ok();
        let real_sr = driver.sample_rate().unwrap_or(0.0);
        println!("=== bufinfo === driver={name}");
        println!("  channels : in={ins} out={outs}   sample_rate={real_sr} (demandé {sr})");
        println!("  format   : in={in_ty:?} out={out_ty:?}");

        // --- ASIOGetBufferSize brut ---
        let (mut min, mut max, mut pref, mut gran) = (0i32, 0i32, 0i32, 0i32);
        let rc = unsafe { ASIOGetBufferSize(&mut min, &mut max, &mut pref, &mut gran) };
        if rc != 0 {
            println!("  ASIOGetBufferSize a échoué (rc={rc}) — driver non prêt ?");
            let _ = driver.destroy();
            return;
        }
        println!("\n  ASIOGetBufferSize : min={min} max={max} pref={pref} granularity={gran}");
        let gran_expl = match gran {
            -1 => "puissances de 2 uniquement (min, 2·min, 4·min…)".to_string(),
            0 => "0 → taille FIXE : seul `pref` est légal".to_string(),
            g if g < -1 => format!("{g} (< -1, non conforme) → traité comme « toute taille ∈ [min,max] »"),
            g => format!("pas de {g} samples (min, min+{g}, min+2·{g}…)"),
        };
        println!("  granularité       : {gran_expl}");

        // --- Grille légale (algorithme JUCE addBufferSizes) ---
        let grid = legal_buffer_sizes(min, max, pref, gran);
        println!("  grille légale     : {grid:?}");

        // --- 64 est-il légal ? (LA question du bug cold-start) ---
        let sixty_four_legal = grid.contains(&64);
        println!(
            "\n  >>> 64 samples est-il une taille ASIO LÉGALE ? {}",
            if sixty_four_legal { "OUI" } else { "NON" }
        );
        if !sixty_four_legal {
            println!(
                "      → SMOKING GUN : forcer Fixed(64) passe une taille HORS grille à\n\
                 \x20       ASIOCreateBuffers (asio-sys ne valide que `<= max`). C'est très\n\
                 \x20       probablement la cause du wedge cold-start. Fix = snapper à une\n\
                 \x20       taille légale (idéalement {pref} = préférée), comme Jamulus/JUCE."
            );
        } else {
            println!(
                "      → 64 est légal ici. Si l'entrée rail quand même à froid, la cause\n\
                 \x20       n'est PAS la valeur de taille mais le churn create→start→create\n\
                 \x20       de cpal (le host single-owner reste le fix)."
            );
        }

        // --- Latences réelles à la taille préférée ---
        println!("\n  latences ASIO à la taille préférée ({pref} samples) :");
        let n_in = (ins.max(0) as usize).clamp(1, 2);
        let n_out = (outs.max(0) as usize).clamp(1, 2);
        match driver
            .prepare_input_stream(None, n_in, Some(pref))
            .and_then(|s| driver.prepare_output_stream(s.input, n_out, Some(pref)))
        {
            Ok(_streams) => {
                let (mut li, mut lo) = (0i32, 0i32);
                let rc = unsafe { ASIOGetLatencies(&mut li, &mut lo) };
                if rc == 0 {
                    let ms = |samp: i32| samp as f64 / sr * 1000.0;
                    println!(
                        "    input={li} samples ({:.2} ms)  output={lo} samples ({:.2} ms)  \
                         RTT total ≈ {:.2} ms",
                        ms(li),
                        ms(lo),
                        ms(li + lo)
                    );
                    println!(
                        "    (ces latences incluent le pipeline USB/converter FIXE du driver, \
                         pas seulement le buffer)"
                    );
                } else {
                    println!("    ASIOGetLatencies a échoué (rc={rc})");
                }
                let _ = driver.dispose_buffers();
            }
            Err(e) => println!("    create_buffers @pref a échoué : {e:?}"),
        }

        let _ = driver.destroy();
        println!("\n=== fin bufinfo ===");
    }

    /// Énumère les tailles de buffer légales exposées par le driver, façon JUCE
    /// `addBufferSizes` (respecte la granularité). Sert à dire si une taille donnée
    /// (64) est réellement atteignable. Liste bornée pour rester lisible.
    fn legal_buffer_sizes(min: i32, max: i32, pref: i32, gran: i32) -> Vec<i32> {
        let mut sizes = Vec::new();
        if min <= 0 || max < min {
            return sizes;
        }
        if gran == -1 {
            // Puissances de 2 dans [min, max].
            let mut s = 1i32;
            while s < min {
                s = s.saturating_mul(2);
            }
            while s <= max && sizes.len() < 32 {
                sizes.push(s);
                s = s.saturating_mul(2);
            }
        } else if gran <= 0 {
            // gran == 0 : taille fixe (pref) ; gran < -1 : non conforme → on montre
            // les bornes + pref (toute taille intermédiaire supposée légale).
            for v in [min, pref, max] {
                if v >= min && v <= max && !sizes.contains(&v) {
                    sizes.push(v);
                }
            }
            sizes.sort_unstable();
        } else {
            // Pas de `gran` samples à partir de `min`.
            let step = gran.max(1);
            let mut s = min;
            while s <= max && sizes.len() < 64 {
                sizes.push(s);
                s = s.saturating_add(step);
            }
            if !sizes.contains(&pref) && pref >= min && pref <= max {
                sizes.push(pref);
                sizes.sort_unstable();
            }
        }
        sizes
    }

    /// Mode `cpal2` : reproduit le chemin de production (2 cpal::Stream séparés).
    /// Ouvre l'ENTRÉE et la SORTIE comme capture.rs/playback.rs (format natif i32,
    /// BufferSize::Default = préféré du driver), démarre la sortie PUIS l'entrée
    /// (ordre de l'agent), et surveille la vivacité des DEUX callbacks. Le bug de
    /// prod : ces callbacks gèlent (~20 s) sur le Focusrite mono-client.
    fn run_cpal2(monitor_secs: u64, main_tid: u32) {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        use cpal::{BufferSize, SampleFormat, SampleRate, StreamConfig};

        let host = match cpal::host_from_id(cpal::HostId::Asio) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("host ASIO indisponible : {e}");
                return;
            }
        };
        let in_dev = host.default_input_device();
        let out_dev = host.default_output_device();
        let (Some(in_dev), Some(out_dev)) = (in_dev, out_dev) else {
            eprintln!("device d'entrée/sortie ASIO introuvable");
            return;
        };
        println!(
            "cpal2 : in={:?} out={:?}",
            in_dev.name().ok(),
            out_dev.name().ok()
        );

        let in_cfg = in_dev.default_input_config().expect("default_input_config");
        let out_cfg = out_dev.default_output_config().expect("default_output_config");
        println!(
            "cpal2 : in_cfg={:?} out_cfg={:?}",
            (in_cfg.channels(), in_cfg.sample_rate().0, in_cfg.sample_format()),
            (out_cfg.channels(), out_cfg.sample_rate().0, out_cfg.sample_format()),
        );
        // Les interfaces ASIO Focusrite sont en Int32 natif (cf. capture.rs).
        if in_cfg.sample_format() != SampleFormat::I32
            || out_cfg.sample_format() != SampleFormat::I32
        {
            eprintln!("cpal2 attend du I32 natif (ASIO Focusrite) — format inattendu, on tente quand même via i32");
        }

        let in_conf = StreamConfig {
            channels: in_cfg.channels(),
            sample_rate: SampleRate(in_cfg.sample_rate().0),
            buffer_size: BufferSize::Default, // préféré du driver (comme l'agent sur ASIO)
        };
        let out_conf = StreamConfig {
            channels: out_cfg.channels(),
            sample_rate: SampleRate(out_cfg.sample_rate().0),
            buffer_size: BufferSize::Default,
        };

        let in_cb = Arc::new(AtomicU64::new(0));
        let out_cb = Arc::new(AtomicU64::new(0));
        let in_tid = Arc::new(AtomicU32::new(0));
        let out_tid = Arc::new(AtomicU32::new(0));
        // Fraîcheur de l'entrée ch0 — isole cpal : reproduit-il le figé de l'agent
        // (que le chemin asio-sys direct, lui, n'a jamais) ?
        use std::sync::atomic::AtomicI64;
        let in_changes = Arc::new(AtomicU64::new(0));
        let in_absmax = Arc::new(AtomicI64::new(0));
        let in_prev0 = Arc::new(AtomicI64::new(i64::MIN));
        let in_nch = (in_conf.channels as usize).max(1);

        let in_cb_w = in_cb.clone();
        let in_tid_w = in_tid.clone();
        let in_changes_w = in_changes.clone();
        let in_absmax_w = in_absmax.clone();
        let in_prev0_w = in_prev0.clone();
        let input_stream = in_dev
            .build_input_stream(
                &in_conf,
                move |data: &[i32], _: &cpal::InputCallbackInfo| {
                    in_tid_w.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
                    in_cb_w.fetch_add(1, Ordering::Relaxed);
                    // Fraîcheur ch0 (indices 0, nch, 2·nch…) : varie = vivant ; figé = wedge.
                    if let Some(&s0) = data.first() {
                        let prev = in_prev0_w.swap(s0 as i64, Ordering::Relaxed);
                        if prev != i64::MIN && prev != s0 as i64 {
                            in_changes_w.fetch_add(1, Ordering::Relaxed);
                        }
                        let mut amax = 0i64;
                        let mut i = 0;
                        while i < data.len() {
                            let a = (data[i] as i64).abs();
                            if a > amax {
                                amax = a;
                            }
                            i += in_nch;
                        }
                        in_absmax_w.fetch_max(amax, Ordering::Relaxed);
                    }
                },
                |e| eprintln!("cpal input error: {e}"),
                None,
            )
            .expect("build_input_stream");

        let out_cb_w = out_cb.clone();
        let out_tid_w = out_tid.clone();
        let output_stream = out_dev
            .build_output_stream(
                &out_conf,
                move |data: &mut [i32], _: &cpal::OutputCallbackInfo| {
                    out_tid_w.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
                    out_cb_w.fetch_add(1, Ordering::Relaxed);
                    for s in data.iter_mut() {
                        *s = 0; // silence
                    }
                },
                |e| eprintln!("cpal output error: {e}"),
                None,
            )
            .expect("build_output_stream");

        // Ordre de l'agent : sortie d'abord, puis entrée (cf. open_duplex_on_com).
        output_stream.play().expect("play output");
        input_stream.play().expect("play input");
        println!("cpal2 : 2 streams démarrés (2 ASIOStart) — surveillance {monitor_secs}s\n");

        let (mut pin, mut pout) = (0u64, 0u64);
        let (mut in_growth, mut out_growth) = (0u64, 0u64);
        let mut tids_logged = false;
        for s in 1..=monitor_secs {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let (ni, no) = (in_cb.load(Ordering::Relaxed), out_cb.load(Ordering::Relaxed));
            let (di, do_) = (ni - pin, no - pout);
            if di > 0 {
                in_growth = s;
            }
            if do_ > 0 {
                out_growth = s;
            }
            if !tids_logged {
                let (it, ot) = (in_tid.load(Ordering::Relaxed), out_tid.load(Ordering::Relaxed));
                if it != 0 || ot != 0 {
                    println!(">>> in_tid={it} out_tid={ot} (main={main_tid})");
                    tids_logged = true;
                }
            }
            println!("t={s:2}s  in/s={di:4} out/s={do_:4}  in_tot={ni:6} out_tot={no:6}");
            pin = ni;
            pout = no;
        }

        let wedged = in_growth < monitor_secs.saturating_sub(1)
            || out_growth < monitor_secs.saturating_sub(1);
        println!(
            "\n=== VERDICT cpal2 : in_last_growth={in_growth}s out_last_growth={out_growth}s wedged={wedged} ===",
        );
        if wedged {
            println!("→ WEDGE reproduit : un des deux flux a cessé (chemin 2-streams de prod).");
        }

        // LE test : cpal reproduit-il le figé de l'entrée (que l'agent a et le lab
        // asio-sys direct n'a jamais) ?
        let full = 2_147_483_648.0_f64;
        let ch = in_changes.load(Ordering::Relaxed);
        let am = in_absmax.load(Ordering::Relaxed) as f64 / full;
        let cbn = in_cb.load(Ordering::Relaxed).max(1);
        let live_ratio = ch as f64 / cbn as f64;
        println!(
            "\n=== fraîcheur ENTRÉE cpal2 : changes={ch} callbacks={cbn} live_ratio={:.0}% |max|={am:.4} ===",
            live_ratio * 100.0
        );
        if live_ratio < 0.05 {
            println!("→ ENTRÉE FIGÉE via cpal (2 streams séparés) — REPRODUIT le wedge de l'agent ⇒ cpal est en cause (le chemin asio-sys direct, lui, arme l'entrée).");
        } else {
            println!("→ Entrée VIVANTE via cpal — le figé ne vient PAS de cpal seul ; suspect = contexte agent (énumérations boot / keep-warm / réutilisation de l'ASIOInit du boot).");
        }

        let _ = input_stream.pause();
        let _ = output_stream.pause();
        drop(input_stream);
        drop(output_stream);
    }

    /// Mode diagnostic `coexist` : énumère les devices ASIO via CPAL (mirror de
    /// `host::probe()` au boot de l'agent). Renvoie le `cpal::Host` si `keep_alive`
    /// (pour tester la coexistence pendant que notre duplex tourne), sinon le droppe.
    fn cpal_enumerate(keep_alive: bool) -> Option<cpal::Host> {
        use cpal::traits::HostTrait;
        match cpal::host_from_id(cpal::HostId::Asio) {
            Ok(h) => {
                let n_in = h.input_devices().map(|it| it.count()).unwrap_or(0);
                let n_out = h.output_devices().map(|it| it.count()).unwrap_or(0);
                println!(
                    "[coexist] énumération cpal ASIO : inputs={n_in} outputs={n_out} \
                     (host cpal {})",
                    if keep_alive { "GARDÉ vivant" } else { "droppé" }
                );
                if keep_alive {
                    Some(h)
                } else {
                    drop(h);
                    None
                }
            }
            Err(e) => {
                println!("[coexist] cpal host_from_id(Asio) a échoué : {e}");
                None
            }
        }
    }

    /// Attend ~1 s. Si `pump`, draine la file de messages Win32 du thread courant
    /// (PeekMessage/Translate/Dispatch) au lieu de simplement dormir — pour tester
    /// si le driver a besoin que son thread créateur pompe les messages.
    fn wait_one_second(pump: bool) {
        if !pump {
            std::thread::sleep(Duration::from_secs(1));
            return;
        }
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(1) {
            unsafe {
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

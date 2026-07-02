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
        // Mode `churn` : ouvre/ferme le driver en boucle rapide (load→create→start
        // →hold→stop→dispose→destroy) — reproduit le cycle ASIOExit/ASIOInit que
        // l'agent faisait à chaque leave/rejoin (cause racine présumée 0.5.4-5,
        // tueur classique des drivers USB ASIO). argv[2] = nb de cycles (défaut 12),
        // JAMLAB_HOLD_MS = durée chaude par cycle (défaut 400 ms).
        if mode == "churn" {
            run_churn(monitor_secs.max(1));
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

        let in_cb_w = in_cb.clone();
        let in_tid_w = in_tid.clone();
        let input_stream = in_dev
            .build_input_stream(
                &in_conf,
                move |_data: &[i32], _: &cpal::InputCallbackInfo| {
                    in_tid_w.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
                    in_cb_w.fetch_add(1, Ordering::Relaxed);
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

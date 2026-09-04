//! Diagnostic hors-ligne de la chaîne d'isolation talkback (denoise → VAD → gate).
//!
//! Sert à isoler un défaut DSP (hachure, coupures, artefacts) du reste (routing,
//! thread live). Rejoue la chaîne EXACTEMENT comme `VoiceIsolator` (mêmes briques
//! publiques, même cadencement de décision) mais en instrumentant chaque étage, et
//! la compare à un gate **oracle non causal** (celui des démos offline validées :
//! décision connue à l'avance + padding avant/après) — l'écart entre les deux
//! chiffre précisément ce que coûte la causalité.
//!
//! Usage :
//!   cargo run --release --example iso_offline -- <in.wav> <out_dir> [prefix]
//!
//! Écrit dans `<out_dir>` : `<prefix>_chain.wav` (chaîne actuelle), `_denoise.wav`
//! (denoise seul), `_oracle.wav` (denoise + gate oracle), `_trace.csv` (proba VAD,
//! gains causal/oracle, RMS) et imprime un rapport.

use std::collections::VecDeque;

use jamodio_audio_core::voice_isolation::gate::{GateParams, VoiceGate};
use jamodio_audio_core::voice_isolation::resample::Decimator3;
use jamodio_audio_core::voice_isolation::vad::VAD_FRAME;
use jamodio_audio_core::voice_isolation::{DenoiseParams, Denoiser, IsolationConfig, Vad, VoiceIsolator};

const SR: usize = 48_000;
const BLOCK: usize = 480; // 10 ms — taille de bloc de référence
/// Échantillons 48 k couverts par une trame VAD (512 @16k × 3).
const VAD_FRAME_48K: usize = VAD_FRAME * 3;

fn rms(s: &[f32]) -> f32 {
    (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt()
}

fn db(x: f32) -> f32 {
    20.0 * (x.max(1e-12)).log10()
}

fn read_wav_mono48(path: &str) -> Vec<f32> {
    let mut reader = hound::WavReader::open(path).expect("wav lisible");
    let spec = reader.spec();
    eprintln!(
        "in: {path}  sr={} ch={} bits={} fmt={:?}",
        spec.sample_rate, spec.channels, spec.bits_per_sample, spec.sample_format
    );
    assert_eq!(spec.sample_rate as usize, SR, "l'isolation attend du 48 kHz");
    let mut samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().map(|s| s.unwrap() as f32 / scale).collect()
        }
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
    };
    if spec.channels == 2 {
        samples = samples.chunks(2).map(|c| (c[0] + c.get(1).copied().unwrap_or(0.0)) * 0.5).collect();
    }
    samples
}

fn write_wav(path: &str, samples: &[f32]) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SR as u32,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for &s in samples {
        w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16).unwrap();
    }
    w.finalize().unwrap();
    eprintln!("écrit {path}");
}

/// Étage 1 — denoise seul, bloc par bloc (streaming, comme en production).
fn denoise_all(input: &[f32]) -> Vec<f32> {
    denoise_with(input, DenoiseParams::default())
}

fn denoise_with(input: &[f32], params: DenoiseParams) -> Vec<f32> {
    // ISO_BLOCK permet de rejouer la chaîne avec la taille de bloc RÉELLE de la
    // capture (souvent 128 éch.), pas seulement un multiple du hop du modèle.
    let blk: usize = std::env::var("ISO_BLOCK").ok().and_then(|v| v.parse().ok()).unwrap_or(BLOCK);
    let mut den = Denoiser::with_params(params).expect("modèle DeepFilterNet");
    let mut out = Vec::with_capacity(input.len());
    for chunk in input.chunks(blk) {
        let mut b = chunk.to_vec();
        den.process_block(&mut b).unwrap();
        out.extend_from_slice(&b);
    }
    out
}

/// Étage 2 — probas VAD sur le signal fourni, une par trame de 512 @16k.
/// Renvoie (proba, index du DERNIER échantillon 48 k de la trame) : c'est
/// l'instant où la décision devient disponible en causal.
fn vad_probs(signal: &[f32]) -> Vec<(f32, usize)> {
    let mut dec = Decimator3::new(SR as f32);
    let mut vad = Vad::new().expect("modèle Silero");
    let mut acc: VecDeque<f32> = VecDeque::new();
    let mut frame = vec![0.0f32; VAD_FRAME];
    let mut probs = Vec::new();
    // On décime bloc par bloc pour connaître l'instant 48 k de chaque décision.
    let mut consumed = 0usize;
    for chunk in signal.chunks(BLOCK) {
        dec.process_into(chunk, &mut acc);
        consumed += chunk.len();
        while acc.len() >= VAD_FRAME {
            for s in frame.iter_mut() {
                *s = acc.pop_front().unwrap();
            }
            probs.push((vad.speech_prob(&frame).unwrap(), consumed));
        }
    }
    probs
}

/// Gate CAUSAL — reproduit fidèlement `VoiceIsolator` : la décision de la trame
/// courante ne s'applique qu'à partir du bloc où elle tombe. Renvoie les gains
/// par échantillon.
fn causal_gains(len: usize, probs: &[(f32, usize)], cfg: &IsolationConfig) -> Vec<f32> {
    let mut gate = VoiceGate::new(SR as f32, cfg.gate);
    let mut gains = vec![0.0f32; len];
    let mut speech = false;
    let mut next = 0usize;
    let mut pos = 0usize;
    while pos < len {
        let n = BLOCK.min(len - pos);
        // Décisions devenues disponibles à la fin de ce bloc (comme en production :
        // la boucle `while accum >= VAD_FRAME` tourne AVANT l'application du gate).
        while next < probs.len() && probs[next].1 <= pos + n {
            speech = probs[next].0 >= cfg.vad_open_threshold;
            next += 1;
        }
        gate.process_block(speech, &mut gains[pos..pos + n]);
        pos += n;
    }
    gains
}

/// Gate ORACLE — non causal (référence des démos offline) : on connaît toutes les
/// décisions à l'avance, on ouvre `pad_before` AVANT le début de parole et on tient
/// `pad_after` APRÈS, avec des rampes douces. Impossible en live tel quel : sert de
/// borne haute pour chiffrer ce que coûte la causalité.
fn oracle_gains(len: usize, probs: &[(f32, usize)], cfg: &IsolationConfig, pad_before_ms: f32, pad_after_ms: f32) -> Vec<f32> {
    let mut target = vec![0.0f32; len];
    let pad_b = (pad_before_ms / 1000.0 * SR as f32) as usize;
    let pad_a = (pad_after_ms / 1000.0 * SR as f32) as usize;
    for &(p, end) in probs {
        if p < cfg.vad_open_threshold {
            continue;
        }
        // La trame couvre [end - VAD_FRAME_48K, end[.
        let start = end.saturating_sub(VAD_FRAME_48K);
        let a = start.saturating_sub(pad_b);
        let b = (end + pad_a).min(len);
        target[a..b].iter_mut().for_each(|g| *g = 1.0);
    }
    // Rampes douces (mêmes constantes que le gate causal, appliquées sur la cible
    // déjà « pré-ouverte » → pas de retard).
    let mut g = 0.0f32;
    let att = 1.0 - (-1.0f32 / (cfg.gate.attack_ms.max(0.1) / 1000.0 * SR as f32)).exp();
    let rel = 1.0 - (-1.0f32 / (cfg.gate.release_ms.max(0.1) / 1000.0 * SR as f32)).exp();
    for t in target.iter_mut() {
        let c = if *t > g { att } else { rel };
        g += (*t - g) * c;
        *t = g.clamp(0.0, 1.0);
    }
    target
}

/// Compte les « trous » du gate causal *pendant* que l'oracle est ouvert
/// (= coupures perçues sur de la voix) et l'énergie de voix perdue.
struct Holes {
    count: usize,
    total_ms: f32,
    longest_ms: f32,
    lost_db: f32,
}

fn analyse_holes(denoised: &[f32], causal: &[f32], oracle: &[f32]) -> Holes {
    let mut count = 0usize;
    let mut total = 0usize;
    let mut longest = 0usize;
    let mut run = 0usize;
    let mut e_oracle = 0.0f64;
    let mut e_causal = 0.0f64;
    for i in 0..denoised.len() {
        let (o, c) = (oracle[i], causal[i]);
        let s = denoised[i] as f64;
        if o > 0.5 {
            e_oracle += (s * o as f64) * (s * o as f64);
            e_causal += (s * c as f64) * (s * c as f64);
            if c < 0.5 * o {
                total += 1;
                run += 1;
                longest = longest.max(run);
                continue;
            }
        }
        if run > 0 {
            count += 1;
            run = 0;
        }
    }
    if run > 0 {
        count += 1;
        longest = longest.max(run);
    }
    let ratio = if e_oracle > 0.0 { (e_causal / e_oracle) as f32 } else { 1.0 };
    Holes {
        count,
        total_ms: total as f32 / SR as f32 * 1000.0,
        longest_ms: longest as f32 / SR as f32 * 1000.0,
        lost_db: 10.0 * ratio.max(1e-12).log10(),
    }
}


/// Paramètres d'une variante de gate à évaluer (banc de réglage hors-ligne).
struct Variant {
    name: &'static str,
    /// Retard appliqué à l'audio avant le gate (le gate « voit venir » la parole).
    lookahead_ms: f32,
    /// Seuil d'OUVERTURE (proba VAD) et seuil de MAINTIEN (hystérésis).
    open_thresh: f32,
    close_thresh: f32,
    attack_ms: f32,
    release_ms: f32,
    hangover_ms: f32,
}

/// Gate causal paramétrable avec hystérésis + lookahead (le lookahead se simule en
/// décalant les gains : appliquer `gains[i + L]` à l'échantillon `i` ⇔ retarder
/// l'audio de `L` avant le gate).
fn variant_gains(len: usize, probs: &[(f32, usize)], v: &Variant) -> Vec<f32> {
    let params = GateParams { attack_ms: v.attack_ms, release_ms: v.release_ms, hangover_ms: v.hangover_ms };
    let mut gate = VoiceGate::new(SR as f32, params);
    let mut raw = vec![0.0f32; len];
    let mut speech = false;
    let mut next = 0usize;
    let mut pos = 0usize;
    while pos < len {
        let n = BLOCK.min(len - pos);
        while next < probs.len() && probs[next].1 <= pos + n {
            let p = probs[next].0;
            // Hystérésis : on OUVRE au-dessus de `open_thresh`, on MAINTIENT tant
            // qu'on reste au-dessus de `close_thresh` (anti-papillotement en mot).
            speech = if speech { p >= v.close_thresh } else { p >= v.open_thresh };
            next += 1;
        }
        gate.process_block(speech, &mut raw[pos..pos + n]);
        pos += n;
    }
    let l = (v.lookahead_ms / 1000.0 * SR as f32) as usize;
    if l == 0 {
        return raw;
    }
    let mut out = vec![0.0f32; len];
    for i in 0..len {
        out[i] = raw[(i + l).min(len - 1)];
    }
    out
}

/// Retard d'ouverture par rapport au VRAI début acoustique + fuite hors parole.
fn score_variant(denoised: &[f32], gains: &[f32], onsets: &[usize], speech_mask: &[bool]) -> (Vec<f32>, f32, f32) {
    let mut delays = Vec::new();
    for &start in onsets {
        let mut d = None;
        let window = &gains[start..(start + SR).min(gains.len())];
        for (k, g) in window.iter().enumerate() {
            if *g > 0.9 {
                d = Some(k as f32 / SR as f32 * 1000.0);
                break;
            }
        }
        delays.push(d.unwrap_or(1000.0));
    }
    // Fuite : RMS de la sortie hors des zones de parole (bruit/instrument qui passe).
    // On mesure LOIN de la parole (≥ 1 s) — c'est là que vit la règle produit
    // « je joue et je ne parle pas ⇒ rien ne sort ».
    let guard = SR; // 1 s de garde autour de chaque zone de parole
    let mut far = vec![false; denoised.len()];
    {
        let mut last_speech: i64 = -(guard as i64) - 1;
        let mut next_speech = vec![usize::MAX; denoised.len()];
        let mut nxt = usize::MAX;
        for i in (0..denoised.len()).rev() {
            if speech_mask[i] {
                nxt = i;
            }
            next_speech[i] = nxt;
        }
        for i in 0..denoised.len() {
            if speech_mask[i] {
                last_speech = i as i64;
            }
            let after_ok = (i as i64 - last_speech) > guard as i64;
            let before_ok = next_speech[i] == usize::MAX || next_speech[i] - i > guard;
            far[i] = !speech_mask[i] && after_ok && before_ok;
        }
    }
    let mut leak = 0.0f64;
    let mut n_leak = 0usize;
    let mut open_out = 0usize;
    for i in 0..denoised.len() {
        if far[i] {
            let y = (denoised[i] * gains[i]) as f64;
            leak += y * y;
            n_leak += 1;
            if gains[i] > 0.5 {
                open_out += 1;
            }
        }
    }
    let leak_rms = if n_leak > 0 { (leak / n_leak as f64).sqrt() as f32 } else { 0.0 };
    let open_pct = if n_leak > 0 { open_out as f32 / n_leak as f32 * 100.0 } else { 0.0 };
    (delays, leak_rms, open_pct)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert!(args.len() >= 3, "usage: iso_offline <in.wav> <out_dir> [prefix]");
    let (inp, outdir) = (&args[1], &args[2]);
    let prefix = args.get(3).cloned().unwrap_or_else(|| "iso".to_string());
    let cfg = IsolationConfig::default();

    let input = read_wav_mono48(inp);
    let denoised = denoise_all(&input);
    let probs_clean = vad_probs(&denoised);
    let probs_raw = vad_probs(&input);

    let causal = causal_gains(denoised.len(), &probs_clean, &cfg);
    let oracle = oracle_gains(denoised.len(), &probs_clean, &cfg, 250.0, 300.0);

    let chain: Vec<f32> = denoised.iter().zip(&causal).map(|(s, g)| s * g).collect();
    let orac: Vec<f32> = denoised.iter().zip(&oracle).map(|(s, g)| s * g).collect();

    let n_speech = probs_clean.iter().filter(|(p, _)| *p >= cfg.vad_open_threshold).count();
    let n_speech_raw = probs_raw.iter().filter(|(p, _)| *p >= cfg.vad_open_threshold).count();
    let holes = analyse_holes(&denoised, &causal, &oracle);

    println!("── Chaîne d'isolation talkback — diagnostic hors-ligne ──");
    println!("durée              : {:.1} s", input.len() as f32 / SR as f32);
    println!("RMS entrée         : {:.4} ({:.1} dBFS)", rms(&input), db(rms(&input)));
    println!("RMS denoise seul   : {:.4} ({:.1} dBFS, {:.0} % du niveau)", rms(&denoised), db(rms(&denoised)), rms(&denoised) / (rms(&input) + 1e-9) * 100.0);
    println!("RMS chaîne (causal): {:.4} ({:.1} dBFS)", rms(&chain), db(rms(&chain)));
    println!("RMS chaîne (oracle): {:.4} ({:.1} dBFS)", rms(&orac), db(rms(&orac)));
    println!(
        "VAD sur voix NETTOYÉE : {n_speech}/{} trames parole (seuil {:.2}) | sur voix BRUTE : {n_speech_raw}/{}",
        probs_clean.len(), cfg.vad_open_threshold, probs_raw.len()
    );
    println!(
        "gate causal vs oracle : {} trous pendant la voix, {:.0} ms cumulés (plus long {:.0} ms), énergie voix perdue {:.2} dB",
        holes.count, holes.total_ms, holes.longest_ms, holes.lost_db
    );
    println!(
        "ballistique actuelle  : attaque {} ms / relâche {} ms / hangover {} ms, seuil VAD {:.2}, latence de décision ≤ {:.0} ms (trame VAD)",
        cfg.gate.attack_ms, cfg.gate.release_ms, cfg.gate.hangover_ms, cfg.vad_open_threshold,
        VAD_FRAME_48K as f32 / SR as f32 * 1000.0
    );

    write_wav(&format!("{outdir}/{prefix}_denoise.wav"), &denoised);
    write_wav(&format!("{outdir}/{prefix}_chain.wav"), &chain);
    write_wav(&format!("{outdir}/{prefix}_oracle.wav"), &orac);

    // Trace CSV (une ligne par bloc de 10 ms) — proba VAD en vigueur, gains, RMS.
    let mut csv = String::from("t_s,vad_prob,gain_causal_min,gain_causal_max,gain_oracle,rms_denoise\n");
    let mut next = 0usize;
    let mut cur_p = 0.0f32;
    for (bi, chunk) in denoised.chunks(BLOCK).enumerate() {
        let pos = bi * BLOCK;
        while next < probs_clean.len() && probs_clean[next].1 <= pos + chunk.len() {
            cur_p = probs_clean[next].0;
            next += 1;
        }
        let gc = &causal[pos..pos + chunk.len()];
        let go = &oracle[pos..pos + chunk.len()];
        csv.push_str(&format!(
            "{:.3},{:.3},{:.3},{:.3},{:.3},{:.5}\n",
            pos as f32 / SR as f32,
            cur_p,
            gc.iter().cloned().fold(f32::INFINITY, f32::min),
            gc.iter().cloned().fold(0.0, f32::max),
            go.iter().cloned().fold(0.0, f32::max),
            rms(chunk)
        ));
    }
    std::fs::write(format!("{outdir}/{prefix}_trace.csv"), csv).unwrap();
    eprintln!("écrit {outdir}/{prefix}_trace.csv");

    // ── Écrêtage : que rend le denoise selon le NIVEAU d'entrée ? ────────────
    // Terrain 03/09 : `WARN df::tract: Possible clipping detected (2.619)`, soit
    // +8,4 dB au-dessus du plein échelle, sur un simple micro-casque. On rejoue la
    // même voix à différents niveaux d'entrée pour voir d'où vient le dépassement.
    if std::env::var("ISO_PEAK_SWEEP").is_ok() {
        let pic_in = input.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        println!("\n── Écrêtage en sortie de denoise selon le niveau d'entrée ──");
        println!("pic du fichier source : {:.3} ({:.1} dBFS)", pic_in, db(pic_in));
        println!("{:<12} {:>10} {:>10} {:>9} {:>12} {:>15}", "entrée", "pic in", "pic out", "gain", "éch. > 1.0", "fidélité/réf");
        // Référence : la même voix traitée DANS la plage (−6 dBFS), remise à l'échelle.
        let ref_k = 10f32.powf(-6.0 / 20.0) / pic_in.max(1e-9);
        let ref_in: Vec<f32> = input.iter().map(|x| x * ref_k).collect();
        let ref_out = denoise_with(&ref_in, DenoiseParams::default());
        for cible_db in [-18.0f32, -12.0, -6.0, -3.0, -1.0, 0.0, 6.0, 8.4] {
            let cible = 10f32.powf(cible_db / 20.0);
            let k = cible / pic_in.max(1e-9);
            let mis: Vec<f32> = input.iter().map(|x| x * k).collect();
            let out = denoise_with(&mis, DenoiseParams::default());
            let pic_out = out.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            let n_clip = out.iter().filter(|x| x.abs() > 1.0).count();
            // Écart au traitement « dans la plage » (les deux ramenés au même niveau).
            let n = out.len().min(ref_out.len());
            let renorm = 10f32.powf(-6.0 / 20.0) / cible;
            let err: f32 = out[..n]
                .iter()
                .zip(&ref_out[..n])
                .map(|(a, b)| (a * renorm - b).powi(2))
                .sum::<f32>()
                / n as f32;
            let sig: f32 = ref_out[..n].iter().map(|x| x * x).sum::<f32>() / n as f32;
            println!(
                "{:>6.1} dBFS {:>10.3} {:>10.3} {:>8.1} dB {:>12} {:>12.1} dB",
                cible_db, cible, pic_out, db(pic_out) - db(cible), n_clip,
                10.0 * (sig / err.max(1e-20)).log10()
            );
        }
    }

    // ── Banc de seuils DENOISE (avant tout gate) ─────────────────────────────
    // Ces seuils commutent des étages ENTIERS du modèle trame par trame : c'est
    // la première chose à écouter quand « ça n'est pas propre » AVANT le gate.
    let denoise_variants = [
        ("libdefaut", DenoiseParams { min_snr_db: -10.0, max_erb_snr_db: 30.0, max_df_snr_db: 20.0, atten_lim_db: 100.0 }),
        ("cli", DenoiseParams::default()),
        ("souple", DenoiseParams { min_snr_db: -30.0, max_erb_snr_db: 35.0, max_df_snr_db: 35.0, atten_lim_db: 100.0 }),
    ];
    if std::env::var("ISO_DF_BENCH").is_ok() {
        println!("\n── Banc de seuils denoise (denoise SEUL, sans gate) ──");
        for (name, p) in &denoise_variants {
            let d = denoise_with(&input, *p);
            println!(
                "{name:<10} min={:>5} erb={:>4} df={:>4}  RMS={:.4} ({:.1} dBFS, {:.0} % du niveau)",
                p.min_snr_db, p.max_erb_snr_db, p.max_df_snr_db,
                rms(&d), db(rms(&d)), rms(&d) / (rms(&input) + 1e-9) * 100.0
            );
            write_wav(&format!("{outdir}/{prefix}_denoise_{name}.wav"), &d);
        }
    }

    // ── Banc de réglage : variantes candidates, chiffrées sur le même fichier ──
    // Vrais débuts acoustiques (référence perceptive) : premier échantillon d'un
    // segment de parole en remontant tant que le signal nettoyé reste au-dessus
    // d'un plancher relatif.
    let mut speech_mask = vec![false; denoised.len()];
    for &(p, end) in &probs_clean {
        if p >= cfg.vad_open_threshold {
            let start = end.saturating_sub(VAD_FRAME_48K);
            speech_mask[start..end.min(denoised.len())].iter_mut().for_each(|m| *m = true);
        }
    }
    let mut onsets = Vec::new();
    {
        let mut prev = false;
        let floor = rms(&denoised) * 0.05;
        for (i, &m) in speech_mask.iter().enumerate() {
            if m && !prev {
                // remonte au vrai début acoustique (bloc de 10 ms au-dessus du plancher)
                let mut s = i;
                while s >= BLOCK && rms(&denoised[s - BLOCK..s]) > floor {
                    s -= BLOCK;
                }
                onsets.push(s);
            }
            prev = m;
        }
    }
    let variants = [
        Variant { name: "actuel (prod)", lookahead_ms: 0.0, open_thresh: 0.5, close_thresh: 0.5, attack_ms: 10.0, release_ms: 120.0, hangover_ms: 250.0 },
        Variant { name: "look 32 + hyst", lookahead_ms: 32.0, open_thresh: 0.5, close_thresh: 0.35, attack_ms: 5.0, release_ms: 150.0, hangover_ms: 400.0 },
        Variant { name: "look 48 + hyst", lookahead_ms: 48.0, open_thresh: 0.5, close_thresh: 0.35, attack_ms: 5.0, release_ms: 150.0, hangover_ms: 400.0 },
        Variant { name: "look 64 + hyst", lookahead_ms: 64.0, open_thresh: 0.5, close_thresh: 0.35, attack_ms: 5.0, release_ms: 150.0, hangover_ms: 400.0 },
        Variant { name: "look 96 + hyst", lookahead_ms: 96.0, open_thresh: 0.5, close_thresh: 0.35, attack_ms: 5.0, release_ms: 150.0, hangover_ms: 400.0 },
        Variant { name: "look 128 + hyst", lookahead_ms: 128.0, open_thresh: 0.5, close_thresh: 0.35, attack_ms: 5.0, release_ms: 150.0, hangover_ms: 400.0 },
    ];
    println!("\n── Banc de réglage ({} onsets détectés) ──", onsets.len());
    println!("{:<28} {:>8} {:>8} {:>8} {:>9} {:>11} {:>9}", "variante", "retard", "médian", "max", ">30ms", "fuite dBFS", "ouvert%");
    let mut best: Option<(String, Vec<f32>)> = None;
    for v in &variants {
        let g = variant_gains(denoised.len(), &probs_clean, v);
        let (mut d, leak, open_pct) = score_variant(&denoised, &g, &onsets, &speech_mask);
        let mean = d.iter().sum::<f32>() / d.len().max(1) as f32;
        let over = d.iter().filter(|x| **x > 30.0).count();
        d.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med = d[d.len() / 2];
        let max = *d.last().unwrap();
        println!("{:<28} {:>6.0}ms {:>6.0}ms {:>6.0}ms {:>6}/{:<2} {:>10.1} {:>8.1}", v.name, mean, med, max, over, d.len(), db(leak), open_pct);
        if v.name == std::env::var("ISO_PICK").unwrap_or_else(|_| "look 64 + hyst".into()) {
            best = Some((v.name.to_string(), g));
        }
    }
    // ── Parité banc ↔ production ────────────────────────────────────────────
    // Le banc ne vaut que s'il décrit VRAIMENT ce que fait `VoiceIsolator`. On
    // rejoue donc la vraie chaîne et on compare à la variante qui reproduit ses
    // réglages par défaut.
    {
        let mut iso = VoiceIsolator::new(cfg).expect("modèles chargés");
        let mut reel = Vec::with_capacity(input.len());
        for chunk in input.chunks(BLOCK) {
            let mut b = chunk.to_vec();
            iso.process_block(&mut b).expect("process");
            reel.extend_from_slice(&b);
        }
        let defaut = Variant {
            name: "défauts production",
            lookahead_ms: cfg.lookahead_ms,
            open_thresh: cfg.vad_open_threshold,
            close_thresh: cfg.vad_close_threshold,
            attack_ms: cfg.gate.attack_ms,
            release_ms: cfg.gate.release_ms,
            hangover_ms: cfg.gate.hangover_ms,
        };
        let g = variant_gains(denoised.len(), &probs_clean, &defaut);
        let simule: Vec<f32> = denoised.iter().zip(&g).map(|(s, gg)| s * gg).collect();
        // La production RETARDE l'audio avant le gate ; le banc, lui, AVANCE les
        // gains (strictement équivalent, au décalage près) → on aligne de
        // `lookahead` avant de comparer.
        let l = (cfg.lookahead_ms / 1000.0 * SR as f32) as usize;
        let n = reel.len().saturating_sub(l).min(simule.len());
        let ecart = reel[l..l + n]
            .iter()
            .zip(&simule[..n])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        println!(
            "\nparité banc ↔ VoiceIsolator (réglages par défaut) : écart max {ecart:.6} ({:.1} dBFS) — latence ajoutée {:.0} ms",
            db(ecart),
            iso.added_latency_ms()
        );
    }

    if let Some((name, g)) = best {
        let fixed: Vec<f32> = denoised.iter().zip(&g).map(|(s, gg)| s * gg).collect();
        eprintln!("variante écrite pour écoute : {name}");
        write_wav(&format!("{outdir}/{prefix}_fix.wav"), &fixed);
    }
}

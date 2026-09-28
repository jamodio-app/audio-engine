//! Lot B0-bis du banc « N musiciens » (`PLAN-BANC-N-MUSICIENS-2026-09.md`) :
//! la réception à N flux, en temps SIMULÉ, sans carte son ni réseau.
//!
//! N flux réguliers passent par le vrai décodage (`decode_one_packet`) et le vrai
//! mélangeur, tirés par un callback simulé à cadence exacte (64 frames). Ce banc
//! ne voit PAS l'ordonnancement du système — c'est le rôle de `session-bench` sur
//! une vraie machine — mais il verrouille ce qui ne dépend que du code : à
//! conditions parfaites, le nombre de flux ne doit rien changer.
//!
//! Le second test est un CONSTAT, lancé à la main : il mesure ce que devient
//! chaque tampon quand un même hoquet de réception touche tous les flux à la
//! fois — le cas observé chez Guillaume le 26/09. Il ne juge pas : il chiffre ce
//! que les corrections du Lot B2 devront changer.

use super::*;
use jamodio_audio_core::codec::encoder::{MusicEncoder, MAX_PACKET_SIZE};
use std::time::{Duration, Instant};

/// Bloc de sortie mesuré sur nos deux plateformes : 64 frames = 1,333 ms.
const BLOCK_US: f64 = 64.0 * 1_000_000.0 / 48_000.0;
const FRAME_US: f64 = 2_500.0;

/// Une trame Opus réelle, réutilisée pour tous les paquets (le décodage coûte
/// autant qu'en vrai, l'encodage n'est fait qu'une fois).
fn payload() -> Vec<u8> {
    let enc = MusicEncoder::new().expect("encodeur Opus");
    let pcm: Vec<f32> = (0..enc.frame_size() * 2).map(|i| 0.2 * (i as f32 * 0.05).sin()).collect();
    let mut out = vec![0u8; MAX_PACKET_SIZE];
    let n = enc.encode(&pcm, &mut out).expect("encodage");
    out.truncate(n);
    out
}

fn packet(payload: &[u8], seq: u16) -> Vec<u8> {
    let header = RtpHeader {
        payload_type: 111,
        sequence: seq,
        timestamp: u32::from(seq).wrapping_mul(120),
        ssrc: 42,
        marker: false,
    };
    rtp::build_packet(&header, payload)
}

/// Résultat par flux après la simulation.
struct Outcome {
    underruns: Vec<u64>,
    targets_ms: Vec<usize>,
    /// Cible la plus haute atteinte pendant la simulation, par flux.
    peak_targets_ms: Vec<usize>,
    /// Plancher anti-trou (« glitch ») à la fin, par flux (ms).
    glitch_floor_ms: Vec<f64>,
}

/// Simule `secs` secondes à `n` flux. `stall` : à cet instant, aucun paquet
/// n'est remis au décodage pendant la durée donnée, POUR TOUS LES FLUX à la fois
/// (hoquet local de la réception), puis tout ce qui a attendu arrive d'un coup.
fn simulate(n: usize, secs: f64, stall: Option<(f64, f64)>) -> Outcome {
    let mixer = Arc::new(AudioMixer::new());
    let stats = Arc::new(Mutex::new(HashMap::new()));
    let recv_path = Arc::new(Mutex::new(Histogram::new(16)));
    let payload = payload();
    let ids: Vec<String> = (0..n).map(|i| format!("peer-{i}")).collect();
    let mut states: Vec<DecodeState> = ids
        .iter()
        .map(|id| {
            mixer.add_stream(id, StreamKind::Instrument);
            DecodeState::new(id, 1, StreamKind::Instrument).expect("décodeur Opus")
        })
        .collect();
    let base = Instant::now();
    let at = |us: f64| base + Duration::from_secs_f64(us / 1e6);
    let mut block = vec![0.0f32; 64 * 2];
    let (mut next_frame_us, mut seq) = (0.0f64, 0u16);
    let mut t = 0.0f64;
    let mut peaks = vec![0usize; n];
    let mut blocks = 0u64;
    while t < secs * 1e6 {
        // Paquets dus : tous les flux au même rythme. Pendant le hoquet, ils
        // attendent ; à sa fin, ils sont remis d'un coup, horodatés à la sortie
        // du hoquet (c'est le fil de réception qui les lit en retard).
        let blocked_until = stall.map(|(start, len)| (start * 1e6, (start + len) * 1e6));
        while next_frame_us <= t {
            if let Some((s, e)) = blocked_until {
                if next_frame_us >= s && t < e {
                    break;
                }
            }
            let recv_us = blocked_until.map_or(next_frame_us, |(s, e)| {
                if next_frame_us >= s && next_frame_us < e { e } else { next_frame_us }
            });
            let pkt = packet(&payload, seq);
            for (st, id) in states.iter_mut().zip(&ids) {
                decode_one_packet(st, id, at(recv_us), &pkt, &mixer, &stats, &recv_path, BLOCK_US / 1000.0);
            }
            seq = seq.wrapping_add(1);
            next_frame_us += FRAME_US;
        }
        mixer.mix_into(&mut block);
        t += BLOCK_US;
        blocks += 1;
        // Relevé de la cible toutes les ~13 ms (hors du tirage simulé).
        if blocks.is_multiple_of(10) {
            for p in mixer.stream_perf_stats() {
                if let Some(i) = ids.iter().position(|id| *id == p.producer_id) {
                    peaks[i] = peaks[i].max(p.target_ms);
                }
            }
        }
    }
    let perf = mixer.stream_perf_stats();
    let by_id = |id: &String| perf.iter().find(|p| &p.producer_id == id).expect("flux présent");
    Outcome {
        underruns: ids.iter().map(|id| by_id(id).underruns).collect(),
        targets_ms: ids.iter().map(|id| by_id(id).target_ms).collect(),
        peak_targets_ms: peaks,
        glitch_floor_ms: ids.iter().map(|id| by_id(id).target_glitch_ms).collect(),
    }
}

/// Garde-fou : de 1 à 8 flux réguliers, aucun trou, et la même cible pour tous
/// et à tous les N. Si un jour la charge du code dépend du nombre de flux (un
/// verrou commun, un coût qui s'additionne dans le tirage), ce test le dira.
#[test]
fn a_n_flux_reguliers_aucun_trou_et_la_meme_cible_quel_que_soit_n() {
    let reference = simulate(1, 6.0, None);
    assert_eq!(reference.underruns, vec![0], "un flux régulier ne fait aucun trou");
    for n in 2..=8 {
        let o = simulate(n, 6.0, None);
        assert!(o.underruns.iter().all(|&u| u == 0), "{n} flux : trous {:?}", o.underruns);
        assert!(
            o.targets_ms.iter().all(|&t| t == reference.targets_ms[0]),
            "{n} flux : cibles {:?} au lieu de {} ms",
            o.targets_ms,
            reference.targets_ms[0]
        );
    }
}

/// CONSTAT (lancé à la main) : un hoquet de réception de 15 ms qui touche tous
/// les flux à la fois. Chaque tampon le prend pour un défaut de SON lien et
/// grossit seul. `cargo test -p jamodio-agent constat_hoquet -- --ignored --nocapture`
#[test]
#[ignore = "constat chiffré, lancé à la main (cf. doc du module)"]
fn constat_hoquet_commun_a_tous_les_flux() {
    println!("\n  N | trous par flux | cible au plus haut (ms) | cible 4 s après (ms) | plancher anti-trou restant (ms)");
    for n in [1, 2, 4, 8] {
        let o = simulate(n, 8.0, Some((4.0, 0.015)));
        println!(
            "  {n} | {:?} | {:?} | {:?} | {:?}",
            o.underruns, o.peak_targets_ms, o.targets_ms, o.glitch_floor_ms
        );
    }
}

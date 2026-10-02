//! Chantier P1 (02/10/2026) — une perte ne doit pas coûter de latence.
//!
//! Au banc (rafales de pertes de longueur fixe, 2 machines), des rafales de 4 à
//! 6 paquets perdus portaient la cible du tampon de 5 à 9-19 ms : le tirage de
//! la sortie monte la cible à chaque trou, sans savoir si le paquet manquant est
//! perdu ou en retard. Ces tests verrouillent la décision prise ensuite par le
//! fil de réception, avec le vrai mélangeur, un vrai trou et le vrai suivi de
//! séquence : rendre la montée d'un trou de perte AVÉRÉE, garder celle d'un
//! retard ou d'une cause locale.

use super::*;
use std::time::{Duration, Instant};

const ID: &str = "pair-p1";
const BLOCK_MS: f64 = 64.0 * 1000.0 / 48_000.0;

/// Un flux qui a fait UN trou : la sortie a vidé le tampon. Rend l'état de
/// décodage (paquets 0 à 9 vus, dernier push relevé AVANT le trou), le rapport
/// du push qui révèle le trou, et l'instant de ce push.
fn stream_with_a_hole(mixer: &AudioMixer) -> (DecodeState, PushReport, Instant) {
    mixer.add_stream(ID, StreamKind::Instrument);
    let mut st = DecodeState::new(ID, 1, StreamKind::Instrument).expect("décodeur Opus");
    for seq in 0..10u16 {
        st.seq.on_packet(seq);
    }
    // 10 ms poussés (cible de départ) : le tampon s'amorce, la sortie le vide.
    let start = Instant::now();
    mixer.push_samples(ID, &vec![0.1f32; 960]).expect("push");
    st.last_push = Some(PushMark {
        // Le push d'avant le trou, 20 ms plus tôt : rien n'a été consommé plus
        // vite que le temps (sinon le trou serait classé « consommation »).
        at: start.checked_sub(Duration::from_millis(20)).expect("horloge"),
        fill_after_ms: 10.0,
        read_index: 0,
        last_received: start.checked_sub(Duration::from_millis(20)),
        lost: st.seq.counters().lost(),
    });
    let mut block = vec![0.0f32; 128];
    for _ in 0..10 {
        mixer.mix_into(&mut block);
    }
    let report = mixer.push_samples(ID, &[0.0f32; 240]).expect("push");
    assert!(report.hole.is_some_and(|h| !h.growth.is_none()), "un trou, avec sa montée relevée");
    (st, report, Instant::now())
}

/// Cible du flux : (anti-trou, filet réactif), en ms.
fn growth_ms(mixer: &AudioMixer) -> (f64, f64) {
    let p = mixer.stream_perf_stats().into_iter().find(|p| p.producer_id == ID).expect("flux");
    (p.target_glitch_ms, p.target_reactive_ms)
}

/// Le push qui révèle le trou : le paquet 16 arrive APRÈS le trou (6 manquent).
fn reveal(st: &mut DecodeState, mixer: &AudioMixer, report: &PushReport, at: Instant) {
    st.seq.on_packet(16);
    note_push(st, mixer, ID, report, at, Some(Arrived { read_at: at, in_system_at: None }), BLOCK_MS);
}

/// Un push ordinaire plus tard (aucun trou) : c'est là que se tranche l'attente.
fn later_push(st: &mut DecodeState, mixer: &AudioMixer, at: Instant) {
    let report = mixer.push_samples(ID, &[0.0f32; 240]).expect("push");
    note_push(st, mixer, ID, &report, at, Some(Arrived { read_at: at, in_system_at: None }), BLOCK_MS);
}

#[test]
fn une_perte_averee_rend_la_montee_de_cible() {
    let mixer = AudioMixer::new();
    let (mut st, report, t) = stream_with_a_hole(&mixer);
    reveal(&mut st, &mixer, &report, t);
    assert_eq!(st.holes.arrival, 1);
    let (glitch, reactive) = growth_ms(&mixer);
    assert!(glitch > 0.0 && reactive > 0.0, "la sortie a monté la cible : {glitch} / {reactive}");
    // Avant la fin de la fenêtre : rien n'est tranché.
    later_push(&mut st, &mixer, t + LOSS_PROOF / 2);
    assert_eq!(st.holes.loss_refunded, 0);
    // Fenêtre passée, aucun manquant arrivé : perte avérée, montée rendue.
    later_push(&mut st, &mixer, t + LOSS_PROOF + Duration::from_millis(1));
    assert_eq!(st.holes.loss_refunded, 1);
    assert_eq!(growth_ms(&mixer), (0.0, 0.0), "montée rendue");
}

#[test]
fn un_paquet_arrive_en_retard_garde_la_montee() {
    let mixer = AudioMixer::new();
    let (mut st, report, t) = stream_with_a_hole(&mixer);
    reveal(&mut st, &mixer, &report, t);
    let before = growth_ms(&mixer);
    // Un des manquants finit par arriver : c'était un RETARD.
    assert_eq!(st.seq.on_packet(12), Arrival::Late);
    later_push(&mut st, &mixer, t + LOSS_PROOF + Duration::from_millis(1));
    assert_eq!(st.holes.loss_refunded, 0);
    assert_eq!(growth_ms(&mixer), before, "un retard garde sa montée");
}

#[test]
fn un_trou_de_cause_locale_garde_sa_montee() {
    let mixer = AudioMixer::new();
    let (mut st, report, t) = stream_with_a_hole(&mixer);
    st.seq.on_packet(16);
    // Le paquet suivant avait été LU avant le trou : le décodage était en retard.
    let h = report.hole.expect("trou");
    let read_before = h.at.checked_sub(Duration::from_millis(1)).expect("horloge");
    note_push(&mut st, &mixer, ID, &report, t, Some(Arrived { read_at: read_before, in_system_at: None }), BLOCK_MS);
    assert_eq!((st.holes.arrival, st.holes.decode), (0, 1));
    assert!(st.loss_refunds.is_empty(), "une cause locale n'est jamais rendue");
    let before = growth_ms(&mixer);
    later_push(&mut st, &mixer, t + LOSS_PROOF + Duration::from_millis(1));
    assert_eq!(growth_ms(&mixer), before);
}

#[test]
fn un_trou_sans_paquet_manquant_n_attend_rien() {
    let mixer = AudioMixer::new();
    let (mut st, report, t) = stream_with_a_hole(&mixer);
    // Le paquet suivant arrive en retard mais DANS L'ORDRE : aucun ne manque.
    st.seq.on_packet(10);
    note_push(&mut st, &mixer, ID, &report, t, Some(Arrived { read_at: t, in_system_at: None }), BLOCK_MS);
    assert_eq!(st.holes.arrival, 1);
    assert!(st.loss_refunds.is_empty(), "rien ne manquait : un retard, la montée reste");
}

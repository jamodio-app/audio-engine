//! Sonde diagnostic FINE du cold-start ASIO (bug « entrée figée/railée » 2026-07-03).
//!
//! # But
//!
//! Comprendre *pourquoi* le canal 0 (tranche instrument) d'un Focusrite USB peut
//! délivrer, au TOUT PREMIER démarrage ASIO à froid, une constante quasi
//! pleine-échelle bit-exacte re-servie à chaque callback (DMA d'entrée figé),
//! alors que le MÊME buffer (64) fonctionne quand le driver est chaud. Le verdict
//! agrégé de l'ancienne sonde disait « railé/vivant » mais ne montrait pas
//! l'ÉVOLUTION : à quel instant exact ch0 bascule silence → figé → railé, sur
//! quelle valeur, combien de temps après l'ouverture. Cette sonde produit cette
//! **timeline**.
//!
//! # Garanties
//!
//! - **Observation PURE** : ne touche NI le signal NI le routage. N'émet que des
//!   logs `tracing`.
//! - **ASIO/Windows uniquement** : instanciée seulement sur le chemin ASIO
//!   (`capture.rs`, `on_asio.then(..)`). macOS/CoreAudio et WASAPI : jamais créée,
//!   coût nul, comportement byte-identique.
//! - **Hot-path RT propre** : le callback audio (`feed`) ne fait QUE des atomiques
//!   (aucune allocation, aucun log, aucun lock). Toute l'émission `tracing` (les
//!   snapshots périodiques + le verdict) est faite par un thread de flush dédié,
//!   HORS du thread audio. Le thread s'auto-termine à la fin de fenêtre ou dès que
//!   le stream est fermé (la sonde n'est plus référencée que par un `Weak`).
//!
//! # Ce qu'on ne peut PAS mesurer ici
//!
//! Côté agent on passe par cpal, dont le callback ne fournit PAS l'index de
//! double-buffer ASIO (0/1) : on ne peut donc pas dire « quelle moitié est figée »
//! — ça se mesure au banc `jamodio-asio-lab` (asio-sys direct, `info.buffer_index`).

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

/// |max| en-dessous duquel un bloc est « silence » (≈ -80 dBFS). Aussi le seuil de
/// déclenchement du dump brut (1er bloc « signal »). `pub(crate)` : lu par capture.rs.
pub(crate) const SILENCE_THRESH: f32 = 1e-4;
/// Nombre de samples ch0 capturés pour le dump brut (32 × i32 = 128 octets ≈ une
/// ligne de texte si le buffer contient de la mémoire parasite).
const RAW_N: usize = 32;
/// Nombre de canaux d'entrée dont on suit la vivacité (discrimine « canal instrument
/// seul figé » vs « TOUTES les entrées figées » = armement global de l'ADC).
const N_CH: usize = 4;
/// |max| au-dessus duquel ch0 est « railé » (un instrument sain n'atteint pas ça
/// sans écrêter de toute façon).
const RAILED_THRESH: f32 = 0.95;
/// Nombre de blocs consécutifs SANS changement du 1er sample ch0 avant de conclure
/// « FIGÉ » (DMA re-servant le même buffer). 8 blocs @64/48k ≈ 10,7 ms — un signal
/// réel ne produit pas 8 blocs i32 bit-exacts d'affilée.
const FROZEN_RUN: u32 = 8;
/// Cadence des snapshots de la timeline (thread de flush).
const SNAPSHOT: Duration = Duration::from_millis(250);
/// Sous ce ratio de blocs « vivants » sur toute la fenêtre, le verdict est FIGÉ.
const LIVE_RATIO_FROZEN: f64 = 0.05;

// Régimes du bloc courant (publiés dans `regime`).
const R_SILENCE: u8 = 0;
const R_LIVE: u8 = 1;
const R_FROZEN: u8 = 2;

fn regime_str(r: u8) -> &'static str {
    match r {
        R_SILENCE => "SILENCE",
        R_LIVE => "VIVANT",
        R_FROZEN => "FIGÉ",
        _ => "?",
    }
}

/// Sentinelle « pas encore survenu » pour les frames d'un événement one-shot.
const NEVER: u64 = u64::MAX;

/// Sonde cold-start. Créée via [`ColdStartProbe::spawn`] (qui lance le thread de
/// flush) sur le chemin ASIO. Le callback audio appelle [`ColdStartProbe::feed`].
pub struct ColdStartProbe {
    sr: u32,
    window_frames: u64,

    // --- publié vers le thread de flush ---
    /// Fenêtre close : court-circuite `feed` et arrête le thread.
    done: AtomicBool,
    /// Frames/canal cumulés depuis l'ouverture (base de temps).
    frames: AtomicU64,
    /// Blocs observés (dénominateur des ratios).
    blocks: AtomicU64,
    /// Blocs où ch0[0] a changé vs le précédent (vivacité).
    changes: AtomicU64,
    /// |max| ch0 cumulé sur toute la fenêtre (ppm de pleine échelle) — pour le verdict.
    abs_max_ppm: AtomicU64,
    /// |max| ch0 de la tranche courante (ppm) — le thread le `swap(0)` à chaque snapshot.
    abs_win_ppm: AtomicU64,
    /// Régime du dernier bloc.
    regime: AtomicU8,
    /// 1er sample ch0 du dernier bloc (affiché en hexa dans la timeline).
    last_key0: AtomicI64,

    // --- vivacité PAR CANAL (jusqu'à N_CH) : répond « instrument seul figé, ou toutes
    //     les entrées ? ». Producteur unique (thread audio). ---
    chan_prev: [AtomicI64; N_CH],
    chan_changes: [AtomicU64; N_CH],
    chan_absmax_ppm: [AtomicU64; N_CH],

    // --- état inter-blocs (producteur unique = thread audio) ---
    prev0: AtomicI64,
    unchanged_run: AtomicU32,

    // --- signatures one-shot (frame de survenue + valeur) ---
    first_signal_frame: AtomicU64,
    first_frozen_frame: AtomicU64,
    first_frozen_key0: AtomicI64,
    first_railed_frame: AtomicU64,
    first_railed_key0: AtomicI64,

    // --- dump brut PÉRIODIQUE de ch0 (handshake flush↔callback ; loggé sur
    //     changement) : révèle le CONTENU du buffer d'entrée et son évolution —
    //     0xFF figé = buffer jamais écrit par l'ADC ; texte lisible = mémoire parasite ---
    /// Snapshot demandé par le thread de flush (incrémenté à chaque dump).
    snap_seq: AtomicU64,
    /// Dernier snapshot rempli par le callback (== `snap_seq` quand prêt à lire).
    snap_done: AtomicU64,
    /// Frame du snapshot courant.
    raw_frame: AtomicU64,
    /// Adresse du buffer d'où provient ch0.
    raw_addr: AtomicU64,
    /// Les `RAW_N` premiers samples ch0 du snapshot.
    raw: [AtomicU32; RAW_N],
}

impl ColdStartProbe {
    /// Construit la sonde SANS thread (usage interne / tests). Préférer [`spawn`].
    fn new(sr: u32, window_frames: u64) -> Self {
        Self {
            sr: sr.max(1),
            window_frames: window_frames.max(1),
            done: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            blocks: AtomicU64::new(0),
            changes: AtomicU64::new(0),
            abs_max_ppm: AtomicU64::new(0),
            abs_win_ppm: AtomicU64::new(0),
            regime: AtomicU8::new(R_SILENCE),
            last_key0: AtomicI64::new(0),
            prev0: AtomicI64::new(i64::MIN),
            unchanged_run: AtomicU32::new(0),
            chan_prev: std::array::from_fn(|_| AtomicI64::new(i64::MIN)),
            chan_changes: std::array::from_fn(|_| AtomicU64::new(0)),
            chan_absmax_ppm: std::array::from_fn(|_| AtomicU64::new(0)),
            first_signal_frame: AtomicU64::new(NEVER),
            first_frozen_frame: AtomicU64::new(NEVER),
            first_frozen_key0: AtomicI64::new(0),
            first_railed_frame: AtomicU64::new(NEVER),
            first_railed_key0: AtomicI64::new(0),
            snap_seq: AtomicU64::new(1),
            snap_done: AtomicU64::new(0),
            raw_frame: AtomicU64::new(0),
            raw_addr: AtomicU64::new(0),
            raw: std::array::from_fn(|_| AtomicU32::new(0)),
        }
    }

    /// Construit la sonde et lance son thread de flush. Le thread ne détient qu'un
    /// `Weak` : dès que le stream (donc le callback qui détient l'`Arc`) est fermé,
    /// le thread s'arrête au tick suivant.
    pub fn spawn(sr: u32, window_frames: u64) -> Arc<Self> {
        let probe = Arc::new(Self::new(sr, window_frames));
        let weak = Arc::downgrade(&probe);
        // Un thread std (pas tokio) : `build_capture_stream` est synchrone et sans
        // contexte tokio garanti. Négligeable : dort 250 ms sur ~10 s puis meurt.
        let spawned = std::thread::Builder::new()
            .name("cold-probe".into())
            .spawn(move || flush_loop(weak));
        if let Err(e) = spawned {
            tracing::warn!(target: "jamodio::capture", error = %e, "cold-probe: thread de flush non démarré (sonde inactive)");
        }
        probe
    }

    /// `true` tant que la fenêtre n'est pas close (le callback évite alors de
    /// scanner ch0).
    #[inline]
    pub fn active(&self) -> bool {
        !self.done.load(Ordering::Acquire)
    }

    /// `true` si le thread de flush attend un nouveau snapshot du contenu ch0 (le
    /// callback n'extrait ch0 que dans ce cas → ~1 remplissage par tick de flush).
    #[inline]
    pub fn wants_snapshot(&self) -> bool {
        self.snap_done.load(Ordering::Relaxed) != self.snap_seq.load(Ordering::Relaxed)
    }

    /// Remplit le snapshot demandé avec le contenu brut de ch0 (jusqu'à [`RAW_N`]
    /// samples) + l'adresse du buffer. Producteur unique (thread audio) : remplit
    /// `raw` PUIS publie `snap_done` (Release) — le thread de flush lit `snap_done`
    /// (Acquire) avant `raw`. No-op si le snapshot courant est déjà rempli.
    #[inline]
    pub fn fill_snapshot(&self, ch0: &[i32], addr: usize) {
        let seq = self.snap_seq.load(Ordering::Relaxed);
        if self.snap_done.load(Ordering::Relaxed) == seq {
            return;
        }
        for (slot, &s) in self.raw.iter().zip(ch0.iter()) {
            slot.store(s as u32, Ordering::Relaxed);
        }
        self.raw_addr.store(addr as u64, Ordering::Relaxed);
        self.raw_frame
            .store(self.frames.load(Ordering::Relaxed), Ordering::Relaxed);
        self.snap_done.store(seq, Ordering::Release);
    }

    /// Alimente la sonde avec les stats ch0 d'un bloc entrelacé. Appelé sur le
    /// thread audio : **atomiques uniquement**. `key0` = 1er sample ch0 bit-exact ;
    /// `abs_frac` = |max| ch0 normalisé [0,1+] ; `block_frames` = frames/canal.
    #[inline]
    pub fn feed(&self, key0: i64, abs_frac: f32, block_frames: usize) {
        if self.done.load(Ordering::Acquire) {
            return;
        }
        let prev = self.prev0.swap(key0, Ordering::Relaxed);
        let seen_prev = prev != i64::MIN;
        let changed = seen_prev && prev != key0;
        if changed {
            self.changes.fetch_add(1, Ordering::Relaxed);
        }
        // Compteur de blocs identiques consécutifs (DMA figé re-servant le buffer).
        let run = if seen_prev && prev == key0 {
            self.unchanged_run.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            self.unchanged_run.store(0, Ordering::Relaxed);
            0
        };
        self.blocks.fetch_add(1, Ordering::Relaxed);
        let ppm = (abs_frac.clamp(0.0, 4.0) * 1_000_000.0) as u64;
        self.abs_max_ppm.fetch_max(ppm, Ordering::Relaxed);
        self.abs_win_ppm.fetch_max(ppm, Ordering::Relaxed);
        self.last_key0.store(key0, Ordering::Relaxed);
        let total =
            self.frames.fetch_add(block_frames as u64, Ordering::Relaxed) + block_frames as u64;

        // Classe le régime du bloc.
        let regime = if run >= FROZEN_RUN {
            R_FROZEN
        } else if abs_frac < SILENCE_THRESH {
            R_SILENCE
        } else {
            R_LIVE
        };
        self.regime.store(regime, Ordering::Relaxed);

        // Signatures one-shot (première survenue → frame + valeur).
        if abs_frac >= SILENCE_THRESH {
            set_once_u64(&self.first_signal_frame, total);
        }
        if run == FROZEN_RUN && set_once_u64(&self.first_frozen_frame, total) {
            self.first_frozen_key0.store(key0, Ordering::Relaxed);
        }
        if abs_frac >= RAILED_THRESH && set_once_u64(&self.first_railed_frame, total) {
            self.first_railed_key0.store(key0, Ordering::Relaxed);
        }

        if total >= self.window_frames {
            // Le thread de flush lira `done` et émettra le verdict final.
            self.done.store(true, Ordering::Release);
        }
    }

    /// Suit la vivacité de chaque canal (jusqu'à [`N_CH`]) : `stats[c]` = `(1er sample
    /// bit-exact, |max| normalisé)` du canal `c`. Sert au verdict à distinguer un
    /// canal instrument seul figé d'un gel de TOUTES les entrées. No-op après la fin
    /// de fenêtre.
    #[inline]
    pub fn feed_channels(&self, stats: &[(i64, f32)]) {
        if self.done.load(Ordering::Acquire) {
            return;
        }
        for (c, &(key, abs_frac)) in stats.iter().take(N_CH).enumerate() {
            let prev = self.chan_prev[c].swap(key, Ordering::Relaxed);
            if prev != i64::MIN && prev != key {
                self.chan_changes[c].fetch_add(1, Ordering::Relaxed);
            }
            let ppm = (abs_frac.clamp(0.0, 4.0) * 1_000_000.0) as u64;
            self.chan_absmax_ppm[c].fetch_max(ppm, Ordering::Relaxed);
        }
    }

    /// Convertit un compteur de frames en millisecondes depuis l'ouverture.
    fn ms(&self, frames: u64) -> f64 {
        frames as f64 * 1000.0 / self.sr as f64
    }
}

/// compare-and-set one-shot sur sentinelle [`NEVER`]. `true` si CE call a posé la
/// valeur (producteur unique, mais garde la sémantique « première fois »).
#[inline]
fn set_once_u64(a: &AtomicU64, v: u64) -> bool {
    a.compare_exchange(NEVER, v, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

/// Boucle du thread de flush : émet la timeline (un snapshot par tranche
/// [`SNAPSHOT`]) puis le verdict final. S'arrête quand la sonde est droppée
/// (stream fermé) ou que la fenêtre est close.
fn flush_loop(weak: Weak<ColdStartProbe>) {
    let mut prev_blocks = 0u64;
    let mut prev_changes = 0u64;
    let mut prev_regime = u8::MAX;
    let (mut signal_logged, mut frozen_logged, mut railed_logged) = (false, false, false);
    let mut prev_dump = String::new();

    loop {
        std::thread::sleep(SNAPSHOT);
        let Some(p) = weak.upgrade() else {
            return; // stream fermé : plus personne ne détient la sonde → stop.
        };

        // Transitions précises (instants one-shot) dès qu'elles sont disponibles.
        log_transition(&p, &p.first_signal_frame, &mut signal_logged, |t_ms, _| {
            tracing::info!(target: "jamodio::capture", t_ms, "cold-probe: ch0 SIGNAL (sort du silence)");
        });
        log_transition(&p, &p.first_frozen_frame, &mut frozen_logged, |t_ms, pr| {
            let key0 = pr.first_frozen_key0.load(Ordering::Relaxed);
            tracing::warn!(target: "jamodio::capture", t_ms, key0_hex = format!("0x{:08X}", key0 as i32), "cold-probe: ch0 FIGÉ (DMA re-sert le même buffer)");
        });
        log_transition(&p, &p.first_railed_frame, &mut railed_logged, |t_ms, pr| {
            let key0 = pr.first_railed_key0.load(Ordering::Relaxed);
            tracing::warn!(target: "jamodio::capture", t_ms, key0_hex = format!("0x{:08X}", key0 as i32), "cold-probe: ch0 RAILÉ (quasi pleine-échelle)");
        });

        // Dump périodique du contenu brut de ch0 — loggé UNIQUEMENT quand il change
        // (buffer figé 0xFF = 1 ligne ; parasite/texte = évolution visible). Handshake
        // avec le callback : on lit quand `snap_done == snap_seq`, puis on redemande.
        if p.snap_done.load(Ordering::Acquire) == p.snap_seq.load(Ordering::Relaxed) {
            let ascii: String = p
                .raw
                .iter()
                .flat_map(|w| w.load(Ordering::Relaxed).to_le_bytes())
                .map(|b| if (0x20..=0x7e).contains(&b) { b as char } else { '.' })
                .collect();
            if ascii != prev_dump {
                let addr = p.raw_addr.load(Ordering::Relaxed);
                let frame = p.raw_frame.load(Ordering::Relaxed);
                let hex: String = p
                    .raw
                    .iter()
                    .map(|w| format!("{:08X}", w.load(Ordering::Relaxed)))
                    .collect::<Vec<_>>()
                    .join(" ");
                tracing::warn!(
                    target: "jamodio::capture",
                    t_ms = p.ms(frame),
                    buf_addr = format!("0x{addr:012X}"),
                    ascii = %ascii,
                    hex = %hex,
                    "cold-probe: ch0 CONTENU BRUT — texte lisible = mémoire parasite ; 0xFFFFFFFF = buffer non écrit par l'ADC"
                );
                prev_dump = ascii;
            }
            p.snap_seq.fetch_add(1, Ordering::Relaxed); // redemande un snapshot
        }

        if p.done.load(Ordering::Acquire) {
            emit_verdict(&p);
            return;
        }

        // Snapshot de tranche.
        let frames = p.frames.load(Ordering::Relaxed);
        let blocks = p.blocks.load(Ordering::Relaxed);
        let changes = p.changes.load(Ordering::Relaxed);
        let d_blocks = blocks - prev_blocks;
        let d_changes = changes - prev_changes;
        let abs_win = p.abs_win_ppm.swap(0, Ordering::Relaxed) as f64 / 1_000_000.0;
        let regime = p.regime.load(Ordering::Relaxed);
        let key0 = p.last_key0.load(Ordering::Relaxed);
        // N'émet que si des blocs sont arrivés (évite le bruit avant le 1er callback).
        if d_blocks > 0 {
            let live_ratio = d_changes as f64 / d_blocks as f64;
            tracing::info!(
                target: "jamodio::capture",
                t_ms = p.ms(frames),
                regime = regime_str(regime),
                blocks = d_blocks,
                live_ratio = format!("{:.0}%", live_ratio * 100.0),
                abs_max = format!("{abs_win:.4}"),
                key0_hex = format!("0x{:08X}", key0 as i32),
                "cold-probe: timeline"
            );
            // Souligne un changement de régime au fil de l'eau.
            if regime != prev_regime && prev_regime != u8::MAX {
                tracing::info!(
                    target: "jamodio::capture",
                    from = regime_str(prev_regime),
                    to = regime_str(regime),
                    t_ms = p.ms(frames),
                    "cold-probe: changement de régime ch0"
                );
            }
            prev_regime = regime;
        }
        prev_blocks = blocks;
        prev_changes = changes;
    }
}

/// Émet un log d'instant one-shot une seule fois, dès que sa frame est renseignée.
#[inline]
fn log_transition(
    p: &Arc<ColdStartProbe>,
    frame: &AtomicU64,
    logged: &mut bool,
    emit: impl Fn(f64, &Arc<ColdStartProbe>),
) {
    if *logged {
        return;
    }
    let f = frame.load(Ordering::Relaxed);
    if f != NEVER {
        emit(p.ms(f), p);
        *logged = true;
    }
}

/// Verdict final (fin de fenêtre) : reprend la classification agrégée + enrichit
/// avec les instants/valeurs des transitions.
fn emit_verdict(p: &Arc<ColdStartProbe>) {
    let changes = p.changes.load(Ordering::Relaxed);
    let blocks = p.blocks.load(Ordering::Relaxed).max(1);
    let abs_max = p.abs_max_ppm.load(Ordering::Relaxed) as f64 / 1_000_000.0;
    let live_ratio = changes as f64 / blocks as f64;
    // Un vrai wedge = entrée FIGÉE (ch0 ne varie quasiment pas). Un `abs_max` élevé
    // AVEC un `live_ratio` élevé n'est PAS un wedge : c'est un signal réel qui sature
    // (larsen du self-monitor, jeu fort). Ne pas confondre les deux.
    let frozen = live_ratio < LIVE_RATIO_FROZEN;
    let saturates = abs_max > RAILED_THRESH as f64;
    let frozen_at = p.first_frozen_frame.load(Ordering::Relaxed);
    let railed_at = p.first_railed_frame.load(Ordering::Relaxed);
    let frozen_ms = (frozen_at != NEVER).then(|| p.ms(frozen_at));
    let railed_ms = (railed_at != NEVER).then(|| p.ms(railed_at));

    // Vivacité par canal : « ch0:0%/0.000 ch1:98%/0.31 … » — un canal vivant pendant
    // que ch0 est figé pointe un problème de canal/routage, pas d'armement global.
    let per_chan: String = (0..N_CH)
        .map(|c| {
            let live = p.chan_changes[c].load(Ordering::Relaxed) as f64 / blocks as f64 * 100.0;
            let am = p.chan_absmax_ppm[c].load(Ordering::Relaxed) as f64 / 1_000_000.0;
            format!("ch{c}:{live:.0}%/{am:.3}")
        })
        .collect::<Vec<_>>()
        .join(" ");

    if frozen {
        // Vrai wedge : l'entrée ne varie pas (DMA figé). Sous-type par le niveau.
        let kind = if saturates {
            "RAILÉE (figée ~pleine-échelle)"
        } else {
            "FIGÉE/silence"
        };
        tracing::warn!(
            target: "jamodio::capture",
            changes, blocks,
            abs_max = format!("{abs_max:.4}"),
            live_ratio = format!("{:.1}%", live_ratio * 100.0),
            frozen_at_ms = frozen_ms,
            railed_at_ms = railed_ms,
            channels = %per_chan,
            "VERDICT cold-start ASIO : entrée {kind} — WEDGE (entrée non armée à froid, \
             nettoyée seulement par un ASIOInit frais / reset USB). Bug 2026-07-03."
        );
    } else if saturates {
        // Vivant MAIS sature : signal réel qui clippe (larsen self-monitor, jeu fort)
        // — PAS le wedge cold-start (l'entrée varie, live_ratio élevé).
        tracing::warn!(
            target: "jamodio::capture",
            changes, blocks,
            abs_max = format!("{abs_max:.4}"),
            live_ratio = format!("{:.1}%", live_ratio * 100.0),
            railed_at_ms = railed_ms,
            channels = %per_chan,
            "VERDICT cold-start ASIO : entrée VIVANTE mais SATURE (abs_max≈1, ch0 varie) — \
             probable larsen/self-monitor ou jeu fort, PAS le wedge cold-start figé."
        );
    } else {
        tracing::info!(
            target: "jamodio::capture",
            changes, blocks,
            abs_max = format!("{abs_max:.4}"),
            live_ratio = format!("{:.1}%", live_ratio * 100.0),
            channels = %per_chan,
            "VERDICT cold-start ASIO : entrée vivante au démarrage (ch0 varie, niveau sain)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // key0 arbitraire pour un i32 « railé » (top-byte ~0x7F = quasi +pleine-échelle).
    const RAIL_KEY: i64 = 0x7ECC_AD00;

    #[test]
    fn live_input_no_wedge() {
        let p = ColdStartProbe::new(48_000, 100);
        // Blocs vivants : key0 varie, niveau sain.
        for i in 0..50i64 {
            p.feed(1000 + i * 7, 0.3, 1);
        }
        assert_eq!(p.regime.load(Ordering::Relaxed), R_LIVE);
        assert_ne!(p.first_signal_frame.load(Ordering::Relaxed), NEVER);
        assert_eq!(p.first_frozen_frame.load(Ordering::Relaxed), NEVER, "aucun figeage");
        assert_eq!(p.first_railed_frame.load(Ordering::Relaxed), NEVER, "aucun rail");
    }

    #[test]
    fn frozen_railed_input_detected() {
        let p = ColdStartProbe::new(48_000, 10_000);
        // 5 blocs vivants puis DMA figé sur une valeur railée bit-exacte.
        for i in 0..5i64 {
            p.feed(2000 + i, 0.2, 64);
        }
        for _ in 0..40 {
            p.feed(RAIL_KEY, 0.99, 64);
        }
        // FIGÉ détecté après FROZEN_RUN blocs identiques.
        assert_eq!(p.regime.load(Ordering::Relaxed), R_FROZEN);
        assert_ne!(p.first_frozen_frame.load(Ordering::Relaxed), NEVER, "figeage détecté");
        assert_eq!(p.first_frozen_key0.load(Ordering::Relaxed), RAIL_KEY);
        assert_ne!(p.first_railed_frame.load(Ordering::Relaxed), NEVER, "rail détecté");
        // Le figeage survient APRÈS le 1er signal (5 blocs vivants d'abord).
        let sig = p.first_signal_frame.load(Ordering::Relaxed);
        let frz = p.first_frozen_frame.load(Ordering::Relaxed);
        assert!(sig < frz, "signal ({sig}) avant figeage ({frz})");
    }

    #[test]
    fn window_closes_after_target_frames() {
        let p = ColdStartProbe::new(48_000, 128);
        assert!(p.active());
        p.feed(1, 0.1, 64);
        assert!(p.active(), "fenêtre encore ouverte à 64/128 frames");
        p.feed(2, 0.1, 64);
        assert!(!p.active(), "fenêtre close à 128/128 frames");
        // feed après done = no-op (pas de panique, pas de compteur qui bouge).
        let blocks_before = p.blocks.load(Ordering::Relaxed);
        p.feed(3, 0.1, 64);
        assert_eq!(p.blocks.load(Ordering::Relaxed), blocks_before);
    }
}

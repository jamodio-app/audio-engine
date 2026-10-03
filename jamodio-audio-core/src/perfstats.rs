//! Histogrammes glissants pour l'instrumentation latence agent (Sprint S1).
//!
//! Deux rôles, séparés pour qu'AUCUN calcul ne se fasse sous le verrou que
//! partagent l'écrivain temps réel et le lecteur :
//!   - [`Histogram`] — côté écrivain (callback audio, fil de réception) :
//!     `observe(value_ms)` est O(1), sans allocation. Il vit derrière un
//!     `parking_lot::Mutex` (pas d'héritage de priorité : le verrou doit
//!     rester tenu un temps borné et minime).
//!   - [`HistogramReader`] — côté lecteur (relevé 1 Hz) : sous le verrou, il
//!     ÉCHANGE seulement l'histogramme plein contre son tampon de réserve vide
//!     (O(1), quelle que soit la capacité) ; copie, tri et percentiles se font
//!     ensuite, verrou relâché. Avant (0.6.6-17) le tri de jusqu'à 8 192
//!     valeurs se faisait sous le verrou, et le fil de réception prioritaire
//!     l'attendait (revue 0.6.6, constat B).
//!
//! Cf. internal-docs/PLAN-EXECUTION-AGENT-STABILITE.md §S1.1 pour le contexte.

use std::cmp::Ordering;

use parking_lot::Mutex;

/// Histogramme circulaire à capacité fixe, côté écrivain.
pub struct Histogram {
    /// Buffer circulaire des dernières observations. Taille = capacity, allouée
    /// une seule fois au `new()`.
    buf: Box<[f32]>,
    /// Position d'écriture courante dans `buf` (modulo cap).
    write_idx: usize,
    /// Nombre d'observations valides depuis la dernière lecture (≤ cap).
    count: usize,
}

impl Histogram {
    /// Nouvelle instance vide de capacité `capacity` mesures. Doit être > 0.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "Histogram capacity must be > 0");
        Self {
            buf: vec![0.0; capacity].into_boxed_slice(),
            write_idx: 0,
            count: 0,
        }
    }

    /// Enregistre une mesure en millisecondes. **Hot path** : O(1), zero-alloc.
    #[inline]
    pub fn observe(&mut self, value_ms: f32) {
        let cap = self.buf.len();
        self.buf[self.write_idx] = value_ms;
        self.write_idx = (self.write_idx + 1) % cap;
        if self.count < cap {
            self.count += 1;
        }
    }

    /// Oublie les observations de la fenêtre en cours. O(1) : `buf` n'est pas
    /// remis à zéro, il sera réécrit par les `observe()` suivants.
    pub fn reset(&mut self) {
        self.count = 0;
        self.write_idx = 0;
    }

    /// Nombre maximal d'observations conservées.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Percentiles des observations, calculés dans `scratch` (capacité déjà
    /// réservée par le lecteur : aucune allocation).
    fn snapshot(&self, scratch: &mut Vec<f32>) -> HistogramSnapshot {
        let n = self.count;
        if n == 0 {
            return HistogramSnapshot::default();
        }
        scratch.clear();
        scratch.extend_from_slice(&self.buf[..n]);
        // partial_cmp peut retourner None sur NaN. On range les NaN comme égaux,
        // ce qui les laisse en position arbitraire — acceptable car observe()
        // ne devrait jamais recevoir de NaN (durée en ms d'un Instant::elapsed).
        scratch.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));

        let p50_idx = n / 2;
        // p99 : index floor(n * 0.99). Pour n=100 → idx 99 = max. Pour n=10 → idx 9 = max.
        // C'est OK : le p99 sur petit échantillon converge vers max.
        let p99_idx = (((n as f32) * 0.99).floor() as usize).min(n - 1);
        HistogramSnapshot {
            count: n,
            p50_ms: scratch[p50_idx],
            p99_ms: scratch[p99_idx],
            max_ms: scratch[n - 1],
            mean_ms: scratch.iter().sum::<f32>() / n as f32,
        }
    }
}

/// Lecteur d'un histogramme partagé : réserve de même capacité + tampon de tri,
/// alloués une fois à la création du lecteur.
pub struct HistogramReader {
    /// Histogramme vide échangé, sous le verrou, contre l'histogramme vivant.
    spare: Histogram,
    /// Tampon de tri (capacité = celle de l'histogramme).
    scratch: Vec<f32>,
}

impl HistogramReader {
    /// Lecteur adapté à `live` (même capacité, lue une fois ici — chemin froid).
    pub fn for_histogram(live: &Mutex<Histogram>) -> Self {
        let capacity = live.lock().capacity();
        Self {
            spare: Histogram::new(capacity),
            scratch: Vec::with_capacity(capacity),
        }
    }

    /// Lit et vide la fenêtre de `live`. Sous le verrou : un seul échange O(1).
    /// Le tri se fait ensuite, sans bloquer l'écrivain.
    pub fn read(&mut self, live: &Mutex<Histogram>) -> HistogramSnapshot {
        {
            let mut h = live.lock();
            // Même capacité des deux côtés : sinon l'échange ferait alterner la
            // taille de l'histogramme vivant d'une seconde à l'autre.
            debug_assert_eq!(h.capacity(), self.spare.capacity(), "lecteur d'une autre capacité");
            std::mem::swap(&mut *h, &mut self.spare);
        }
        let snap = self.spare.snapshot(&mut self.scratch);
        self.spare.reset();
        snap
    }
}

/// Résultat d'une lecture ([`HistogramReader::read`]). Copy/Clone-friendly pour sérialisation tranquille.
#[derive(Debug, Clone, Copy, Default)]
pub struct HistogramSnapshot {
    /// Nombre d'observations sur la fenêtre.
    pub count: usize,
    /// Médiane (ms).
    pub p50_ms: f32,
    /// 99e percentile (ms).
    pub p99_ms: f32,
    /// Maximum observé (ms).
    pub max_ms: f32,
    /// Moyenne arithmétique (ms).
    pub mean_ms: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(capacity: usize, values: &[f32]) -> Mutex<Histogram> {
        let mut h = Histogram::new(capacity);
        for &v in values {
            h.observe(v);
        }
        Mutex::new(h)
    }

    #[test]
    fn empty_read_zeros() {
        let h = live(16, &[]);
        let s = HistogramReader::for_histogram(&h).read(&h);
        assert_eq!(s.count, 0);
        assert_eq!(s.p50_ms, 0.0);
        assert_eq!(s.p99_ms, 0.0);
        assert_eq!(s.max_ms, 0.0);
    }

    #[test]
    fn single_observation() {
        let h = live(16, &[2.5]);
        let s = HistogramReader::for_histogram(&h).read(&h);
        assert_eq!(s.count, 1);
        assert_eq!(s.p50_ms, 2.5);
        assert_eq!(s.p99_ms, 2.5);
        assert_eq!(s.max_ms, 2.5);
        assert_eq!(s.mean_ms, 2.5);
    }

    #[test]
    fn percentiles_100_values() {
        let values: Vec<f32> = (1..=100).map(|i| i as f32).collect();
        let h = live(128, &values);
        let s = HistogramReader::for_histogram(&h).read(&h);
        assert_eq!(s.count, 100);
        assert_eq!(s.p50_ms, 51.0); // index 50 = la 51e valeur triée = 51
        assert_eq!(s.p99_ms, 100.0); // floor(100*0.99) = 99 → la 100e valeur = 100
        assert_eq!(s.max_ms, 100.0);
        assert!((s.mean_ms - 50.5).abs() < 0.01);
    }

    #[test]
    fn ring_buffer_overwrites_oldest() {
        let values: Vec<f32> = (1..=10).map(|i| i as f32).collect();
        let h = live(4, &values);
        // Les 4 dernières observations (7, 8, 9, 10) doivent être les seules
        // conservées.
        let s = HistogramReader::for_histogram(&h).read(&h);
        assert_eq!(s.count, 4);
        assert_eq!(s.max_ms, 10.0);
        assert_eq!(s.p50_ms, 9.0); // index 4/2 = 2 → 3e valeur triée = 9
        assert_eq!(s.mean_ms, (7.0 + 8.0 + 9.0 + 10.0) / 4.0);
    }

    /// Chaque lecture rend la fenêtre écoulée puis repart de zéro, sur la durée
    /// (les deux tampons alternent : aucun ne doit garder d'anciennes valeurs).
    #[test]
    fn read_resets_window() {
        let h = live(16, &[1.0, 2.0]);
        let mut r = HistogramReader::for_histogram(&h);
        assert_eq!(r.read(&h).count, 2);
        assert_eq!(r.read(&h).count, 0);
        h.lock().observe(5.0);
        let s = r.read(&h);
        assert_eq!(s.count, 1);
        assert_eq!(s.p50_ms, 5.0);
        h.lock().observe(7.0);
        h.lock().observe(9.0);
        let s = r.read(&h);
        assert_eq!((s.count, s.max_ms), (2, 9.0));
    }

    /// Après une lecture, l'histogramme vivant est vide et garde sa capacité :
    /// l'écrivain continue sans réallocation.
    #[test]
    fn read_leaves_live_empty_with_same_capacity() {
        let h = live(8192, &[3.0; 100]);
        let mut r = HistogramReader::for_histogram(&h);
        assert_eq!(r.read(&h).count, 100);
        assert_eq!(h.lock().capacity(), 8192);
        assert_eq!(HistogramReader::for_histogram(&h).read(&h).count, 0, "vivant vide");
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "lecteur d'une autre capacité")]
    fn reader_of_another_capacity_is_refused() {
        let small = live(4, &[]);
        let big = live(16, &[]);
        HistogramReader::for_histogram(&small).read(&big);
    }

    #[test]
    fn reset_forgets_window() {
        let h = live(4, &[1.0, 2.0]);
        h.lock().reset();
        assert_eq!(HistogramReader::for_histogram(&h).read(&h).count, 0);
        h.lock().observe(4.0);
        assert_eq!(HistogramReader::for_histogram(&h).read(&h).max_ms, 4.0);
    }
}

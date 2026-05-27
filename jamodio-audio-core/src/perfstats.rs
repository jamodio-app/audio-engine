//! Histogrammes glissants pour l'instrumentation latence agent (Sprint S1).
//!
//! Conçu pour zero-alloc dans le hot path RT audio :
//!   - `observe(value_ms)` est O(1), pas de Vec resize, pas de Mutex.
//!     Le caller détient un `&mut Histogram` (typiquement `parking_lot::Mutex`
//!     contesté < 1 µs par un writer unique + 1 reader rare).
//!   - `flush()` (1 Hz, cold path) trie un scratch buffer pré-alloué et
//!     retourne un `HistogramSnapshot` `Copy`-friendly.
//!
//! Capacity recommandée : 512 ou 1024 (couvre largement la fenêtre 1 Hz pour
//! des cadences process_stereo de 375 obs/s à 48k/128).
//!
//! Cf. internal-docs/PLAN-EXECUTION-AGENT-STABILITE.md §S1.1 pour le contexte.

use std::cmp::Ordering;

/// Histogramme circulaire à capacité fixe. Réinitialisé à chaque `flush()`.
pub struct Histogram {
    /// Buffer circulaire des dernières observations. Taille = capacity, allouée
    /// une seule fois au `new()`.
    buf: Box<[f32]>,
    /// Scratch buffer pour tri lazy au flush. Capacity égale à `buf`, alloué
    /// une seule fois — `clear()` + `extend_from_slice` ne réallouent jamais.
    sort_scratch: Vec<f32>,
    /// Position d'écriture courante dans `buf` (modulo cap).
    write_idx: usize,
    /// Nombre d'observations valides depuis le dernier flush (≤ cap après le 1er tour).
    count: usize,
    /// Compteur de drops/erreurs accumulé depuis le dernier flush (hors `buf`).
    drops: u64,
}

impl Histogram {
    /// Nouvelle instance vide de capacité `capacity` mesures. Doit être > 0.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "Histogram capacity must be > 0");
        Self {
            buf: vec![0.0; capacity].into_boxed_slice(),
            sort_scratch: Vec::with_capacity(capacity),
            write_idx: 0,
            count: 0,
            drops: 0,
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

    /// Incrémente le compteur de drops (cf. "sample channel full" en capture.rs).
    /// Saturating pour éviter overflow théorique sur sessions très longues.
    #[inline]
    pub fn record_drop(&mut self) {
        self.drops = self.drops.saturating_add(1);
    }

    /// Snapshot triée + percentiles. **Cold path** (typiquement 1 Hz).
    /// Réinitialise `count` et `drops` pour la fenêtre suivante. `buf` n'est
    /// pas zeroisé — il sera overwrité par les `observe()` futurs.
    pub fn flush(&mut self) -> HistogramSnapshot {
        let n = self.count;
        let drops = self.drops;
        if n == 0 {
            // Reset compteurs même si rien à mesurer (pour ne pas mélanger
            // les drops d'une fenêtre où il n'y a plus de hot path actif).
            self.drops = 0;
            return HistogramSnapshot {
                count: 0,
                p50_ms: 0.0,
                p99_ms: 0.0,
                max_ms: 0.0,
                mean_ms: 0.0,
                drops,
            };
        }

        // Copie zero-alloc dans le scratch buffer (capacity == buf.len()).
        self.sort_scratch.clear();
        self.sort_scratch.extend_from_slice(&self.buf[..n]);
        // partial_cmp peut retourner None sur NaN. On range les NaN comme égaux,
        // ce qui les laisse en position arbitraire — acceptable car observe()
        // ne devrait jamais recevoir de NaN (durée en ms d'un Instant::elapsed).
        self.sort_scratch
            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));

        let p50_idx = n / 2;
        // p99 : index floor(n * 0.99). Pour n=100 → idx 99 = max. Pour n=10 → idx 9 = max.
        // C'est OK : le p99 sur petit échantillon converge vers max.
        let p99_idx = (((n as f32) * 0.99).floor() as usize).min(n - 1);
        let p50_ms = self.sort_scratch[p50_idx];
        let p99_ms = self.sort_scratch[p99_idx];
        let max_ms = self.sort_scratch[n - 1];
        let mean_ms = self.sort_scratch.iter().sum::<f32>() / n as f32;

        self.count = 0;
        self.write_idx = 0;
        self.drops = 0;

        HistogramSnapshot {
            count: n,
            p50_ms,
            p99_ms,
            max_ms,
            mean_ms,
            drops,
        }
    }

    /// Vrai si aucune observation depuis le dernier flush. Utile pour skipper
    /// l'émission d'un PerfStats vide quand le pipeline est idle.
    pub fn is_empty(&self) -> bool {
        self.count == 0 && self.drops == 0
    }
}

/// Résultat d'un `flush()`. Copy/Clone-friendly pour sérialisation tranquille.
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
    /// Compteur de drops/erreurs accumulé depuis le dernier flush.
    pub drops: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_flush_zeros() {
        let mut h = Histogram::new(16);
        let s = h.flush();
        assert_eq!(s.count, 0);
        assert_eq!(s.p50_ms, 0.0);
        assert_eq!(s.p99_ms, 0.0);
        assert_eq!(s.max_ms, 0.0);
        assert_eq!(s.drops, 0);
    }

    #[test]
    fn single_observation() {
        let mut h = Histogram::new(16);
        h.observe(2.5);
        let s = h.flush();
        assert_eq!(s.count, 1);
        assert_eq!(s.p50_ms, 2.5);
        assert_eq!(s.p99_ms, 2.5);
        assert_eq!(s.max_ms, 2.5);
        assert_eq!(s.mean_ms, 2.5);
    }

    #[test]
    fn percentiles_100_values() {
        let mut h = Histogram::new(128);
        for i in 1..=100 {
            h.observe(i as f32);
        }
        let s = h.flush();
        assert_eq!(s.count, 100);
        assert_eq!(s.p50_ms, 51.0); // index 50 = la 51e valeur triée = 51
        assert_eq!(s.p99_ms, 100.0); // floor(100*0.99) = 99 → la 100e valeur = 100
        assert_eq!(s.max_ms, 100.0);
        assert!((s.mean_ms - 50.5).abs() < 0.01);
    }

    #[test]
    fn ring_buffer_overwrites_oldest() {
        let mut h = Histogram::new(4);
        for i in 1..=10 {
            h.observe(i as f32);
        }
        // Les 4 dernières observations (7, 8, 9, 10) doivent être les seules
        // conservées.
        let s = h.flush();
        assert_eq!(s.count, 4);
        assert_eq!(s.max_ms, 10.0);
        assert_eq!(s.p50_ms, 9.0); // index 4/2 = 2 → 3e valeur triée = 9
        assert_eq!(s.mean_ms, (7.0 + 8.0 + 9.0 + 10.0) / 4.0);
    }

    #[test]
    fn flush_resets_state() {
        let mut h = Histogram::new(16);
        h.observe(1.0);
        h.observe(2.0);
        h.record_drop();
        let s1 = h.flush();
        assert_eq!(s1.count, 2);
        assert_eq!(s1.drops, 1);

        let s2 = h.flush();
        assert_eq!(s2.count, 0);
        assert_eq!(s2.drops, 0);

        h.observe(5.0);
        let s3 = h.flush();
        assert_eq!(s3.count, 1);
        assert_eq!(s3.p50_ms, 5.0);
    }

    #[test]
    fn drops_independent_of_observations() {
        let mut h = Histogram::new(8);
        for _ in 0..20 {
            h.record_drop();
        }
        let s = h.flush();
        assert_eq!(s.count, 0);
        assert_eq!(s.drops, 20);
    }

    #[test]
    fn is_empty_tracks_both_axes() {
        let mut h = Histogram::new(4);
        assert!(h.is_empty());
        h.observe(1.0);
        assert!(!h.is_empty());
        h.flush();
        assert!(h.is_empty());
        h.record_drop();
        assert!(!h.is_empty());
    }
}

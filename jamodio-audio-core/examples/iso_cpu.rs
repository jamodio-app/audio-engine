//! Coût CPU du filtre antibruit — combien d'un cœur consomme-t-il ?
//!
//! Écrit le 06/09/2026 parce qu'on s'est aperçu, APRÈS avoir publié la 0.6.0,
//! qu'on n'avait aucun moyen de répondre à cette question : rien ne mesurait le
//! chemin voix, ni en test ni sur le terrain. Une fonctionnalité qui tourne en
//! continu doit avoir son coût chiffré — sinon on découvre la facture chez
//! l'utilisateur, sous forme de « tap voix saturé ».
//!
//! Usage : `cargo run --release -p jamodio-audio-core --example iso_cpu`
//! (le mode release est OBLIGATOIRE — en debug le résultat n'a aucun sens.)
//!
//! Lecture : « % d'un cœur » = temps de calcul / durée d'audio traitée. 30 % veut
//! dire que traiter 10 s de parole occupe 3 s d'un cœur. À comparer sur la machine
//! la plus FAIBLE du parc, pas sur la station de dev.
use jamodio_audio_core::voice_isolation::{IsolationConfig, VoiceIsolator};
use std::time::Instant;

fn main() {
    let sr = 48_000usize;
    // Bloc de 512 échantillons = 10,7 ms @48k, ordre de grandeur du chemin voix.
    for &bloc in &[128usize, 256, 512, 1024] {
        let mut iso = VoiceIsolator::new(IsolationConfig::default()).expect("isolator");
        // 30 s de signal : parole synthétique (somme de formants) + bruit léger.
        let secondes = 30usize;
        let total = sr * secondes;
        let mut src: Vec<f32> = (0..total)
            .map(|i| {
                let t = i as f32 / sr as f32;
                0.25 * (2.0 * std::f32::consts::PI * 140.0 * t).sin()
                    + 0.12 * (2.0 * std::f32::consts::PI * 700.0 * t).sin()
                    + 0.05 * ((i * 2654435761) as f32 / u32::MAX as f32 - 0.5)
            })
            .collect();
        // Rodage : la 1re inférence alloue.
        let mut warm = vec![0.0f32; bloc];
        for _ in 0..20 { iso.process_block(&mut warm).unwrap(); }

        let t0 = Instant::now();
        for c in src.chunks_mut(bloc) {
            if c.len() == bloc { iso.process_block(c).unwrap(); }
        }
        let dt = t0.elapsed().as_secs_f64();
        let audio = secondes as f64;
        println!(
            "bloc {:>4} ({:>5.1} ms) : {:>6.2} s de calcul pour {:.0} s d'audio  →  {:>5.2} % d'un cœur  (RTF {:.4})",
            bloc, bloc as f64 * 1000.0 / sr as f64, dt, audio, dt / audio * 100.0, dt / audio
        );
    }
}

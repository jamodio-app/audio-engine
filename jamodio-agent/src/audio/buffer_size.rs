//! Helpers partagés autour de `cpal::BufferSize`.
//!
//! Capture (input) et playback (output) partagent la même décision :
//! demander `BufferSize::Fixed(N)` low-latency si le device l'expose dans
//! son `SupportedBufferSize::Range`, sinon retomber sur `Default` pour
//! laisser le backend choisir. La logique de probe étant strictement
//! identique des deux côtés (seul l'iterator de configs diffère), on
//! la factorise ici.

use cpal::{SampleRate, SupportedBufferSize, SupportedStreamConfigRange};

/// Vérifie qu'un iterator de configs supportées contient un Range qui
/// couvre `target_buf` pour le couple `(channels, sr)`. Utilisé côté
/// input (`device.supported_input_configs()`) et output
/// (`device.supported_output_configs()`) — la sémantique du Range est
/// la même.
///
/// Comportement par OS / type de device (vrai pour input ET output) :
/// - macOS CoreAudio       : Range large, contient quasi toujours 128.
/// - Windows ASIO          : `Range { min: 16, max: 4096 }` typique.
/// - Windows WASAPI excl.  : Range large, dépend du device.
/// - Windows WASAPI shared : `Range { min: 480, max: 480 }` (10 ms à 48k)
///   ou `BufferSize::Unknown` → refus pour 128.
pub fn configs_support_fixed_buffer<I>(
    configs: I,
    channels: u16,
    sr: u32,
    target_buf: u32,
) -> bool
where
    I: IntoIterator<Item = SupportedStreamConfigRange>,
{
    let target_sr = SampleRate(sr);
    for cfg in configs {
        if cfg.channels() != channels {
            continue;
        }
        if cfg.min_sample_rate() > target_sr || cfg.max_sample_rate() < target_sr {
            continue;
        }
        if let SupportedBufferSize::Range { min, max } = cfg.buffer_size() {
            if target_buf >= *min && target_buf <= *max {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note : `SupportedStreamConfigRange` n'a pas de constructeur public
    // dans CPAL, on ne peut donc pas en fabriquer pour des tests unitaires
    // exhaustifs. Les vrais tests d'intégration (= contre un Device CPAL)
    // vivent dans les modules caller `capture.rs` et `playback.rs` qui
    // nécessitent un hardware audio. On garde cette fonction PURE
    // (aucun side-effect, aucun I/O) pour rester triviale à auditer.

    /// Smoke test minimaliste : iterator vide → false. Garantit la
    /// sémantique « pas de config supportée = pas de Fixed », qui est
    /// précisément ce qui déclenche le fallback `BufferSize::Default`
    /// côté caller.
    #[test]
    fn empty_iterator_returns_false() {
        let configs: Vec<SupportedStreamConfigRange> = Vec::new();
        assert!(!configs_support_fixed_buffer(configs, 2, 48000, 128));
    }
}

use audiopus::{coder::Encoder as OpusEncoder, Application, Channels, SampleRate};

/// Opus encoder configured for low-latency music streaming.
/// Frame size: 120 samples = 2.5ms at 48kHz stereo.
pub struct MusicEncoder {
    encoder: OpusEncoder,
    frame_size: usize,
}

const FRAME_SAMPLES: usize = 120; // 2.5ms at 48kHz
pub const MAX_PACKET_SIZE: usize = 4000;

impl MusicEncoder {
    pub fn new() -> Result<Self, audiopus::Error> {
        // RESTRICTED_LOWDELAY : supprime le lookahead réservé au resampler SILK.
        // À 2,5 ms de frame Opus est de toute façon CELT-only (SILK exige des
        // frames ≥ 10 ms), donc ce lookahead est du délai mort. Mesuré via
        // OPUS_GET_LOOKAHEAD (cf. test `lowdelay_lookahead_vs_audio`) :
        //   Audio    = 312 samples (6,5 ms)
        //   LowDelay = 120 samples (2,5 ms)  → −4 ms note→oreille, qualité CELT identique.
        let mut encoder = OpusEncoder::new(
            SampleRate::Hz48000,
            Channels::Stereo,
            Application::LowDelay,
        )?;

        // Low-latency music settings
        encoder.set_bitrate(audiopus::Bitrate::BitsPerSecond(320000))?;
        encoder.set_inband_fec(false)?;
        // DTX OFF = un paquet par frame en CONTINU, même en silence. INVARIANT
        // dont dépend l'idle-timeout fantôme de `recv_io_task` (pipeline.rs) :
        // « 8 s sans paquet = flux mort ». Activer DTX casserait cette hypothèse
        // (un pair silencieux cesserait d'émettre) → revoir l'idle-timeout avant.
        encoder.set_dtx(false)?;
        encoder.set_vbr(false)?; // CBR for predictable latency

        Ok(Self {
            encoder,
            frame_size: FRAME_SAMPLES,
        })
    }

    /// Encode one frame of interleaved f32 stereo samples.
    /// Input: exactly `frame_size * 2` f32 samples (stereo interleaved).
    pub fn encode(&self, pcm: &[f32], output: &mut [u8]) -> Result<usize, audiopus::Error> {
        assert_eq!(pcm.len(), self.frame_size * 2);
        self.encoder.encode_float(pcm, output)
    }

    pub fn frame_size(&self) -> usize {
        self.frame_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construit un encodeur dans la config réseau réelle (320k CBR stéréo 48k)
    /// pour un mode `application` donné, et renvoie son lookahead algorithmique
    /// (en samples/canal à 48 kHz), lu via OPUS_GET_LOOKAHEAD.
    fn lookahead_for(app: Application) -> u32 {
        let mut enc = OpusEncoder::new(SampleRate::Hz48000, Channels::Stereo, app).unwrap();
        enc.set_bitrate(audiopus::Bitrate::BitsPerSecond(320000)).unwrap();
        enc.set_vbr(false).unwrap();
        enc.lookahead().unwrap()
    }

    /// Mesure empirique du gain de latence algorithmique de RESTRICTED_LOWDELAY
    /// vs AUDIO, dans NOTRE config (frame 2,5 ms). Sert de preuve + garde-fou.
    #[test]
    fn lowdelay_lookahead_vs_audio() {
        let audio = lookahead_for(Application::Audio);
        let lowdelay = lookahead_for(Application::LowDelay);
        let to_ms = |s: u32| s as f64 / 48.0;
        println!(
            "lookahead Audio    = {audio} samples ({:.3} ms)",
            to_ms(audio)
        );
        println!(
            "lookahead LowDelay = {lowdelay} samples ({:.3} ms)",
            to_ms(lowdelay)
        );
        println!(
            "gain               = {} samples ({:.3} ms)",
            audio.saturating_sub(lowdelay),
            to_ms(audio.saturating_sub(lowdelay))
        );
        assert!(lowdelay <= audio, "LowDelay ne doit jamais ajouter de delay");
        // Invariant verrouillé : en CELT-only le lookahead vaut exactement une
        // frame (120 samples = 2,5 ms). C'est ce qui justifie `opus_ms = 2.5`
        // dans la télémétrie de latence (ws_server.rs). Si libopus changeait
        // cette valeur, ce test casserait et la télémétrie devrait suivre.
        assert_eq!(
            lowdelay, FRAME_SAMPLES as u32,
            "le lookahead LowDelay doit valoir une frame"
        );
    }
}

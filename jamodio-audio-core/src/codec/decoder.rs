use audiopus::{coder::Decoder as OpusDecoder, packet::Packet, Channels, MutSignals, SampleRate};
use std::convert::TryFrom;

/// Opus decoder for one remote music stream.
pub struct MusicDecoder {
    decoder: OpusDecoder,
    /// Max buffer size for decode (handles any Opus frame up to 120ms).
    max_frame: usize,
    /// Actual frame size learned from first successful decode.
    /// Used for PLC so we don't generate 120ms of concealment audio.
    actual_frame: usize,
    /// Buffers pré-alloués réutilisés à chaque decode (~400 paquets/s).
    /// Sans ça, decode() allouait 2 Vec/paquet → ~10 Mo/s d'allocations
    /// par stream sur le hot path receive.
    pcm_buf: Vec<i16>,
    f32_buf: Vec<f32>,
    log_count: u64,
}

/// Max Opus frame: 120ms at 48kHz = 5760 samples per channel.
const MAX_FRAME_SAMPLES: usize = 5760;
/// Default PLC frame size until we learn the real one (20ms = typical Chrome).
const DEFAULT_PLC_SAMPLES: usize = 960;

impl MusicDecoder {
    pub fn new() -> Result<Self, audiopus::Error> {
        let decoder = OpusDecoder::new(SampleRate::Hz48000, Channels::Stereo)?;
        // Gain à 0dB — le signal Opus est déjà au bon niveau.
        // (Le +26dB précédent compensait des bugs RTP/RTCP depuis corrigés.)
        let max_samples_stereo = MAX_FRAME_SAMPLES * 2;
        Ok(Self {
            decoder,
            max_frame: MAX_FRAME_SAMPLES,
            actual_frame: DEFAULT_PLC_SAMPLES,
            pcm_buf: vec![0i16; max_samples_stereo],
            f32_buf: vec![0.0f32; max_samples_stereo],
            log_count: 0,
        })
    }

    /// Decode an Opus packet into interleaved f32 stereo samples.
    pub fn decode(&mut self, opus_data: &[u8]) -> Option<&[f32]> {
        let packet = match Packet::try_from(opus_data) {
            Ok(p) => p,
            Err(e) => {
                if self.log_count % 500 == 0 {
                    tracing::warn!(target: "jamodio::decoder", bytes = opus_data.len(), error = ?e, "Packet::try_from failed");
                }
                self.log_count += 1;
                return None;
            }
        };
        let stereo_len = self.max_frame * 2;
        let signals = match MutSignals::try_from(&mut self.pcm_buf[..stereo_len]) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(target: "jamodio::decoder", error = ?e, "MutSignals failed");
                return None;
            }
        };
        let decoded = match self.decoder.decode(Some(packet), signals, false) {
            Ok(n) => n,
            Err(e) => {
                if self.log_count % 500 == 0 {
                    tracing::warn!(target: "jamodio::decoder", bytes = opus_data.len(), error = ?e, "decode failed");
                }
                self.log_count += 1;
                return None;
            }
        };

        // Learn actual frame size from first successful decode
        if self.log_count == 0 {
            self.actual_frame = decoded;
            tracing::info!(target: "jamodio::decoder", samples_per_channel = decoded, bytes_in = opus_data.len(), "first decode");
        }
        self.log_count += 1;

        let n = decoded * 2;
        for i in 0..n {
            self.f32_buf[i] = self.pcm_buf[i] as f32 / 32768.0;
        }
        Some(&self.f32_buf[..n])
    }

    /// Decode a lost packet (PLC). Uses actual frame size, not max.
    pub fn decode_loss(&mut self) -> Option<&[f32]> {
        let stereo_len = self.actual_frame * 2;
        let signals = MutSignals::try_from(&mut self.pcm_buf[..stereo_len]).ok()?;
        let decoded = self.decoder.decode(None, signals, false).ok()?;
        let n = decoded * 2;
        for i in 0..n {
            self.f32_buf[i] = self.pcm_buf[i] as f32 / 32768.0;
        }
        Some(&self.f32_buf[..n])
    }
}

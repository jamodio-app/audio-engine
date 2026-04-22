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
        Ok(Self {
            decoder,
            max_frame: MAX_FRAME_SAMPLES,
            actual_frame: DEFAULT_PLC_SAMPLES,
            log_count: 0,
        })
    }

    /// Decode an Opus packet into interleaved f32 stereo samples.
    pub fn decode(&mut self, opus_data: &[u8]) -> Option<Vec<f32>> {
        let packet = match Packet::try_from(opus_data) {
            Ok(p) => p,
            Err(e) => {
                if self.log_count % 500 == 0 {
                    eprintln!("[DECODER] Packet::try_from failed ({} bytes): {:?}", opus_data.len(), e);
                }
                self.log_count += 1;
                return None;
            }
        };
        let mut pcm = vec![0i16; self.max_frame * 2]; // stereo, large enough for any frame
        let signals = match MutSignals::try_from(&mut pcm[..]) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[DECODER] MutSignals failed: {:?}", e);
                return None;
            }
        };
        let decoded = match self.decoder.decode(Some(packet), signals, false) {
            Ok(n) => n,
            Err(e) => {
                if self.log_count % 500 == 0 {
                    eprintln!("[DECODER] decode failed ({} bytes): {:?}", opus_data.len(), e);
                }
                self.log_count += 1;
                return None;
            }
        };

        // Learn actual frame size from first successful decode
        if self.log_count == 0 {
            self.actual_frame = decoded;
            eprintln!("[DECODER] First decode: {} samples/ch, {} bytes in", decoded, opus_data.len());
        }
        self.log_count += 1;

        Some(pcm[..decoded * 2].iter().map(|&s| s as f32 / 32768.0).collect())
    }

    /// Decode a lost packet (PLC). Uses actual frame size, not max.
    pub fn decode_loss(&mut self) -> Option<Vec<f32>> {
        let mut pcm = vec![0i16; self.actual_frame * 2];
        let signals = MutSignals::try_from(&mut pcm[..]).ok()?;
        let decoded = self.decoder.decode(None, signals, false).ok()?;
        Some(pcm[..decoded * 2].iter().map(|&s| s as f32 / 32768.0).collect())
    }
}

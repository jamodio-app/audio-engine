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
        let mut encoder = OpusEncoder::new(
            SampleRate::Hz48000,
            Channels::Stereo,
            Application::Audio,
        )?;

        // Low-latency music settings
        encoder.set_bitrate(audiopus::Bitrate::BitsPerSecond(320000))?;
        encoder.set_inband_fec(false)?;
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

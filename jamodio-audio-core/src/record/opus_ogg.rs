//! OpusOggRecorder — un OpusEncoder + un OggWriter qui s'alimente.
//!
//! Entrée : samples PCM stéréo f32 48kHz (le format unifié interne du pipeline).
//! Sortie (finalize) : un Vec<u8> contenant un fichier Ogg/Opus complet,
//! lisible directement par VLC, ffplay, Reaper, etc.
//!
//! Choix techniques :
//!   - Frame Opus de 20ms (= 960 samples par canal à 48kHz). Permet la
//!     meilleure compression Opus tout en restant souple sur la latence
//!     de finalisation (on n'enregistre pas en temps réel — donc inutile
//!     de payer des frames 2.5ms comme la pipeline RTP).
//!   - Bitrate : 128 kbps stéréo, VBR. Qualité musicale élevée pour ~1 MB/min.
//!   - Pre-skip : 312 samples (= 6.5ms) — défaut Opus standard pour la
//!     compensation du delay du décodeur. Référencé tel quel dans OpusHead.
//!   - Packets groupés en pages : 50 packets par page (≈ 1s d'audio) →
//!     overhead Ogg négligeable (~28 octets/s).

use super::ogg::{build_opus_head, build_opus_tags, OggWriter};
use audiopus::{coder::Encoder as OpusEncoder, Application, Channels, SampleRate};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS_U8: u8 = 2;
const FRAME_SAMPLES_PER_CHANNEL: usize = 960; // 20ms à 48kHz
const FRAME_SAMPLES_INTERLEAVED: usize = FRAME_SAMPLES_PER_CHANNEL * 2; // stéréo
const BITRATE_BPS: i32 = 128_000;
const OPUS_PRE_SKIP: u16 = 312;
const PACKETS_PER_PAGE: usize = 50;
const OPUS_MAX_PACKET_BYTES: usize = 4000;

/// État d'un recorder pour un seul stream (= un seul fichier Ogg/Opus en sortie).
pub struct OpusOggRecorder {
    encoder: OpusEncoder,
    /// Buffer d'accumulation de samples PCM stéréo interleaved (L,R,L,R,...).
    /// Vidé par chunks de FRAME_SAMPLES_INTERLEAVED quand plein.
    frame_buf: Vec<f32>,
    /// Buffer réutilisable pour l'encodage Opus.
    opus_out: Vec<u8>,
    /// Packets Opus encodés en attente d'écriture dans une page Ogg.
    pending_packets: Vec<Vec<u8>>,
    /// Granule cumulée (samples décodés à 48kHz depuis le début).
    granule: u64,
    ogg: OggWriter,
    headers_written: bool,
}

impl OpusOggRecorder {
    pub fn new() -> Result<Self, audiopus::Error> {
        let mut encoder = OpusEncoder::new(
            SampleRate::Hz48000,
            Channels::Stereo,
            Application::Audio, // musique : prend les transforms MDCT optimaux
        )?;
        encoder.set_bitrate(audiopus::Bitrate::BitsPerSecond(BITRATE_BPS))?;
        encoder.set_vbr(true)?; // VBR pour la qualité (pas de contrainte latence ici)
        encoder.set_inband_fec(false)?;
        encoder.set_dtx(false)?;

        // Serial pseudo-random : OK avec le bas du process timestamp ; pas
        // de collision possible dans un même fichier de toute façon.
        let serial = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)) ^ (std::process::id() as u32);

        let mut ogg = OggWriter::new(serial);

        // Header page (OpusHead) + Tags page : écrits immédiatement pour
        // que `bytes` soit toujours un Ogg valide même si finalize jamais
        // appelé (cas d'un crash : on perd l'audio mais le header est OK).
        let head = build_opus_head(CHANNELS_U8, OPUS_PRE_SKIP, SAMPLE_RATE);
        ogg.write_header(&head);
        let tags = build_opus_tags("Jamodio Agent");
        ogg.write_tags(&tags);

        Ok(Self {
            encoder,
            frame_buf: Vec::with_capacity(FRAME_SAMPLES_INTERLEAVED * 2),
            opus_out: vec![0u8; OPUS_MAX_PACKET_BYTES],
            pending_packets: Vec::with_capacity(PACKETS_PER_PAGE),
            granule: 0,
            ogg,
            headers_written: true,
        })
    }

    /// Push des samples PCM stéréo entrelacés (L,R,L,R,...). Encode dès
    /// qu'une frame complète (960 samples par canal = 1920 stéréo) est
    /// accumulée. Pages Ogg flush automatiquement par paquets de 50.
    ///
    /// Coût : fonction non-bloquante côté caller. Encode Opus 20ms ≈ 30-80μs
    /// sur Apple Silicon — exécuté SOUS le lock côté caller, donc le caller
    /// doit accepter ce coût ou l'éviter en routant via channel + thread
    /// dédié (cf. recording.rs côté agent qui fait ce routage).
    pub fn push_samples(&mut self, pcm_stereo: &[f32]) {
        self.frame_buf.extend_from_slice(pcm_stereo);

        while self.frame_buf.len() >= FRAME_SAMPLES_INTERLEAVED {
            // Collect d'une frame, drain pour libérer la capacity.
            let frame: Vec<f32> = self.frame_buf.drain(..FRAME_SAMPLES_INTERLEAVED).collect();
            match self.encoder.encode_float(&frame, &mut self.opus_out) {
                Ok(n) => {
                    self.pending_packets.push(self.opus_out[..n].to_vec());
                    self.granule += FRAME_SAMPLES_PER_CHANNEL as u64;
                    if self.pending_packets.len() >= PACKETS_PER_PAGE {
                        self.flush_page(false);
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "jamodio::record", error = %e, "opus encode failed, frame dropped");
                }
            }
        }
    }

    /// Force le flush des packets en attente dans une page Ogg.
    /// `is_last` → set EOS flag (dernière page).
    fn flush_page(&mut self, is_last: bool) {
        if self.pending_packets.is_empty() && !is_last { return; }
        if self.pending_packets.is_empty() && is_last {
            // Cas pathologique : aucun packet jamais écrit (recording stoppé
            // avant la 1re frame complète). On écrit une page audio vide
            // avec EOS pour que le fichier soit valide.
            self.ogg.write_audio_page(&[], self.granule, true);
            return;
        }
        let refs: Vec<&[u8]> = self.pending_packets.iter().map(|p| p.as_slice()).collect();
        self.ogg.write_audio_page(&refs, self.granule, is_last);
        self.pending_packets.clear();
    }

    /// Écrit la dernière page (EOS) + retourne les bytes Ogg complets.
    /// Consomme le recorder.
    pub fn finalize(mut self) -> Vec<u8> {
        // Pad le frame_buf à une frame complète si on a un reliquat (sinon
        // les derniers samples non encodés sont perdus). Padding silencieux.
        if !self.frame_buf.is_empty() && self.frame_buf.len() < FRAME_SAMPLES_INTERLEAVED {
            let needed = FRAME_SAMPLES_INTERLEAVED - self.frame_buf.len();
            self.frame_buf.extend(std::iter::repeat(0.0f32).take(needed));
            // Encode cette dernière frame paddée
            let frame: Vec<f32> = self.frame_buf.drain(..FRAME_SAMPLES_INTERLEAVED).collect();
            if let Ok(n) = self.encoder.encode_float(&frame, &mut self.opus_out) {
                self.pending_packets.push(self.opus_out[..n].to_vec());
                self.granule += FRAME_SAMPLES_PER_CHANNEL as u64;
            }
        }
        self.flush_page(true);
        debug_assert!(self.headers_written);
        self.ogg.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_valid_ogg_magic_and_headers() {
        let mut rec = OpusOggRecorder::new().expect("encoder");
        // 1 seconde de silence stéréo (48000 frames × 2 = 96000 f32s)
        let silence = vec![0.0f32; 96000];
        rec.push_samples(&silence);
        let bytes = rec.finalize();

        // Magic OggS au début
        assert_eq!(&bytes[..4], b"OggS");
        // OpusHead doit apparaître dans les ~30 premiers bytes
        let head_idx = bytes.windows(8).position(|w| w == b"OpusHead");
        assert!(head_idx.is_some(), "OpusHead not found");
        // OpusTags aussi
        let tags_idx = bytes.windows(8).position(|w| w == b"OpusTags");
        assert!(tags_idx.is_some(), "OpusTags not found");
        // Plusieurs pages OggS attendues (header + tags + audio)
        let page_count = bytes.windows(4).filter(|w| w == b"OggS").count();
        assert!(page_count >= 3, "expected >=3 pages, got {}", page_count);
    }

    #[test]
    fn finalize_empty_still_produces_valid_ogg() {
        // Cas pathologique : recording stoppé avant la 1re frame complète.
        let rec = OpusOggRecorder::new().expect("encoder");
        let bytes = rec.finalize();
        assert_eq!(&bytes[..4], b"OggS");
        // Au moins header + tags + 1 page audio EOS vide
        let page_count = bytes.windows(4).filter(|w| w == b"OggS").count();
        assert!(page_count >= 2, "expected >=2 pages, got {}", page_count);
    }
}

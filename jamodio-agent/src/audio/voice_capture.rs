//! Capture du canal **VOIX** (talkback) sur un périphérique DÉDIÉ, distinct de
//! l'interface instrument.
//!
//! # Pourquoi ce module (chantier micro talkback séparé, 09/2026)
//!
//! Jusqu'ici le talkback était un CANAL du flux instrument : impossible de parler
//! avec une interface à une seule entrée (basse branchée = pas de micro), et
//! choisir un autre micro faisait sortir la voix de l'agent (capture navigateur,
//! donc sans Filtre antibruit). Ce module ouvre un **second flux d'entrée**, sur
//! le périphérique voix choisi, et fournit à l'étage voix exactement ce qu'il
//! attend : des blocs **mono 48 kHz**.
//!
//! # Ce qu'il fait, et ce qu'il ne fait pas
//!
//! - Extrait UN canal du flux (la voix est mono).
//! - **Rééchantillonne vers 48 kHz** si le micro tourne à une autre fréquence —
//!   un micro-casque ou interne est souvent en 44,1 ou 16 kHz. C'est une
//!   exception ASSUMÉE et limitée au canal voix (décision Ben, 04/09/2026) : le
//!   chemin instrument garde R2 (48 kHz natif obligatoire, aucun resampler).
//!   Coût mesuré ~3-4 ms, sur un canal qui en porte déjà ~130.
//! - Ne touche à RIEN du chemin instrument : autre thread, autre flux, autre host.

use cpal::traits::DeviceTrait;
use cpal::{Device, SampleFormat};
use crossbeam_channel::{Sender, TrySendError};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

/// Fréquence de travail de l'étage voix (et de tout le pipeline).
pub const VOICE_SR: u32 = 48_000;
/// Taille de chunk du rééchantillonneur, en trames d'ENTRÉE. 128 trames ≈ 2,9 ms
/// à 44,1 kHz : assez petit pour ne pas alourdir le canal, assez grand pour que
/// le coût par appel reste négligeable.
const RESAMPLE_CHUNK: usize = 128;
/// Longueur du noyau sinc. 64 taps = bon compromis qualité/CPU pour de la parole ;
/// retard de groupe ≈ 32 échantillons (0,7 ms à 44,1 kHz).
const SINC_LEN: usize = 64;

/// Échecs possibles de l'ouverture du canal voix. Chacun est REMONTÉ tel quel à
/// l'appelant : aucun n'est rattrapé en douce par un repli sur un autre
/// périphérique ou un autre canal (doctrine device id strict).
#[derive(Debug)]
pub enum VoiceCaptureError {
    ChannelOutOfRange { requested: usize, available: usize },
    Config(String),
    UnsupportedFormat(SampleFormat),
    Resampler { from: u32, to: u32, detail: String },
    BuildStream(cpal::BuildStreamError),
}

impl std::fmt::Display for VoiceCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChannelOutOfRange { requested, available } => write!(
                f,
                "canal {requested} demandé mais le périphérique n'en a que {available}"
            ),
            Self::Config(e) => write!(f, "configuration d'entrée illisible : {e}"),
            Self::UnsupportedFormat(fmt) => {
                write!(f, "format d'échantillon non pris en charge : {fmt:?}")
            }
            Self::Resampler { from, to, detail } => {
                write!(f, "rééchantillonneur {from} Hz → {to} Hz indisponible : {detail}")
            }
            Self::BuildStream(e) => write!(f, "ouverture du flux voix impossible : {e}"),
        }
    }
}

impl std::error::Error for VoiceCaptureError {}

impl From<cpal::BuildStreamError> for VoiceCaptureError {
    fn from(e: cpal::BuildStreamError) -> Self {
        Self::BuildStream(e)
    }
}

/// Convertit un flux d'entrée quelconque (multicanal, fréquence quelconque) en
/// blocs **mono 48 kHz**. Partie PURE du module : aucun accès matériel, donc
/// testable sans carte son.
pub struct VoiceInputConverter {
    channels_in: usize,
    channel: usize,
    /// `None` quand le périphérique est déjà en 48 kHz → le signal passe tel
    /// quel, sans aucun retard ajouté (cas de la plupart des interfaces).
    resampler: Option<SincFixedIn<f32>>,
    /// Mono à la fréquence SOURCE, en attente d'un chunk complet.
    in_acc: Vec<f32>,
    /// Tampons rubato préalloués (1 canal) — pas d'allocation dans le callback.
    in_buf: Vec<Vec<f32>>,
    out_buf: Vec<Vec<f32>>,
    sample_rate_in: u32,
}

impl VoiceInputConverter {
    /// `channel` est l'index 0-based du canal à extraire.
    pub fn new(
        channels_in: usize,
        channel: usize,
        sample_rate_in: u32,
    ) -> Result<Self, VoiceCaptureError> {
        if channels_in == 0 || channel >= channels_in {
            return Err(VoiceCaptureError::ChannelOutOfRange {
                requested: channel,
                available: channels_in,
            });
        }
        let resampler = if sample_rate_in == VOICE_SR {
            None
        } else {
            let params = SincInterpolationParameters {
                sinc_len: SINC_LEN,
                // Cutoff optimal pour ce noyau et cette fenêtre. En DÉCIMATION
                // (micro en 96 kHz), rubato resserre lui-même le cutoff selon le
                // ratio → l'anti-repliement est correct dans les deux sens.
                f_cutoff: rubato::calculate_cutoff(SINC_LEN, WindowFunction::BlackmanHarris2),
                interpolation: SincInterpolationType::Cubic,
                oversampling_factor: 128,
                window: WindowFunction::BlackmanHarris2,
            };
            let ratio = VOICE_SR as f64 / sample_rate_in as f64;
            Some(
                SincFixedIn::<f32>::new(ratio, 1.1, params, RESAMPLE_CHUNK, 1).map_err(|e| {
                    VoiceCaptureError::Resampler {
                        from: sample_rate_in,
                        to: VOICE_SR,
                        detail: e.to_string(),
                    }
                })?,
            )
        };
        let out_max = resampler.as_ref().map(|r| r.output_frames_max()).unwrap_or(0);
        Ok(Self {
            channels_in,
            channel,
            resampler,
            in_acc: Vec::with_capacity(RESAMPLE_CHUNK * 4),
            in_buf: vec![vec![0.0; RESAMPLE_CHUNK]],
            out_buf: vec![vec![0.0; out_max]],
            sample_rate_in,
        })
    }

    /// Retard ajouté par la conversion, en millisecondes (0 si le périphérique
    /// est déjà en 48 kHz). Sert à l'annoncer plutôt qu'à le subir.
    pub fn added_latency_ms(&self) -> f32 {
        if self.resampler.is_none() {
            return 0.0;
        }
        let frames = RESAMPLE_CHUNK + SINC_LEN / 2;
        frames as f32 / self.sample_rate_in as f32 * 1000.0
    }

    pub fn resamples(&self) -> bool {
        self.resampler.is_some()
    }

    /// Consomme un buffer d'entrée ENTRELACÉ et émet zéro, un ou plusieurs blocs
    /// mono 48 kHz via `emit`. Une trame incomplète en fin de buffer est ignorée
    /// (elle ne peut pas exister avec un périphérique sain).
    pub fn feed(&mut self, interleaved: &[f32], emit: &mut impl FnMut(&[f32])) {
        // Sans rééchantillonnage : extraction directe, un bloc par buffer.
        if self.resampler.is_none() {
            self.in_acc.clear();
            self.in_acc.extend(
                interleaved
                    .chunks(self.channels_in)
                    .filter(|f| f.len() == self.channels_in)
                    .map(|f| f[self.channel]),
            );
            if !self.in_acc.is_empty() {
                emit(&self.in_acc);
            }
            return;
        }
        self.in_acc.extend(
            interleaved
                .chunks(self.channels_in)
                .filter(|f| f.len() == self.channels_in)
                .map(|f| f[self.channel]),
        );
        let resampler = self.resampler.as_mut().expect("branche resampler");
        while self.in_acc.len() >= RESAMPLE_CHUNK {
            self.in_buf[0].copy_from_slice(&self.in_acc[..RESAMPLE_CHUNK]);
            self.in_acc.drain(..RESAMPLE_CHUNK);
            match resampler.process_into_buffer(&self.in_buf, &mut self.out_buf, None) {
                Ok((_in_frames, out_frames)) => emit(&self.out_buf[0][..out_frames]),
                Err(e) => {
                    // Ne peut survenir que sur une incohérence de tampons ; on le
                    // DIT (jamais de silence inexpliqué) et on saute le chunk.
                    tracing::error!(
                        target: "jamodio::voice_capture",
                        error = %e,
                        "rééchantillonnage voix en échec sur un chunk"
                    );
                }
            }
        }
    }
}

/// Informations sur le flux voix ouvert — remontées à l'UI (fréquence réelle du
/// micro, rééchantillonnage actif ou non : ça se voit, ça ne se subit pas).
#[derive(Debug, Clone, Copy)]
pub struct VoiceStreamInfo {
    pub channels: u16,
    pub sample_rate: u32,
    pub resampling: bool,
    pub added_latency_ms: f32,
}

/// Ouvre le flux de capture du périphérique VOIX et pousse des blocs mono
/// 48 kHz dans `out_tx`.
///
/// `out_tx` est le MÊME canal que celui du tap sur le flux instrument : l'étage
/// voix (isolation, limiteur, Opus, RTP) ne sait pas — et n'a pas à savoir — d'où
/// viennent ses blocs.
pub fn build_voice_capture_stream(
    device: &Device,
    channel: usize,
    out_tx: Sender<Vec<f32>>,
) -> Result<(cpal::Stream, VoiceStreamInfo), VoiceCaptureError> {
    let default_cfg = device
        .default_input_config()
        .map_err(|e| VoiceCaptureError::Config(e.to_string()))?;
    let channels_in = default_cfg.channels().max(1) as usize;
    let sample_rate = default_cfg.sample_rate().0;
    let sample_format = default_cfg.sample_format();
    let mut converter = VoiceInputConverter::new(channels_in, channel, sample_rate)?;
    let info = VoiceStreamInfo {
        channels: channels_in as u16,
        sample_rate,
        resampling: converter.resamples(),
        added_latency_ms: converter.added_latency_ms(),
    };
    let config: cpal::StreamConfig = default_cfg.clone().into();

    // Blocs abandonnés d'affilée quand l'étage voix est en retard. Comme pour le
    // tap instrument : on ne bloque JAMAIS le thread audio, mais on ne jette pas
    // en silence non plus (log échantillonné, remis à zéro dès que ça repasse).
    let mut drops: u32 = 0;
    let mut send = move |block: &[f32]| match out_tx.try_send(block.to_vec()) {
        Ok(()) => drops = 0,
        Err(TrySendError::Full(_)) => {
            drops += 1;
            if drops.is_power_of_two() {
                tracing::warn!(
                    target: "jamodio::voice_capture",
                    consecutive_drops = drops,
                    "étage voix saturé — blocs talkback abandonnés"
                );
            }
        }
        Err(TrySendError::Disconnected(_)) => {}
    };

    let err_fn = |err| {
        tracing::error!(target: "jamodio::voice_capture", error = %err, "erreur du flux voix");
    };

    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _| converter.feed(data, &mut send),
            err_fn,
            None,
        )?,
        SampleFormat::I16 => {
            let mut scratch: Vec<f32> = Vec::new();
            device.build_input_stream(
                &config,
                move |data: &[i16], _| {
                    scratch.clear();
                    scratch.extend(data.iter().map(|s| *s as f32 / i16::MAX as f32));
                    converter.feed(&scratch, &mut send);
                },
                err_fn,
                None,
            )?
        }
        SampleFormat::I32 => {
            let mut scratch: Vec<f32> = Vec::new();
            device.build_input_stream(
                &config,
                move |data: &[i32], _| {
                    scratch.clear();
                    scratch.extend(data.iter().map(|s| *s as f32 / i32::MAX as f32));
                    converter.feed(&scratch, &mut send);
                },
                err_fn,
                None,
            )?
        }
        // Zéro fallback silencieux : un format inattendu est une erreur explicite.
        other => return Err(VoiceCaptureError::UnsupportedFormat(other)),
    };
    Ok((stream, info))
}

/// Poignée du flux voix : la lâcher **arrête le flux et libère le périphérique**.
///
/// Le `cpal::Stream` n'est jamais transféré entre threads : il est créé, tenu et
/// détruit par un thread propriétaire dédié. C'est plus simple et plus sûr que de
/// le rendre `Send` (le contrat de `SendStream`, côté instrument, n'existe que
/// parce qu'ASIO impose l'apartment COM créateur — la voix, elle, est WASAPI ou
/// CoreAudio, sans cette contrainte).
pub struct VoiceCaptureHandle {
    stop_tx: Sender<()>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for VoiceCaptureHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join) = self.join.take() {
            // On ATTEND la fin du thread : au changement de micro, le nouveau
            // périphérique ne doit pas s'ouvrir avant que l'ancien soit relâché.
            let _ = join.join();
        }
    }
}

/// Ouvre le périphérique voix `device_id` sur un thread propriétaire et pousse
/// des blocs mono 48 kHz dans `out_tx`.
///
/// Renvoie une erreur EXPLICITE (jamais un repli sur un autre périphérique) si
/// l'id ne résout pas, si le canal n'existe pas, ou si le flux ne s'ouvre pas.
pub fn spawn_voice_capture(
    device_id: String,
    channel: usize,
    out_tx: Sender<Vec<f32>>,
) -> Result<(VoiceStreamInfo, VoiceCaptureHandle), String> {
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<VoiceStreamInfo, String>>(1);
    let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
    let id_for_thread = device_id.clone();
    let join = std::thread::Builder::new()
        .name("voice-capture".into())
        .spawn(move || {
            let Some(device) = super::device::get_voice_input_device(&id_for_thread) else {
                let _ = ready_tx.send(Err(format!(
                    "périphérique voix introuvable ou renommé : {id_for_thread}"
                )));
                return;
            };
            let (stream, info) = match build_voice_capture_stream(&device, channel, out_tx) {
                Ok(v) => v,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            use cpal::traits::StreamTrait as _;
            if let Err(e) = stream.play() {
                let _ = ready_tx.send(Err(format!("démarrage du flux voix impossible : {e}")));
                return;
            }
            let _ = ready_tx.send(Ok(info));
            // Le thread ne fait plus que TENIR le flux en vie : les échantillons
            // arrivent par le callback du pilote. Il rend la main à l'arrêt.
            let _ = stop_rx.recv();
            // `pause()` avant destruction : sur macOS, dropper un stream d'entrée
            // n'arrête PAS l'AudioUnit (callbacks fantômes) — même piège que côté
            // instrument, cf. `SendStream::drop` dans pipeline.rs.
            if let Err(e) = stream.pause() {
                tracing::debug!(
                    target: "jamodio::voice_capture",
                    error = %e,
                    "pause() du flux voix à l'arrêt a échoué (déjà arrêté ?)"
                );
            }
            drop(stream);
            tracing::info!(target: "jamodio::voice_capture", "flux voix fermé");
        })
        .map_err(|e| format!("spawn voice-capture: {e}"))?;

    match ready_rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(Ok(info)) => {
            tracing::info!(
                target: "jamodio::voice_capture",
                device = %device_id,
                channel,
                sample_rate = info.sample_rate,
                resampling = info.resampling,
                added_latency_ms = info.added_latency_ms,
                "flux voix dédié ouvert"
            );
            Ok((
                info,
                VoiceCaptureHandle { stop_tx, join: Some(join) },
            ))
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err("le périphérique voix n'a pas répondu (10 s)".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(conv: &mut VoiceInputConverter, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        conv.feed(input, &mut |b: &[f32]| out.extend_from_slice(b));
        out
    }

    /// Sinus de `freq` Hz à `sr`, amplitude 1.
    fn sinus(n: usize, freq: f32, sr: u32) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / sr as f32).sin())
            .collect()
    }

    fn entrelace(mono: &[f32], channels: usize, channel: usize) -> Vec<f32> {
        let mut v = vec![0.0; mono.len() * channels];
        for (i, s) in mono.iter().enumerate() {
            // Les autres canaux reçoivent un signal DIFFÉRENT : si l'extraction
            // se trompe de canal, les tests le voient.
            for c in 0..channels {
                v[i * channels + c] = if c == channel { *s } else { -0.5 };
            }
        }
        v
    }

    #[test]
    fn extrait_le_bon_canal_sans_rien_changer_a_48k() {
        // Périphérique déjà en 48 kHz : aucun rééchantillonnage, donc aucun
        // retard ajouté et des échantillons IDENTIQUES à l'entrée.
        let mut conv = VoiceInputConverter::new(2, 1, 48_000).unwrap();
        assert!(!conv.resamples());
        assert_eq!(conv.added_latency_ms(), 0.0);
        let mono = sinus(256, 440.0, 48_000);
        let out = collect(&mut conv, &entrelace(&mono, 2, 1));
        assert_eq!(out, mono, "le canal 1 doit ressortir tel quel");
    }

    #[test]
    fn reechantillonne_44100_vers_48000() {
        // Le cas visé par le chantier : micro-casque en 44,1 kHz.
        let mut conv = VoiceInputConverter::new(1, 0, 44_100).unwrap();
        assert!(conv.resamples());
        let mono = sinus(44_100, 440.0, 44_100); // 1 s
        let out = collect(&mut conv, &mono);
        // Ratio de longueur ≈ 48000/44100, à un chunk près (le reliquat non
        // consommé reste dans l'accumulateur).
        let attendu = 48_000.0 * (out.len() as f32 / 48_000.0);
        assert!(
            (out.len() as f32 - 48_000.0).abs() < 600.0,
            "≈1 s à 48 kHz attendue, obtenu {} échantillons ({attendu})",
            out.len()
        );
        assert!(out.iter().all(|x| x.is_finite() && x.abs() <= 1.2));
        // Le signal doit rester un 440 Hz : on compte les passages par zéro sur
        // la partie stable (≈ 2 par période → ~880 sur 1 s).
        let stable = &out[2000..out.len() - 2000];
        let zc = stable.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        let duree = stable.len() as f32 / 48_000.0;
        let freq = zc as f32 / duree;
        assert!((freq - 440.0).abs() < 5.0, "fréquence après conversion = {freq} Hz");
    }

    #[test]
    fn reechantillonne_16000_vers_48000() {
        // Micro-casque en mode « communications » : 16 kHz.
        let mut conv = VoiceInputConverter::new(2, 0, 16_000).unwrap();
        let mono = sinus(16_000, 300.0, 16_000);
        let out = collect(&mut conv, &entrelace(&mono, 2, 0));
        assert!(
            (out.len() as i32 - 48_000).abs() < 1200,
            "≈3× plus d'échantillons attendus, obtenu {}",
            out.len()
        );
        let stable = &out[2000..out.len() - 2000];
        let zc = stable.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        let freq = zc as f32 / (stable.len() as f32 / 48_000.0);
        assert!((freq - 300.0).abs() < 5.0, "fréquence après conversion = {freq} Hz");
    }

    #[test]
    fn le_retard_ajoute_est_annonce_et_reste_petit() {
        let conv = VoiceInputConverter::new(1, 0, 44_100).unwrap();
        let ms = conv.added_latency_ms();
        assert!(ms > 0.0 && ms < 6.0, "retard de conversion = {ms} ms");
    }

    #[test]
    fn canal_hors_bornes_est_une_erreur_explicite() {
        // Doctrine : pas de repli silencieux sur le canal 0.
        let err = match VoiceInputConverter::new(2, 5, 48_000) {
            Err(e) => e,
            Ok(_) => panic!("un canal hors bornes doit être refusé"),
        };
        assert!(matches!(
            err,
            VoiceCaptureError::ChannelOutOfRange { requested: 5, available: 2 }
        ));
        assert!(VoiceInputConverter::new(0, 0, 48_000).is_err());
    }

    #[test]
    fn tolere_des_tailles_de_buffer_irregulieres() {
        // Un pilote peut livrer des buffers de taille variable ; aucune trame
        // incomplète ne doit provoquer de panique ni de décalage de canal.
        let mut conv = VoiceInputConverter::new(2, 1, 44_100).unwrap();
        let mono = sinus(9_000, 440.0, 44_100);
        let entrelace = entrelace(&mono, 2, 1);
        let mut out = Vec::new();
        let mut pos = 0;
        for taille in [130usize, 7, 512, 65, 1024, 3] .iter().cycle().take(60) {
            let fin = (pos + taille * 2).min(entrelace.len());
            if pos >= fin { break; }
            conv.feed(&entrelace[pos..fin], &mut |b: &[f32]| out.extend_from_slice(b));
            pos = fin;
        }
        assert!(!out.is_empty());
        assert!(out.iter().all(|x| x.is_finite()));
    }
}

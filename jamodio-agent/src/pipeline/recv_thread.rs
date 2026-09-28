//! Fil de réception DÉDIÉ — Lot 1-D3 du plan « ms en trop sur PC »
//! (`PLAN-LOT1-MS-PC-2026-09.md`, dépôt du site).
//!
//! # Pourquoi
//!
//! Jusqu'en 0.6.6-6, chaque flux reçu était lu par une tâche tokio ordinaire.
//! Banc du 28/09/2026 (Mac de Ben, faux serveur précis à 0,19 ms, CPU à 16 %) :
//! des paquets déjà reçus par la machine attendaient 6,5 à 11 ms avant d'être
//! lus (horodatage noyau, Lot 1-D2) — assez pour vider le tampon. Même symptôme
//! sur PC (trous simultanés sur plusieurs flux, 26/09). Une question de PRIORITÉ :
//! le fil qui lit n'était pas dans la bande audio, le fil qui décode l'était.
//!
//! # Ce que fait ce fil
//!
//! UN fil, promu comme le décodage (`promote_thread_for_audio_recv`), surveille
//! TOUTES les sockets reçues par `mio` (kqueue / IOCP). Il reprend exactement ce
//! que faisait la tâche de chaque flux :
//! - lecture dès que le système signale la socket, jusqu'à la vider ; horodatage
//!   d'arrivée (`recv_instant`) et attente système → lecture ; déchiffrement ;
//! - envoi au thread de décodage, par le MÊME canal qu'avant ;
//! - perçage comedia (toutes les 100 ms, 30 fois au plus, jusqu'au 1er paquet) ;
//! - activité du flux (silences, erreurs) et journal des silences d'instrument ;
//! - au retrait d'un flux, `Remove` envoyé au décodage APRÈS son dernier paquet
//!   (même fil émetteur : l'ordre est garanti).
//!
//! Une réception en erreur répétée met SON flux en pause (`recv_error_backoff`,
//! N13), jamais les autres, et jamais de boucle serrée à priorité audio.
//!
//! Aucun étage n'est ajouté au trajet du son : on retire au contraire le réveil
//! d'une tâche tokio par paquet.

use super::DecodeMsg;
use crate::recv_activity::{recv_error_backoff, RecvActivity, SILENCE_LOG_AFTER_MS};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use jamodio_audio_core::net::udp::RtpReceiver;
use jamodio_audio_core::perfstats::Histogram;
use jamodio_audio_core::protocol::StreamKind;
use mio::{Events, Interest, Poll, Token, Waker};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Jeton réservé au réveil par une commande.
const WAKER: Token = Token(0);
/// Perçage comedia : cadence et nombre maximal (3 s, marge pour le
/// `connect-plain-transport` du navigateur) — inchangés.
const PUNCH_EVERY: Duration = Duration::from_millis(100);
const PUNCH_MAX: u32 = 30;
/// Relevé des silences d'instrument (journal seulement).
const SILENCE_CHECK_EVERY: Duration = Duration::from_secs(1);
/// Attente maximale du fil sans événement : il revient au moins à cette cadence
/// pour ses échéances (perçage, silences, reprises après erreur).
const MAX_WAIT: Duration = Duration::from_secs(1);

/// Un flux à recevoir, tel que `PipelineState::add_stream` le prépare.
pub(super) struct NewStream {
    pub producer_id: Arc<str>,
    pub epoch: u64,
    pub kind: StreamKind,
    pub receiver: RtpReceiver,
    pub sfu_addr: SocketAddr,
    pub activity: Arc<RecvActivity>,
}

pub(super) enum RecvCmd {
    Add(NewStream),
    /// Retire le flux de cette génération (un `Remove` d'une génération
    /// ancienne ne touche pas un flux recréé).
    Remove { producer_id: Arc<str>, epoch: u64 },
}

/// Handle du fil de réception, détenu par `PipelineState`.
pub(super) struct RecvThread {
    cmd_tx: Sender<RecvCmd>,
    waker: Arc<Waker>,
    join: std::thread::JoinHandle<()>,
}

impl RecvThread {
    /// Démarre le fil. `decode_tx` / `pool_rx` : le canal et la réserve de
    /// tampons du thread de décodage.
    pub fn spawn(
        decode_tx: Sender<DecodeMsg>,
        pool_rx: Receiver<Vec<u8>>,
        stack_delay_hist: Arc<Mutex<Histogram>>,
    ) -> std::io::Result<Self> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKER)?);
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let join = std::thread::Builder::new()
            .name("audio-recv".into())
            .spawn(move || recv_loop(poll, cmd_rx, decode_tx, pool_rx, stack_delay_hist))?;
        Ok(Self { cmd_tx, waker, join })
    }

    pub fn send(&self, cmd: RecvCmd) {
        // Le fil ne s'arrête qu'avec `shutdown` : l'envoi ne peut pas échouer
        // avant. Le réveil fait traiter la commande tout de suite.
        let _ = self.cmd_tx.send(cmd);
        if let Err(e) = self.waker.wake() {
            tracing::error!(target: "jamodio::recv", error = %e, "réveil du fil de réception impossible");
        }
    }

    /// Arrête le fil : chaque flux encore là est retiré (`Remove` envoyé au
    /// décodage après son dernier paquet), puis le fil se termine.
    pub fn shutdown(self) {
        drop(self.cmd_tx);
        let _ = self.waker.wake();
        let _ = self.join.join();
    }
}

/// État d'un flux dans le fil.
struct Stream {
    producer_id: Arc<str>,
    epoch: u64,
    kind: StreamKind,
    receiver: RtpReceiver,
    sfu_addr: SocketAddr,
    activity: Arc<RecvActivity>,
    got_first: bool,
    punches_left: u32,
    next_punch: Instant,
    consecutive_errors: u32,
    /// Pause après des erreurs enchaînées : on ne relit pas avant.
    retry_at: Option<Instant>,
    silence_logged: bool,
}

impl Stream {
    fn short(&self) -> &str {
        &self.producer_id[..8.min(self.producer_id.len())]
    }
}

/// Le décodage est parti : plus rien à faire.
struct DecodeGone;

fn recv_loop(
    mut poll: Poll,
    cmd_rx: Receiver<RecvCmd>,
    decode_tx: Sender<DecodeMsg>,
    pool_rx: Receiver<Vec<u8>>,
    stack_delay_hist: Arc<Mutex<Histogram>>,
) {
    // Même promotion que le décodage : bande temps réel (macOS, contrainte de
    // temps légère), MMCSS « Pro Audio » (Windows).
    let _rt = crate::audio::rt_priority::promote_thread_for_audio_recv();
    let mut streams: HashMap<Token, Stream> = HashMap::new();
    let mut next_token = 1usize;
    let mut events = Events::with_capacity(64);
    // Tampon courant (recyclé via la réserve). 2048 ≥ MTU + tag SRTP + en-tête RTP.
    let mut buf: Vec<u8> = pool_rx.try_recv().unwrap_or_else(|_| Vec::with_capacity(2048));
    let mut next_silence_check = Instant::now() + SILENCE_CHECK_EVERY;

    loop {
        // Prochaine échéance : perçage, reprise après erreur, relevé des silences.
        let now = Instant::now();
        let mut due = next_silence_check.min(now + MAX_WAIT);
        for st in streams.values() {
            if st.punches_left > 0 {
                due = due.min(st.next_punch);
            }
            if let Some(t) = st.retry_at {
                due = due.min(t);
            }
        }
        if let Err(e) = poll.poll(&mut events, Some(due.saturating_duration_since(now))) {
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            tracing::error!(target: "jamodio::recv", error = %e, "attente des sockets impossible — réception arrêtée");
            break;
        }

        // 1. Les sockets prêtes : lues jusqu'à être vides (attente « sur front »).
        for ev in events.iter() {
            if ev.token() == WAKER {
                continue;
            }
            let Some(st) = streams.get_mut(&ev.token()) else { continue };
            // En pause après des erreurs : la reprise relira (point 3).
            if st.retry_at.is_some() {
                continue;
            }
            if drain(st, &mut buf, &decode_tx, &pool_rx, &stack_delay_hist).is_err() {
                return;
            }
        }

        // 2. Les commandes.
        loop {
            match cmd_rx.try_recv() {
                Ok(RecvCmd::Add(ns)) => {
                    let token = Token(next_token);
                    next_token += 1;
                    let mut ns = ns;
                    if let Err(e) = poll.registry().register(ns.receiver.source(), token, Interest::READABLE) {
                        // Le flux resterait muet sans que rien ne le dise : on le dit.
                        tracing::error!(target: "jamodio::recv", producer = %ns.producer_id, error = %e, "flux non reçu : inscription de la socket impossible");
                        continue;
                    }
                    streams.insert(
                        token,
                        Stream {
                            producer_id: ns.producer_id,
                            epoch: ns.epoch,
                            kind: ns.kind,
                            receiver: ns.receiver,
                            sfu_addr: ns.sfu_addr,
                            activity: ns.activity,
                            got_first: false,
                            punches_left: PUNCH_MAX,
                            next_punch: Instant::now(),
                            consecutive_errors: 0,
                            retry_at: None,
                            silence_logged: false,
                        },
                    );
                }
                Ok(RecvCmd::Remove { producer_id, epoch }) => {
                    let token = streams
                        .iter()
                        .find(|(_, st)| st.producer_id == producer_id && st.epoch == epoch)
                        .map(|(t, _)| *t);
                    if let Some(t) = token {
                        if remove(&mut poll, &mut streams, t, &decode_tx).is_err() {
                            return;
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                // `shutdown` : tout retirer, dans l'ordre, puis finir.
                Err(TryRecvError::Disconnected) => {
                    let tokens: Vec<Token> = streams.keys().copied().collect();
                    for t in tokens {
                        if remove(&mut poll, &mut streams, t, &decode_tx).is_err() {
                            return;
                        }
                    }
                    return;
                }
            }
        }

        // 3. Les échéances : reprises après erreur, perçages, silences.
        let now = Instant::now();
        for st in streams.values_mut() {
            if st.retry_at.is_some_and(|t| t <= now) {
                st.retry_at = None;
                if drain(st, &mut buf, &decode_tx, &pool_rx, &stack_delay_hist).is_err() {
                    return;
                }
            }
            if st.punches_left > 0 && st.next_punch <= now {
                // Un perçage perdu est rattrapé 100 ms plus tard ; l'erreur ne dit
                // rien de plus que ce que la reprise (ou non) du flux dira.
                let _ = st.receiver.punch(st.sfu_addr);
                st.punches_left -= 1;
                st.next_punch = now + PUNCH_EVERY;
            }
        }
        if now >= next_silence_check {
            next_silence_check = now + SILENCE_CHECK_EVERY;
            for st in streams.values_mut() {
                // La voix se tait légitimement dès que personne ne parle ;
                // l'instrument envoie en continu, même quand on ne joue pas.
                if st.kind != StreamKind::Instrument || !st.got_first || st.silence_logged {
                    continue;
                }
                let silent_ms = st.activity.silent_ms(now);
                if silent_ms >= SILENCE_LOG_AFTER_MS {
                    tracing::warn!(target: "jamodio::recv", producer = st.short(), silent_ms, "aucun paquet reçu — flux conservé, reprise automatique à leur retour");
                    st.silence_logged = true;
                }
            }
        }
    }
}

/// Lit tout ce que la socket de `st` contient et l'envoie au décodage.
fn drain(
    st: &mut Stream,
    buf: &mut Vec<u8>,
    decode_tx: &Sender<DecodeMsg>,
    pool_rx: &Receiver<Vec<u8>>,
    stack_delay_hist: &Arc<Mutex<Histogram>>,
) -> Result<(), DecodeGone> {
    loop {
        match st.receiver.read(buf) {
            Ok(r) if r.len > 0 => {
                // Horodatage d'arrivée — ICI, avant tout parse/file (load-bearing).
                let recv_instant = Instant::now();
                // Lot 1-D2 — attente système → lecture (mesure seule).
                if let Some(d) = r.stack_delay {
                    stack_delay_hist.lock().observe(d.as_secs_f32() * 1000.0);
                }
                if st.silence_logged {
                    let silent_ms = st.activity.silent_ms(recv_instant);
                    tracing::info!(target: "jamodio::recv", producer = st.short(), silent_ms, "paquets revenus après un silence");
                    st.silence_logged = false;
                }
                st.activity.mark_packet(recv_instant);
                st.consecutive_errors = 0;
                // 1er paquet valide : comedia activé → on cesse de percer.
                if !st.got_first {
                    st.got_first = true;
                    st.punches_left = 0;
                }
                // Échange le tampon plein contre un neuf (réserve) et envoie le
                // plein au décodage.
                let fresh = pool_rx.try_recv().unwrap_or_else(|_| Vec::with_capacity(2048));
                let full = std::mem::replace(buf, fresh);
                decode_tx
                    .send(DecodeMsg::Packet {
                        producer_id: st.producer_id.clone(),
                        epoch: st.epoch,
                        recv_instant,
                        stack_delay: r.stack_delay,
                        buf: full,
                        kind: st.kind,
                    })
                    .map_err(|_| DecodeGone)?;
            }
            // RTCP filtré / échec SRTP (déjà journalisé) : on réutilise le tampon.
            Ok(_) => {}
            // Socket vide : jusqu'au prochain signal du système.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) => {
                st.activity.mark_recv_error();
                st.consecutive_errors = st.consecutive_errors.saturating_add(1);
                // Une erreur isolée est journalisée (elle reste un fait) ; une
                // rafale ne l'est plus qu'au début, sinon le journal devient
                // illisible au moment précis où on le lit.
                if st.consecutive_errors <= 3 {
                    tracing::warn!(
                        target: "jamodio::recv",
                        producer = %st.producer_id,
                        error = %e,
                        consecutive = st.consecutive_errors,
                        "erreur de réception UDP"
                    );
                }
                // N13 : la première erreur ne coûte rien ; des erreurs enchaînées
                // mettent CE flux en pause, jamais les autres.
                let wait = recv_error_backoff(st.consecutive_errors);
                if !wait.is_zero() {
                    st.retry_at = Some(Instant::now() + wait);
                    return Ok(());
                }
            }
        }
    }
}

/// Désinscrit un flux et prévient le décodage, après son dernier paquet.
fn remove(
    poll: &mut Poll,
    streams: &mut HashMap<Token, Stream>,
    token: Token,
    decode_tx: &Sender<DecodeMsg>,
) -> Result<(), DecodeGone> {
    let Some(mut st) = streams.remove(&token) else { return Ok(()) };
    if let Err(e) = poll.registry().deregister(st.receiver.source()) {
        tracing::warn!(target: "jamodio::recv", producer = st.short(), error = %e, "désinscription de la socket");
    }
    decode_tx
        .send(DecodeMsg::Remove { producer_id: st.producer_id.clone(), epoch: st.epoch })
        .map_err(|_| DecodeGone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamodio_audio_core::net::rtp::{self, RtpHeader};
    use jamodio_audio_core::net::srtp::{SrtpContext, SrtpParameters};
    use std::net::UdpSocket;

    /// Un faux serveur : sa socket et son contexte de chiffrement vers l'agent.
    struct Serveur {
        socket: UdpSocket,
        ctx: SrtpContext,
    }

    fn banc() -> (RecvThread, Receiver<DecodeMsg>, Serveur, NewStream, Arc<Mutex<Histogram>>) {
        let (decode_tx, decode_rx) = crossbeam_channel::bounded(256);
        let (pool_tx, pool_rx) = crossbeam_channel::bounded(16);
        for _ in 0..16 {
            let _ = pool_tx.try_send(Vec::with_capacity(2048));
        }
        let hist = Arc::new(Mutex::new(Histogram::new(64)));
        let rt = RecvThread::spawn(decode_tx, pool_rx, hist.clone()).unwrap();

        let server_keys = SrtpParameters::generate_aead_aes_256_gcm();
        let agent_keys = SrtpParameters::generate_aead_aes_256_gcm();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let receiver = RtpReceiver::new(Arc::new(SrtpContext::new(&agent_keys, &server_keys).unwrap())).unwrap();
        let ns = NewStream {
            producer_id: Arc::from("peer-a"),
            epoch: 7,
            kind: StreamKind::Instrument,
            receiver,
            sfu_addr: socket.local_addr().unwrap(),
            activity: Arc::new(RecvActivity::new(Instant::now())),
        };
        let serveur = Serveur { socket, ctx: SrtpContext::new(&server_keys, &agent_keys).unwrap() };
        (rt, decode_rx, serveur, ns, hist)
    }

    /// Le serveur attend le perçage (comedia), puis envoie `n` paquets à l'agent.
    fn percer_puis_envoyer(s: &Serveur, n: u16) {
        let mut buf = [0u8; 2048];
        let (_, agent) = s.socket.recv_from(&mut buf).expect("le fil perce vers le serveur");
        for seq in 0..n {
            let h = RtpHeader { payload_type: 111, sequence: seq, timestamp: u32::from(seq) * 120, ssrc: 9, marker: false };
            let mut p = rtp::build_packet(&h, &[0u8; 100]);
            s.ctx.protect(&mut p).unwrap();
            s.socket.send_to(&p, agent).unwrap();
        }
    }

    fn paquets(rx: &Receiver<DecodeMsg>, n: usize) -> Vec<(u16, Instant, Option<Duration>)> {
        (0..n)
            .map(|_| match rx.recv_timeout(Duration::from_secs(2)).expect("paquet transmis au décodage") {
                DecodeMsg::Packet { producer_id, epoch, recv_instant, stack_delay, buf, kind } => {
                    assert_eq!((&*producer_id, epoch, kind), ("peer-a", 7, StreamKind::Instrument));
                    (rtp::parse_header(&buf).unwrap().0.sequence, recv_instant, stack_delay)
                }
                _ => panic!("un paquet était attendu"),
            })
            .collect()
    }

    /// Le fil perce, lit, déchiffre, date et transmet au décodage, dans l'ordre.
    #[test]
    fn le_fil_recoit_et_transmet_au_decodage_dans_l_ordre() {
        let (rt, decode_rx, serveur, ns, hist) = banc();
        let activity = ns.activity.clone();
        rt.send(RecvCmd::Add(ns));
        percer_puis_envoyer(&serveur, 20);
        let p = paquets(&decode_rx, 20);
        assert_eq!(p.iter().map(|x| x.0).collect::<Vec<_>>(), (0..20).collect::<Vec<u16>>());
        assert!(p.windows(2).all(|w| w[1].1 >= w[0].1), "horodatages croissants");
        if cfg!(target_os = "macos") {
            assert!(p.iter().all(|x| x.2.is_some()), "attente système → lecture mesurée");
            assert!(!hist.lock().is_empty());
        }
        assert!(activity.silent_ms(Instant::now()) < 1_000, "activité marquée");
        rt.shutdown();
    }

    /// Le retrait part au décodage APRÈS le dernier paquet du flux.
    #[test]
    fn le_retrait_suit_le_dernier_paquet() {
        let (rt, decode_rx, serveur, ns, _) = banc();
        rt.send(RecvCmd::Add(ns));
        percer_puis_envoyer(&serveur, 5);
        paquets(&decode_rx, 5);
        rt.send(RecvCmd::Remove { producer_id: Arc::from("peer-a"), epoch: 7 });
        match decode_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
            DecodeMsg::Remove { producer_id, epoch } => assert_eq!((&*producer_id, epoch), ("peer-a", 7)),
            _ => panic!("un retrait était attendu"),
        }
        rt.shutdown();
    }

    /// Un retrait d'une AUTRE génération ne touche pas le flux en cours.
    #[test]
    fn un_retrait_d_une_autre_generation_est_ignore() {
        let (rt, decode_rx, serveur, ns, _) = banc();
        rt.send(RecvCmd::Add(ns));
        rt.send(RecvCmd::Remove { producer_id: Arc::from("peer-a"), epoch: 6 });
        percer_puis_envoyer(&serveur, 3);
        assert_eq!(paquets(&decode_rx, 3).len(), 3, "le flux reçoit toujours");
        rt.shutdown();
    }

    /// L'arrêt retire les flux restants auprès du décodage, puis le fil finit.
    #[test]
    fn l_arret_retire_les_flux_restants() {
        let (rt, decode_rx, _serveur, ns, _) = banc();
        rt.send(RecvCmd::Add(ns));
        rt.shutdown();
        let mut removed = false;
        while let Ok(m) = decode_rx.recv_timeout(Duration::from_millis(500)) {
            if let DecodeMsg::Remove { producer_id, epoch } = m {
                assert_eq!((&*producer_id, epoch), ("peer-a", 7));
                removed = true;
            }
        }
        assert!(removed, "le décodage apprend la fin du flux");
    }
}

//! Fil UNIQUE de réception et de décodage — Lot 1-D4 du plan « ms en trop »
//! (`PLAN-LOT1-MS-PC-2026-09.md`, dépôt du site).
//!
//! # Pourquoi
//!
//! Jusqu'en 0.6.6-6, chaque flux reçu était lu par une tâche tokio ordinaire :
//! paquets lus 6,5 à 11 ms en retard (banc du 28/09/2026, Mac, horodatage
//! noyau). En 0.6.6-7/-8, un fil de lecture prioritaire dédié les remettait à un
//! fil de décodage prioritaire par un canal : la lecture était corrigée, mais le
//! paquet attendait jusqu'à 5,5 ms dans le canal (Mac, `decode_queue_ms`), et
//! l'essai inverse (lecture en priorité normale) faisait lire lentement et
//! réveiller le décodage jusqu'à 10 ms en retard. Déduit : deux fils qui se
//! passent le relais se retiennent l'un l'autre. Ici, il n'y a plus de relais.
//!
//! # Ce que fait ce fil
//!
//! UN fil, promu en priorité audio (`promote_thread_for_audio_recv`), surveille
//! TOUTES les sockets reçues par `mio` (kqueue / IOCP) et fait tout, à la suite :
//! - lecture dès que le système signale la socket, les sockets prêtes À TOUR
//!   DE RÔLE (un paquet chacune par passe) jusqu'à les vider ; horodatage
//!   d'arrivée (`recv_instant`) et attente système → lecture ; déchiffrement ;
//!   décodage et push au mélangeur (`RxCore`), sans canal ;
//! - échéances de masquage (le délai d'attente du fil = le prochain examen) ;
//!   avant d'inventer une trame pour un flux, sa socket est relue : un paquet
//!   déjà dans la machine n'est jamais remplacé (M0) ;
//! - perçage comedia (toutes les 100 ms, 30 fois au plus, jusqu'au 1er paquet) ;
//! - activité du flux (silences, erreurs) et journal des silences d'instrument ;
//! - retrait d'un flux : socket désinscrite, état, stream du mélangeur et
//!   statistiques retirés au même endroit — rien ne peut suivre son dernier
//!   paquet.
//!
//! Une réception en erreur répétée met SON flux en pause (`recv_error_backoff`,
//! N13), jamais les autres, et jamais de boucle serrée à priorité audio. Un
//! flux en pause reste couvert par le masquage.
//!
//! Aucun étage n'est ajouté au trajet du son : on retire au contraire un
//! passage par un canal et un réveil de fil par paquet.

use super::RxCore;
use crate::audio::rt_priority::AUDIO_RECV_COMPUTATION;
use crate::recv_activity::{recv_error_backoff, RecvActivity, SILENCE_LOG_AFTER_MS};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use jamodio_audio_core::net::udp::RtpReceiver;
use jamodio_audio_core::perfstats::Histogram;
use jamodio_audio_core::protocol::StreamKind;
use mio::{Events, Interest, Poll, Token, Waker};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Mesures tenues par le fil lui-même (lues à 1 Hz, cf. `PerfHandles`).
pub(super) struct RxMeasures {
    /// Lot 1-D2 — attente système → lecture.
    pub stack_delay: Arc<Mutex<Histogram>>,
    /// Lot 1-C — retard du réveil sur l'instant prévu, échéance armée.
    pub wake_late: Arc<Mutex<Histogram>>,
    /// Lot 1-D4 — travail par réveil, et réveils au-delà du contrat de calcul.
    pub wake_work: Arc<Mutex<Histogram>>,
    pub wake_over_budget: Arc<AtomicU64>,
}

/// Handle du fil de réception, détenu par `PipelineState`.
pub(super) struct RecvThread {
    cmd_tx: Sender<RecvCmd>,
    waker: Arc<Waker>,
    join: std::thread::JoinHandle<()>,
}

impl RecvThread {
    /// Démarre le fil, qui détient désormais `core` (seul écrivain du mélangeur
    /// côté pairs).
    pub fn spawn(core: RxCore, measures: RxMeasures) -> std::io::Result<Self> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKER)?);
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let join = std::thread::Builder::new()
            .name("audio-recv".into())
            .spawn(move || recv_loop(poll, cmd_rx, core, measures))?;
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

    /// Arrête le fil : chaque flux encore là est retiré (socket, état, stream
    /// du mélangeur, statistiques), puis le fil se termine.
    pub fn shutdown(self) {
        drop(self.cmd_tx);
        let _ = self.waker.wake();
        let _ = self.join.join();
    }
}

/// État réseau d'un flux dans le fil (son décodage vit dans `RxCore`).
pub(super) struct Stream {
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
    pub(super) fn new(ns: NewStream) -> Self {
        Self {
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
        }
    }

    fn short(&self) -> &str {
        &self.producer_id[..8.min(self.producer_id.len())]
    }
}

/// Ce que le fil lit et écrit à chaque paquet, rassemblé pour que la lecture
/// tienne en fonctions testables.
pub(super) struct Rx<'a> {
    pub core: &'a mut RxCore,
    /// Tampon de lecture, réutilisé : aucune allocation par paquet.
    pub buf: &'a mut Vec<u8>,
    pub stack_delay: &'a Mutex<Histogram>,
    /// Bloc de sortie (ms), relu une fois par réveil.
    pub output_block_ms: f64,
    /// Chantier P2 — paquets lus pendant ce réveil.
    pub packets: u32,
}

/// Chantier P2 — au-delà, un réveil du fil de réception est découpé au journal.
const LONG_WAKE: std::time::Duration = std::time::Duration::from_millis(2);

fn recv_loop(mut poll: Poll, cmd_rx: Receiver<RecvCmd>, mut core: RxCore, m: RxMeasures) {
    // Bande temps réel (macOS, contrainte de temps légère), MMCSS « Pro Audio »
    // (Windows).
    let _rt = crate::audio::rt_priority::promote_thread_for_audio_recv("réception et décodage");
    let mut streams: HashMap<Token, Stream> = HashMap::new();
    let mut next_token = 1usize;
    let mut events = Events::with_capacity(64);
    // 2048 ≥ MTU + tag SRTP + en-tête RTP.
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    // Sockets à lire dans ce réveil (≤ 16 flux : aucune allocation ensuite).
    let mut ready: Vec<Token> = Vec::with_capacity(16);
    let mut next_silence_check = Instant::now() + SILENCE_CHECK_EVERY;

    loop {
        // Prochaine échéance : examen de masquage, perçage, reprise après
        // erreur, relevé des silences.
        let slept_at = Instant::now();
        let mut due = next_silence_check.min(slept_at + MAX_WAIT);
        for st in streams.values() {
            if st.punches_left > 0 {
                due = due.min(st.next_punch);
            }
            if let Some(t) = st.retry_at {
                due = due.min(t);
            }
        }
        let wait = core.next_wait(slept_at).min(due.saturating_duration_since(slept_at));
        // Lot 1-C — une échéance de masquage est armée : ce réveil est celui dont
        // dépend le masquage. (Sans elle, son heure n'importe à personne.)
        let deadline_armed = core.deadline_armed();
        if let Err(e) = poll.poll(&mut events, Some(wait)) {
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            tracing::error!(target: "jamodio::recv", error = %e, "attente des sockets impossible — réception arrêtée");
            // `core` part avec le fil : son `Drop` retire les pairs du mélangeur.
            return;
        }
        let woke = Instant::now();
        if events.is_empty() && deadline_armed {
            // De combien le réveil a dépassé l'instant prévu : l'imprécision que
            // `conceal::WAKE_SLACK_MS` doit couvrir, mesurée avec la vraie
            // primitive d'attente de ce fil.
            let late = woke.saturating_duration_since(slept_at).saturating_sub(wait);
            m.wake_late.lock().observe(late.as_secs_f32() * 1000.0);
        }
        let output_block_ms = core.output_block_ms();
        // Chantier P2 — de quoi découper un réveil long (cf. fin de boucle).
        let (log_before, holes_before) = core.hole_reporting();
        let mut rx = Rx { core: &mut core, buf: &mut buf, stack_delay: &m.stack_delay, output_block_ms, packets: 0 };

        // 1. Les sockets prêtes, lues à tour de rôle jusqu'à être vides (attente
        //    « sur front »). En pause après des erreurs : la reprise relira (3).
        ready.clear();
        for ev in events.iter() {
            let t = ev.token();
            if t != WAKER && !ready.contains(&t) && streams.get(&t).is_some_and(|st| st.retry_at.is_none()) {
                ready.push(t);
            }
        }
        read_ready(&mut ready, &mut streams, &mut rx);
        let read_done = Instant::now();

        // 2. Les commandes.
        loop {
            match cmd_rx.try_recv() {
                Ok(RecvCmd::Add(ns)) => {
                    let token = Token(next_token);
                    next_token += 1;
                    let mut st = Stream::new(ns);
                    if let Err(e) = poll.registry().register(st.receiver.source(), token, Interest::READABLE) {
                        // Le flux resterait muet sans que rien ne le dise : on le dit.
                        tracing::error!(target: "jamodio::recv", producer = %st.producer_id, error = %e, "flux non reçu : inscription de la socket impossible");
                        continue;
                    }
                    streams.insert(token, st);
                }
                Ok(RecvCmd::Remove { producer_id, epoch }) => {
                    let token = streams
                        .iter()
                        .find(|(_, st)| st.producer_id == producer_id && st.epoch == epoch)
                        .map(|(t, _)| *t);
                    if let Some(t) = token {
                        remove(&mut poll, &mut streams, t, rx.core);
                    }
                }
                Err(TryRecvError::Empty) => break,
                // `shutdown` : tout retirer, puis finir.
                Err(TryRecvError::Disconnected) => {
                    let tokens: Vec<Token> = streams.keys().copied().collect();
                    for t in tokens {
                        remove(&mut poll, &mut streams, t, rx.core);
                    }
                    return;
                }
            }
        }

        // 3. Les échéances réseau : reprises après erreur, perçages, silences.
        let now = Instant::now();
        let commands_done = now;
        ready.clear();
        for (t, st) in streams.iter_mut() {
            if st.retry_at.is_some_and(|r| r <= now) {
                st.retry_at = None;
                ready.push(*t);
            }
            if st.punches_left > 0 && st.next_punch <= now {
                // Un perçage perdu est rattrapé 100 ms plus tard ; l'erreur ne dit
                // rien de plus que ce que la reprise (ou non) du flux dira.
                let _ = st.receiver.punch(st.sfu_addr);
                st.punches_left -= 1;
                st.next_punch = now + PUNCH_EVERY;
            }
        }
        read_ready(&mut ready, &mut streams, &mut rx);
        if now >= next_silence_check {
            next_silence_check = now + SILENCE_CHECK_EVERY;
            log_silences(&mut streams, now);
        }

        // 4. Le masquage, après avoir relu les sockets des flux qu'il va juger.
        let network_done = Instant::now();
        conceal_pass(&mut ready, &mut streams, &mut rx, Instant::now);

        // Lot 1-D4 — le travail de ce réveil, face au contrat de calcul du fil.
        let work = woke.elapsed();
        m.wake_work.lock().observe(work.as_secs_f32() * 1000.0);
        if work > AUDIO_RECV_COMPUTATION {
            m.wake_over_budget.fetch_add(1, Ordering::Relaxed);
        }
        // Chantier P2 (02/10/2026) — un réveil long se découpe : à quoi le fil
        // a-t-il passé ce temps, pendant que les paquets des autres flux
        // attendaient ? (01/10 : 9,5 ms de travail au moment de 6 trous
        // « réception » simultanés.) Journal seulement pour ces réveils-là,
        // rares (~70 secondes sur une journée de banc) : rien sur le chemin courant.
        if work > LONG_WAKE {
            let packets = rx.packets;
            let (log_after, holes_after) = rx.core.hole_reporting();
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
            tracing::info!(
                target: "jamodio::recv",
                work_ms = ms(work),
                read_ms = ms(read_done.saturating_duration_since(woke)),
                commands_ms = ms(commands_done.saturating_duration_since(read_done)),
                network_ms = ms(network_done.saturating_duration_since(commands_done)),
                conceal_ms = ms(Instant::now().saturating_duration_since(network_done)),
                hole_log_ms = ms(log_after.saturating_sub(log_before)),
                packets,
                holes = holes_after.saturating_sub(holes_before),
                "réveil long du fil de réception — découpe (P2)"
            );
        }
    }
}

/// M0 — avant de décider de masquer, on relit la socket de chaque flux que le
/// masquage va juger : un paquet déjà dans la machine est joué, jamais remplacé
/// par une trame inventée puis écarté comme « en retard ». Puis on décide, à
/// l'heure d'après cette lecture (`now` est relu une fois les sockets vidées).
pub(super) fn conceal_pass(
    ready: &mut Vec<Token>,
    streams: &mut HashMap<Token, Stream>,
    rx: &mut Rx<'_>,
    now: impl Fn() -> Instant,
) {
    let t = now();
    ready.clear();
    for (token, st) in streams.iter() {
        if st.retry_at.is_none() && rx.core.conceal_due(&st.producer_id, st.epoch, t) {
            ready.push(*token);
        }
    }
    read_ready(ready, streams, rx);
    let block_ms = rx.output_block_ms;
    rx.core.conceal(now(), block_ms);
}

/// Lit les sockets de `ready` jusqu'à les vider, À TOUR DE RÔLE.
fn read_ready(ready: &mut Vec<Token>, streams: &mut HashMap<Token, Stream>, rx: &mut Rx<'_>) {
    round_robin(ready, |t| streams.get_mut(&t).is_some_and(|st| read_one(st, rx)));
}

/// Sert les sources de `ready` à tour de rôle : `take(s)` en prend UN élément
/// et rend `true` s'il faut revenir à cette source, `false` quand elle est
/// épuisée (elle quitte alors la liste). Un flux qui arrive en rafale (après un
/// accroc réseau, une dizaine de paquets ≈ 1 ms de décodage) ne passe donc pas
/// devant les paquets des autres, déjà là.
fn round_robin<T: Copy>(ready: &mut Vec<T>, mut take: impl FnMut(T) -> bool) {
    while !ready.is_empty() {
        let mut i = 0;
        while i < ready.len() {
            if take(ready[i]) {
                i += 1;
            } else {
                ready.remove(i);
            }
        }
    }
}

/// Lit UN datagramme de la socket de `st` et, si c'est un paquet RTP valide,
/// le décode et le pousse. `true` = il peut en rester, `false` = socket vide,
/// ou flux mis en pause après des erreurs.
fn read_one(st: &mut Stream, rx: &mut Rx<'_>) -> bool {
    match st.receiver.read(rx.buf) {
        Ok(r) if r.len > 0 => {
            // Horodatage d'arrivée — ICI, avant tout parse (load-bearing).
            let recv_instant = Instant::now();
            rx.packets += 1;
            // Lot 1-D2 — attente système → lecture (mesure seule).
            if let Some(d) = r.stack_delay {
                rx.stack_delay.lock().observe(d.as_secs_f32() * 1000.0);
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
            rx.core.on_packet(
                &st.producer_id,
                st.epoch,
                st.kind,
                recv_instant,
                r.stack_delay,
                rx.buf,
                rx.output_block_ms,
            );
            true
        }
        // RTCP filtré / échec SRTP (déjà journalisé) : on continue de lire.
        Ok(_) => true,
        // Socket vide : jusqu'au prochain signal du système.
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
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
            if wait.is_zero() {
                true
            } else {
                st.retry_at = Some(Instant::now() + wait);
                false
            }
        }
    }
}

/// Journal des silences d'instrument (une fois par silence).
fn log_silences(streams: &mut HashMap<Token, Stream>, now: Instant) {
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

/// Désinscrit un flux, puis retire son état, son stream du mélangeur et ses
/// statistiques : sa socket n'étant plus lue, rien ne peut suivre.
fn remove(poll: &mut Poll, streams: &mut HashMap<Token, Stream>, token: Token, core: &mut RxCore) {
    let Some(mut st) = streams.remove(&token) else { return };
    if let Err(e) = poll.registry().deregister(st.receiver.source()) {
        tracing::warn!(target: "jamodio::recv", producer = st.short(), error = %e, "désinscription de la socket");
    }
    core.remove(&st.producer_id, st.epoch);
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::pipeline::ProducerNetStats;
    use jamodio_audio_core::codec::encoder::MusicEncoder;
    use jamodio_audio_core::mixer::mixer::AudioMixer;
    use jamodio_audio_core::net::rtp::{self, RtpHeader};
    use jamodio_audio_core::net::srtp::{SrtpContext, SrtpParameters};
    use std::net::UdpSocket;

    /// Un faux serveur : sa socket et son contexte de chiffrement vers l'agent.
    pub(in crate::pipeline) struct Serveur {
        socket: UdpSocket,
        ctx: SrtpContext,
    }

    impl Serveur {
        /// Envoie le paquet Opus `seq` (2,5 ms de silence) à `to`, chiffré.
        pub fn envoyer(&self, seq: u16, to: SocketAddr) {
            let mut p = paquet(seq);
            self.ctx.protect(&mut p).unwrap();
            self.socket.send_to(&p, to).unwrap();
        }
    }

    /// Un vrai paquet Opus (2,5 ms de silence) numéroté `seq`, en clair.
    pub(in crate::pipeline) fn paquet(seq: u16) -> Vec<u8> {
        let enc = MusicEncoder::new().expect("encodeur Opus");
        let pcm = vec![0.0f32; enc.frame_size() * 2];
        let mut out = vec![0u8; 1500];
        let n = enc.encode(&pcm, &mut out).expect("encodage");
        let h = RtpHeader { payload_type: 111, sequence: seq, timestamp: u32::from(seq) * 120, ssrc: 9, marker: false };
        rtp::build_packet(&h, &out[..n])
    }

    /// Un flux prêt à être reçu (`producer_id`, génération `epoch`) et le faux
    /// serveur qui lui parle, clés de chiffrement assorties.
    pub(in crate::pipeline) fn flux(producer_id: &str, epoch: u64) -> (NewStream, Serveur) {
        let server_keys = SrtpParameters::generate_aead_aes_256_gcm();
        let agent_keys = SrtpParameters::generate_aead_aes_256_gcm();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let receiver = RtpReceiver::new(Arc::new(SrtpContext::new(&agent_keys, &server_keys).unwrap())).unwrap();
        let ns = NewStream {
            producer_id: Arc::from(producer_id),
            epoch,
            kind: StreamKind::Instrument,
            receiver,
            sfu_addr: socket.local_addr().unwrap(),
            activity: Arc::new(RecvActivity::new(Instant::now())),
        };
        (ns, Serveur { socket, ctx: SrtpContext::new(&server_keys, &agent_keys).unwrap() })
    }

    fn hist() -> Arc<Mutex<Histogram>> {
        Arc::new(Mutex::new(Histogram::new(64)))
    }

    struct Banc {
        rt: RecvThread,
        mixer: Arc<AudioMixer>,
        stats: Arc<Mutex<HashMap<String, ProducerNetStats>>>,
        stack: Arc<Mutex<Histogram>>,
        work: Arc<Mutex<Histogram>>,
    }

    fn banc() -> Banc {
        let mixer = Arc::new(AudioMixer::new());
        let stats = Arc::new(Mutex::new(HashMap::new()));
        let core = RxCore::new(mixer.clone(), stats.clone(), hist(), Arc::new(std::sync::atomic::AtomicU32::new(64)));
        let (stack, work) = (hist(), hist());
        let rt = RecvThread::spawn(
            core,
            RxMeasures {
                stack_delay: stack.clone(),
                wake_late: hist(),
                wake_work: work.clone(),
                wake_over_budget: Arc::new(AtomicU64::new(0)),
            },
        )
        .unwrap();
        Banc { rt, mixer, stats, stack, work }
    }

    /// Le serveur attend le perçage (comedia) et rend l'adresse de l'agent.
    fn perce(s: &Serveur) -> SocketAddr {
        let mut buf = [0u8; 2048];
        s.socket.recv_from(&mut buf).expect("le fil perce vers le serveur").1
    }

    /// Attend (2 s au plus) que `cond` soit vraie — sans jamais supposer de
    /// durée : on regarde, on ne dort pas « assez longtemps ».
    fn attendre(cond: impl Fn() -> bool) -> bool {
        let fin = Instant::now() + Duration::from_secs(2);
        while Instant::now() < fin {
            if cond() {
                return true;
            }
            std::thread::yield_now();
        }
        cond()
    }

    /// Le fil perce, lit, déchiffre, décode et pousse au mélangeur, dans
    /// l'ordre : 40 paquets attendus, 40 comptés, aucun perdu ni en retard.
    #[test]
    fn le_fil_recoit_decode_et_pousse_dans_l_ordre() {
        let b = banc();
        let (ns, serveur) = flux("peer-a", 7);
        let activity = ns.activity.clone();
        b.rt.send(RecvCmd::Add(ns));
        let agent = perce(&serveur);
        for seq in 0..40 {
            serveur.envoyer(seq, agent);
        }
        // Les compteurs du flux sont publiés au 40e paquet.
        assert!(attendre(|| b.stats.lock().contains_key("peer-a")), "40 paquets décodés");
        let s = b.stats.lock().get("peer-a").copied().unwrap();
        assert_eq!((s.packets_expected, s.packets_lost, s.packets_late), (40, 0, 0));
        assert!(b.mixer.playout("peer-a").is_some_and(|p| p.buffered_ms > 0.0), "le son est dans le tampon");
        if cfg!(target_os = "macos") {
            assert!(!b.stack.lock().is_empty(), "attente système → lecture mesurée");
        }
        assert!(!b.work.lock().is_empty(), "travail par réveil mesuré");
        assert!(activity.silent_ms(Instant::now()) < 1_000, "activité marquée");
        b.rt.shutdown();
    }

    /// Le retrait enlève le flux du mélangeur et des statistiques.
    #[test]
    fn le_retrait_retire_le_flux_du_melangeur() {
        let b = banc();
        let (ns, serveur) = flux("peer-a", 7);
        b.rt.send(RecvCmd::Add(ns));
        let agent = perce(&serveur);
        for seq in 0..40 {
            serveur.envoyer(seq, agent);
        }
        assert!(attendre(|| b.stats.lock().contains_key("peer-a")));
        b.rt.send(RecvCmd::Remove { producer_id: Arc::from("peer-a"), epoch: 7 });
        assert!(attendre(|| b.mixer.playout("peer-a").is_none()), "stream retiré du mélangeur");
        assert!(!b.stats.lock().contains_key("peer-a"), "statistiques retirées");
        b.rt.shutdown();
    }

    /// Un retrait d'une AUTRE génération ne touche pas le flux en cours.
    #[test]
    fn un_retrait_d_une_autre_generation_est_ignore() {
        let b = banc();
        let (ns, serveur) = flux("peer-a", 7);
        b.rt.send(RecvCmd::Add(ns));
        b.rt.send(RecvCmd::Remove { producer_id: Arc::from("peer-a"), epoch: 6 });
        let agent = perce(&serveur);
        for seq in 0..3 {
            serveur.envoyer(seq, agent);
        }
        assert!(attendre(|| b.mixer.playout("peer-a").is_some()), "le flux reçoit toujours");
        b.rt.shutdown();
    }

    /// L'arrêt retire les flux restants du mélangeur, puis le fil finit.
    #[test]
    fn l_arret_retire_les_flux_restants() {
        let b = banc();
        let (ns, serveur) = flux("peer-a", 7);
        b.rt.send(RecvCmd::Add(ns));
        let agent = perce(&serveur);
        serveur.envoyer(0, agent);
        assert!(attendre(|| b.mixer.playout("peer-a").is_some()));
        b.rt.shutdown();
        assert!(b.mixer.playout("peer-a").is_none(), "le mélangeur ne garde aucun pair parti");
    }

    /// À tour de rôle : la source en rafale ne passe pas devant les autres.
    #[test]
    fn les_sources_pretes_sont_servies_a_tour_de_role() {
        let mut restant = HashMap::from([('a', 3u32), ('b', 1), ('c', 2)]);
        let mut ordre = Vec::new();
        let mut ready = vec!['a', 'b', 'c'];
        round_robin(&mut ready, |s| {
            let n = restant.get_mut(&s).unwrap();
            if *n == 0 {
                return false;
            }
            *n -= 1;
            ordre.push(s);
            true
        });
        assert_eq!(ordre, vec!['a', 'b', 'c', 'a', 'c', 'a']);
        assert!(ready.is_empty(), "chaque source quitte la liste une fois vide");
    }

    // ─── M0 (21/09/2026), renforcé au Lot 1-D4 : ce qui est arrivé passe avant
    //     la décision — y compris un paquet encore dans la socket. ─────────────

    struct BancM0 {
        core: RxCore,
        streams: HashMap<Token, Stream>,
        serveur: Serveur,
        agent: SocketAddr,
        /// Sert seulement à savoir, sans supposer de durée, que le paquet du
        /// serveur est arrivé dans la socket.
        poll: Poll,
    }

    /// Un flux connu, sortie en lecture, tampon VIDE, échéance dépassée bien
    /// au-delà de la grâce : sans rien d'autre, on masquerait.
    fn banc_m0(now: Instant) -> BancM0 {
        use crate::pipeline::conceal_loop_tests::{flux_en_lecture, state};
        let mixer = Arc::new(AudioMixer::new());
        flux_en_lecture(&mixer);
        let mut core =
            RxCore::new(mixer, Arc::new(Mutex::new(HashMap::new())), hist(), Arc::new(std::sync::atomic::AtomicU32::new(64)));
        let mut st = state(now);
        st.next_deadline = Some(now - Duration::from_millis(10));
        core.states.insert(Arc::from("peer-test"), st);
        let (ns, serveur) = flux("peer-test", 1);
        let agent = SocketAddr::from(([127, 0, 0, 1], ns.receiver.local_addr().unwrap().port()));
        let mut stream = Stream::new(ns);
        let poll = Poll::new().unwrap();
        poll.registry().register(stream.receiver.source(), Token(1), Interest::READABLE).unwrap();
        BancM0 { core, streams: HashMap::from([(Token(1), stream)]), serveur, agent, poll }
    }

    fn attendre_le_paquet(b: &mut BancM0) {
        let mut ev = Events::with_capacity(4);
        let fin = Instant::now() + Duration::from_secs(2);
        while ev.is_empty() && Instant::now() < fin {
            b.poll.poll(&mut ev, Some(fin.saturating_duration_since(Instant::now()))).unwrap();
        }
        assert!(!ev.is_empty(), "le paquet du serveur est dans la socket");
    }

    fn passe(b: &mut BancM0, now: Instant) {
        let mut buf = Vec::with_capacity(2048);
        let stack = Mutex::new(Histogram::new(16));
        let mut rx = Rx { core: &mut b.core, buf: &mut buf, stack_delay: &stack, output_block_ms: 64.0 * 1000.0 / 48_000.0, packets: 0 };
        conceal_pass(&mut Vec::new(), &mut b.streams, &mut rx, || now);
    }

    /// Le paquet attendu est déjà dans la socket quand le fil se réveille à
    /// l'échéance : il est joué, rien n'est inventé à sa place. (Avec deux
    /// fils, un paquet lu mais pas encore transmis pouvait être remplacé.)
    #[test]
    fn un_paquet_deja_dans_la_socket_a_l_echeance_est_joue_pas_remplace() {
        let now = Instant::now();
        let mut b = banc_m0(now);
        b.serveur.envoyer(1001, b.agent);
        attendre_le_paquet(&mut b);
        passe(&mut b, now);
        let s = b.core.states.values().next().unwrap();
        assert_eq!(s.concealed_underrun_frames, 0, "rien d'inventé : le vrai paquet était là");
        assert_eq!(s.seq.counters().late, 0, "et il n'a pas été écarté");
        assert!(b.core.mixer.playout("peer-test").unwrap().buffered_ms > 0.0, "il est dans le tampon");
    }

    /// Le contrôle : relire la socket ne doit pas désarmer le masquage d'un
    /// vrai retard.
    #[test]
    fn sans_rien_dans_la_socket_le_masquage_part_toujours() {
        let now = Instant::now();
        let mut b = banc_m0(now);
        passe(&mut b, now);
        assert_eq!(b.core.states.values().next().unwrap().concealed_underrun_frames, 1);
    }
}

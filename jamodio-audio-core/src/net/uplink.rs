//! Rapports RTCP du flux d'envoi instrument : le flux montant vu par le SFU.
//!
//! Une tâche tokio par transport d'envoi instrument, HORS du thread audio :
//!   - toutes les 5 s, un Sender Report (horodatages et totaux lus dans
//!     `RtpSender::activity`, écrits par le thread d'encodage) ;
//!   - à chaque Receiver Report du SFU : pertes et gigue du flux montant, et temps
//!     d'aller-retour UDP agent ↔ SFU quand le rapport cite l'un de nos SR.
//!
//! Chiffrement par un `SrtcpContext` dédié : jamais le verrou SRTP du son.
//! Vérifié contre le worker mediasoup réel : exemple `rtcp_interop`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::rtcp::{self, NtpTime, SenderReport, SenderReportHistory};
use super::srtp::SrtcpContext;
use super::udp::RtpSender;

/// Cadence des Sender Reports : celle des Receiver Reports audio du SFU (5 s).
const REPORT_INTERVAL: Duration = Duration::from_secs(5);
/// Premier SR peu après le départ du son, pour que le premier RR le cite déjà.
const FIRST_REPORT_DELAY: Duration = Duration::from_millis(500);
/// Aucun SR si rien n'est parti depuis ce délai : on ne décrit pas un flux arrêté.
const SENDING_MAX_IDLE: Duration = Duration::from_secs(1);
/// Index SRTCP (4 octets) + tag d'authentification AES-GCM (16 octets).
const SRTCP_OVERHEAD: usize = 20;
/// Les rapports du SFU (RR + XR) tiennent largement dans un MTU.
const RECV_BUFFER_LEN: usize = 1500;
/// Pause après une erreur de réception (Windows remonte un ICMP « port
/// injoignable » comme une erreur de `recv_from`) : jamais de boucle active.
const RECV_ERROR_PAUSE: Duration = Duration::from_millis(100);

/// Dernier état du flux montant d'après le SFU.
#[derive(Debug, Clone, Copy)]
pub struct UplinkStats {
    /// Temps d'aller-retour agent ↔ SFU (ms) sur le chemin UDP du son. `None` si
    /// le rapport ne cite aucun de nos SR (le premier rapport peut précéder le SR).
    pub rtt_ms: Option<f32>,
    /// Pertes depuis le rapport précédent (%).
    pub fraction_lost_pct: f32,
    /// Pertes cumulées depuis le début du flux.
    pub packets_lost: i32,
    /// Gigue d'arrivée au SFU (ms).
    pub jitter_ms: f32,
    /// Arrivée du rapport.
    pub received_at: Instant,
}

/// Tâche en cours pour un transport d'envoi. La lâcher arrête la tâche.
pub struct UplinkHandle {
    stats: Arc<Mutex<Option<UplinkStats>>>,
    _stop: tokio::sync::oneshot::Sender<()>,
}

impl UplinkHandle {
    /// Dernier rapport reçu, `None` avant le premier (~5 s après le départ).
    pub fn latest(&self) -> Option<UplinkStats> {
        *self.stats.lock()
    }
}

/// Lance la tâche du flux `ssrc` émis par `sender` (dans le runtime tokio courant).
/// `srtcp` : contexte créé avec les mêmes paramètres que le `SrtpContext` du transport.
pub fn spawn(sender: Arc<RtpSender>, srtcp: SrtcpContext, ssrc: u32) -> UplinkHandle {
    let stats = Arc::new(Mutex::new(None));
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(run(sender, srtcp, ssrc, stats.clone(), stop_rx));
    UplinkHandle {
        stats,
        _stop: stop_tx,
    }
}

async fn run(
    sender: Arc<RtpSender>,
    mut srtcp: SrtcpContext,
    ssrc: u32,
    stats: Arc<Mutex<Option<UplinkStats>>>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    let mut report_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + FIRST_REPORT_DELAY,
        REPORT_INTERVAL,
    );
    report_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut history = SenderReportHistory::default();
    let mut buf = Vec::with_capacity(RECV_BUFFER_LEN);
    let mut errors: u64 = 0;
    let mut first_report_logged = false;

    loop {
        tokio::select! {
            // Envoyé ou lâché (`UplinkHandle` détruit) : dans les deux cas, fin.
            _ = &mut stop => break,
            _ = report_tick.tick() => {
                if let Err(error) = send_report(&sender, &mut srtcp, ssrc, &mut history).await {
                    log_error(&mut errors, "sender report", &error);
                }
            }
            received = sender.recv_from_sfu(&mut buf) => {
                // Horodatage d'arrivée AVANT le déchiffrement (il entre dans le RTT).
                let arrived_at = Instant::now();
                match received {
                    Ok(true) => match read_report(&mut srtcp, ssrc, &history, &mut buf, arrived_at) {
                        Ok(Some(report)) => {
                            if !first_report_logged {
                                tracing::info!(
                                    target: "jamodio::uplink",
                                    rtt_ms = ?report.rtt_ms,
                                    fraction_lost_pct = report.fraction_lost_pct,
                                    jitter_ms = report.jitter_ms,
                                    "premier rapport RTCP du SFU"
                                );
                                first_report_logged = true;
                            }
                            *stats.lock() = Some(report);
                        }
                        Ok(None) => {}
                        Err(error) => log_error(&mut errors, "receiver report", &error),
                    },
                    Ok(false) => {}
                    Err(error) => {
                        log_error(&mut errors, "recv", &error.to_string());
                        tokio::time::sleep(RECV_ERROR_PAUSE).await;
                    }
                }
            }
        }
    }
}

async fn send_report(
    sender: &RtpSender,
    srtcp: &mut SrtcpContext,
    ssrc: u32,
    history: &mut SenderReportHistory,
) -> Result<(), String> {
    let Some(sent) = sender.activity().snapshot(Instant::now()) else {
        return Ok(());
    };
    if sent.age > SENDING_MAX_IDLE {
        return Ok(());
    }
    let ntp = NtpTime::now();
    let report = SenderReport {
        ssrc,
        ntp,
        rtp_ts: sent.rtp_ts_now(),
        packets: sent.packets,
        octets: sent.octets,
    };
    let mut packet = Vec::with_capacity(SenderReport::LEN + SRTCP_OVERHEAD);
    packet.extend_from_slice(&report.to_bytes());
    srtcp.protect_rtcp(&mut packet)?;
    let sent_at = Instant::now();
    sender.send_rtcp(&packet).await.map_err(|e| e.to_string())?;
    history.push(ntp, sent_at);
    Ok(())
}

/// Rapport du SFU sur notre flux, `None` si le datagramme n'en contient pas.
fn read_report(
    srtcp: &mut SrtcpContext,
    ssrc: u32,
    history: &SenderReportHistory,
    buf: &mut Vec<u8>,
    arrived_at: Instant,
) -> Result<Option<UplinkStats>, String> {
    if !rtcp::is_rtcp(buf) {
        return Ok(None);
    }
    srtcp.unprotect_rtcp(buf)?;
    let Some(block) = rtcp::report_blocks(buf)
        .into_iter()
        .find(|b| b.ssrc == ssrc)
    else {
        return Ok(None);
    };
    let rtt_ms = history
        .sent_at(block.lsr)
        .and_then(|sent_at| rtcp::round_trip(sent_at, arrived_at, block.dlsr))
        .map(|rtt| rtt.as_secs_f32() * 1000.0);
    Ok(Some(UplinkStats {
        rtt_ms,
        fraction_lost_pct: block.fraction_lost_pct(),
        packets_lost: block.cumulative_lost,
        jitter_ms: block.jitter_ms(),
        received_at: arrived_at,
    }))
}

/// Journalise la 1re erreur puis aux puissances de 2 (jamais une ligne par rapport).
fn log_error(count: &mut u64, stage: &str, error: &str) {
    *count += 1;
    if count.is_power_of_two() {
        tracing::warn!(target: "jamodio::uplink", stage, error, occurrences = *count, "RTCP du flux montant");
    }
}

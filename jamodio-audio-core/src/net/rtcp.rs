//! RTCP minimal du flux d'envoi agent → SFU (RFC 3550 §6.4).
//!
//! L'agent émet un **Sender Report** (SR) toutes les quelques secondes et lit les
//! **Receiver Reports** (RR) que le SFU renvoie sur le même socket. Un RR donne les
//! pertes et la gigue du flux montant tels que le SFU les constate ; s'il cite notre
//! dernier SR (champs LSR / DLSR), il donne aussi le vrai temps d'aller-retour UDP
//! agent ↔ SFU, sur le chemin du son.
//!
//! Le thread d'encodage RT n'appelle que [`SendActivity::record`] (deux écritures
//! atomiques). Tout le reste (construction, chiffrement, envoi, lecture) tourne dans
//! une tâche hors du thread audio.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Type de paquet RTCP : Sender Report.
pub const PT_SENDER_REPORT: u8 = 200;
/// Type de paquet RTCP : Receiver Report.
pub const PT_RECEIVER_REPORT: u8 = 201;

/// Horloge RTP du flux (Opus, 48 kHz).
const RTP_CLOCK_RATE: u64 = 48_000;
/// Taille d'un bloc de réception (RFC 3550 §6.4.1).
const REPORT_BLOCK_LEN: usize = 24;
/// Secondes entre l'époque NTP (1900) et l'époque Unix (1970).
const NTP_UNIX_OFFSET_SECS: u64 = 2_208_988_800;

/// Un octet 2 d'en-tête dans cette plage désigne du RTCP et non du RTP (RFC 5761 §4).
pub fn is_rtcp(packet: &[u8]) -> bool {
    packet.len() >= 8 && (192..=223).contains(&packet[1])
}

// ─── Activité d'envoi (écrite par le thread RT) ─────────────────────────────

/// Dernier paquet RTP envoyé et totaux du flux, pour écrire le Sender Report.
///
/// Écrit par UN seul thread (l'encodage RT), lu par la tâche RTCP. Deux atomiques
/// indépendants : un lecteur peut voir l'horodatage d'un paquet et les totaux du
/// précédent, écart sans effet (un paquet sur des milliers).
pub struct SendActivity {
    epoch: Instant,
    /// `(horodatage RTP << 32) | microsecondes depuis epoch (modulo 2^32)`.
    last_packet: AtomicU64,
    /// `(paquets << 32) | octets de charge utile`, chacun modulo 2^32 (RFC 3550).
    totals: AtomicU64,
}

impl Default for SendActivity {
    fn default() -> Self {
        Self::new()
    }
}

impl SendActivity {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            last_packet: AtomicU64::new(0),
            totals: AtomicU64::new(0),
        }
    }

    /// Thread RT, après un envoi réussi : deux écritures atomiques, rien d'autre
    /// (ni verrou, ni allocation, ni appel système).
    #[inline]
    pub fn record(&self, rtp_ts: u32, sent_at: Instant, packets: u32, octets: u32) {
        let micros = sent_at.duration_since(self.epoch).as_micros() as u32;
        self.last_packet.store(
            (u64::from(rtp_ts) << 32) | u64::from(micros),
            Ordering::Relaxed,
        );
        self.totals.store(
            (u64::from(packets) << 32) | u64::from(octets),
            Ordering::Relaxed,
        );
    }

    /// État à l'instant `now`, `None` tant qu'aucun paquet n'est parti.
    /// L'âge est exact tant que le dernier envoi date de moins de 71 minutes.
    pub fn snapshot(&self, now: Instant) -> Option<SendSnapshot> {
        let totals = self.totals.load(Ordering::Relaxed);
        let packets = (totals >> 32) as u32;
        if packets == 0 {
            return None;
        }
        let last = self.last_packet.load(Ordering::Relaxed);
        let sent_micros = last as u32;
        let now_micros = now.saturating_duration_since(self.epoch).as_micros() as u32;
        Some(SendSnapshot {
            rtp_ts: (last >> 32) as u32,
            age: Duration::from_micros(u64::from(now_micros.wrapping_sub(sent_micros))),
            packets,
            octets: totals as u32,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendSnapshot {
    /// Horodatage RTP du dernier paquet envoyé.
    pub rtp_ts: u32,
    /// Temps écoulé depuis son envoi.
    pub age: Duration,
    pub packets: u32,
    pub octets: u32,
}

impl SendSnapshot {
    /// Horodatage RTP correspondant à l'instant du snapshot (dernier paquet + âge).
    pub fn rtp_ts_now(&self) -> u32 {
        let elapsed = self.age.as_micros() as u64 * RTP_CLOCK_RATE / 1_000_000;
        self.rtp_ts.wrapping_add(elapsed as u32)
    }
}

// ─── Temps NTP ───────────────────────────────────────────────────────────────

/// Horodatage NTP 64 bits (32 bits de secondes, 32 bits de fraction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NtpTime(pub u64);

impl NtpTime {
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    pub fn from_system_time(time: SystemTime) -> Self {
        let since_unix = time.duration_since(UNIX_EPOCH).unwrap_or_default();
        let secs = since_unix.as_secs() + NTP_UNIX_OFFSET_SECS;
        let frac = (u64::from(since_unix.subsec_nanos()) << 32) / 1_000_000_000;
        Self((secs << 32) | frac)
    }

    /// Les 32 bits du milieu : ce que le récepteur renvoie dans le champ LSR.
    pub fn middle32(self) -> u32 {
        (self.0 >> 16) as u32
    }
}

// ─── Sender Report ───────────────────────────────────────────────────────────

/// Sender Report sans bloc de réception (l'agent ne reçoit rien sur ce transport).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderReport {
    pub ssrc: u32,
    pub ntp: NtpTime,
    pub rtp_ts: u32,
    pub packets: u32,
    pub octets: u32,
}

impl SenderReport {
    pub const LEN: usize = 28;

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0] = 0x80; // V=2, P=0, RC=0
        b[1] = PT_SENDER_REPORT;
        b[2..4].copy_from_slice(&((Self::LEN / 4 - 1) as u16).to_be_bytes());
        b[4..8].copy_from_slice(&self.ssrc.to_be_bytes());
        b[8..16].copy_from_slice(&self.ntp.0.to_be_bytes());
        b[16..20].copy_from_slice(&self.rtp_ts.to_be_bytes());
        b[20..24].copy_from_slice(&self.packets.to_be_bytes());
        b[24..28].copy_from_slice(&self.octets.to_be_bytes());
        b
    }
}

// ─── Blocs de réception ──────────────────────────────────────────────────────

/// Bloc de réception d'un SR ou d'un RR (RFC 3550 §6.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportBlock {
    /// Flux décrit (le nôtre : SSRC de notre producteur).
    pub ssrc: u32,
    /// Pertes depuis le rapport précédent, en 256e.
    pub fraction_lost: u8,
    /// Pertes cumulées (négatives si des doubles ont été reçus).
    pub cumulative_lost: i32,
    pub highest_seq: u32,
    /// Gigue d'arrivée, en unités d'horloge RTP.
    pub jitter: u32,
    /// 32 bits du milieu du NTP de notre dernier SR reçu (0 = aucun).
    pub lsr: u32,
    /// Délai entre la réception de ce SR et l'envoi du rapport, en 1/65536 s.
    pub dlsr: u32,
}

impl ReportBlock {
    fn parse(b: &[u8]) -> Self {
        let be32 = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        // Pertes cumulées : entier signé sur 24 bits.
        let lost24 = (i32::from(b[5]) << 16) | (i32::from(b[6]) << 8) | i32::from(b[7]);
        Self {
            ssrc: be32(0),
            fraction_lost: b[4],
            cumulative_lost: (lost24 << 8) >> 8,
            highest_seq: be32(8),
            jitter: be32(12),
            lsr: be32(16),
            dlsr: be32(20),
        }
    }

    pub fn fraction_lost_pct(&self) -> f32 {
        f32::from(self.fraction_lost) * 100.0 / 256.0
    }

    pub fn jitter_ms(&self) -> f32 {
        (f64::from(self.jitter) * 1000.0 / RTP_CLOCK_RATE as f64) as f32
    }
}

/// Blocs de réception de tous les SR et RR d'un paquet RTCP composé (déchiffré).
/// Les autres types de paquets sont ignorés ; un paquet tronqué arrête la lecture.
pub fn report_blocks(compound: &[u8]) -> Vec<ReportBlock> {
    let mut blocks = Vec::new();
    let mut offset = 0;
    while compound.len() - offset >= 4 {
        let header = &compound[offset..];
        if header[0] >> 6 != 2 {
            break;
        }
        let count = usize::from(header[0] & 0x1F);
        let len = (usize::from(u16::from_be_bytes([header[2], header[3]])) + 1) * 4;
        if len > header.len() {
            break;
        }
        let first_block = match header[1] {
            PT_SENDER_REPORT => 28,
            PT_RECEIVER_REPORT => 8,
            _ => {
                offset += len;
                continue;
            }
        };
        for i in 0..count {
            let start = first_block + i * REPORT_BLOCK_LEN;
            let end = start + REPORT_BLOCK_LEN;
            if end > len {
                break;
            }
            blocks.push(ReportBlock::parse(&header[start..end]));
        }
        offset += len;
    }
    blocks
}

// ─── Temps d'aller-retour ────────────────────────────────────────────────────

/// Temps d'aller-retour (RFC 3550 §6.4.1) : arrivée du rapport − envoi du SR qu'il
/// cite − temps passé chez le récepteur. Mesuré sur l'horloge monotone de l'agent
/// (insensible à un changement d'heure).
///
/// Résolution : 1 ms. Le SFU compte DLSR sur une horloge à la milliseconde (mediasoup
/// `DepLibUV::GetTimeMs`, arrondie à l'inférieur aux deux bouts), donc DLSR peut
/// dépasser le vrai temps de garde de moins d'1 ms : un résultat négatif dans cette
/// marge vaut 0 (boucle locale). Au-delà, `None` : rapport incohérent.
pub fn round_trip(sr_sent_at: Instant, report_arrived_at: Instant, dlsr: u32) -> Option<Duration> {
    const SFU_CLOCK_RESOLUTION: Duration = Duration::from_millis(1);
    let elapsed = report_arrived_at.checked_duration_since(sr_sent_at)?;
    let held = Duration::from_micros(u64::from(dlsr) * 1_000_000 / 65_536);
    if elapsed + SFU_CLOCK_RESOLUTION < held {
        return None;
    }
    Some(elapsed.saturating_sub(held))
}

/// Derniers SR envoyés, pour retrouver celui qu'un rapport cite dans LSR.
#[derive(Debug, Default)]
pub struct SenderReportHistory {
    entries: [Option<(u32, Instant)>; 8],
    next: usize,
}

impl SenderReportHistory {
    pub fn push(&mut self, ntp: NtpTime, sent_at: Instant) {
        self.entries[self.next] = Some((ntp.middle32(), sent_at));
        self.next = (self.next + 1) % self.entries.len();
    }

    /// Instant d'envoi du SR cité par `lsr`, s'il est encore connu.
    pub fn sent_at(&self, lsr: u32) -> Option<Instant> {
        if lsr == 0 {
            return None;
        }
        self.entries
            .iter()
            .flatten()
            .find(|(middle, _)| *middle == lsr)
            .map(|&(_, at)| at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_report_au_format_rfc3550() {
        let sr = SenderReport {
            ssrc: 0x1122_3344,
            ntp: NtpTime(0xAABB_CCDD_EEFF_0011),
            rtp_ts: 0x0102_0304,
            packets: 7,
            octets: 900,
        };
        let b = sr.to_bytes();
        assert_eq!(b[0], 0x80);
        assert_eq!(b[1], 200);
        assert_eq!(u16::from_be_bytes([b[2], b[3]]), 6);
        assert_eq!(&b[4..8], &[0x11, 0x22, 0x33, 0x44]);
        assert_eq!(&b[8..16], &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11]);
        assert_eq!(&b[16..20], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(u32::from_be_bytes([b[20], b[21], b[22], b[23]]), 7);
        assert_eq!(u32::from_be_bytes([b[24], b[25], b[26], b[27]]), 900);
        assert!(is_rtcp(&b));
    }

    #[test]
    fn ntp_depuis_l_heure_systeme() {
        let t = UNIX_EPOCH + Duration::new(1_000, 500_000_000);
        let ntp = NtpTime::from_system_time(t);
        assert_eq!(ntp.0 >> 32, 1_000 + NTP_UNIX_OFFSET_SECS);
        assert_eq!(ntp.0 as u32, 1 << 31); // une demi-seconde
        assert_eq!(ntp.middle32(), (ntp.0 >> 16) as u32);
    }

    fn block_bytes(
        ssrc: u32,
        fraction: u8,
        lost: i32,
        jitter: u32,
        lsr: u32,
        dlsr: u32,
    ) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&ssrc.to_be_bytes());
        b.push(fraction);
        b.extend_from_slice(&lost.to_be_bytes()[1..]);
        b.extend_from_slice(&1234u32.to_be_bytes());
        b.extend_from_slice(&jitter.to_be_bytes());
        b.extend_from_slice(&lsr.to_be_bytes());
        b.extend_from_slice(&dlsr.to_be_bytes());
        b
    }

    fn receiver_report(blocks: &[Vec<u8>]) -> Vec<u8> {
        let len_words = (8 + blocks.len() * 24) / 4 - 1;
        let mut p = vec![0x80 | blocks.len() as u8, PT_RECEIVER_REPORT];
        p.extend_from_slice(&(len_words as u16).to_be_bytes());
        p.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        for b in blocks {
            p.extend_from_slice(b);
        }
        p
    }

    #[test]
    fn lit_les_blocs_d_un_paquet_compose() {
        // RR (2 blocs) suivi d'un XR (type 207) ignoré, comme le composé du SFU.
        let mut compound = receiver_report(&[
            block_bytes(1, 64, 12, 96, 0xABCD_0001, 65_536),
            block_bytes(2, 0, -3, 0, 0, 0),
        ]);
        compound.extend_from_slice(&[0x80, 207, 0x00, 0x04]);
        compound.extend_from_slice(&[0u8; 16]);

        let blocks = report_blocks(&compound);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].ssrc, 1);
        assert_eq!(blocks[0].fraction_lost_pct(), 25.0);
        assert_eq!(blocks[0].cumulative_lost, 12);
        assert_eq!(blocks[0].highest_seq, 1234);
        assert_eq!(blocks[0].jitter_ms(), 2.0);
        assert_eq!(blocks[0].lsr, 0xABCD_0001);
        assert_eq!(blocks[0].dlsr, 65_536);
        assert_eq!(blocks[1].cumulative_lost, -3);
    }

    #[test]
    fn lit_les_blocs_d_un_sender_report() {
        let mut p = vec![0x81, PT_SENDER_REPORT, 0x00, 12];
        p.extend_from_slice(&[0u8; 24]); // SSRC + infos émetteur
        p.extend_from_slice(&block_bytes(9, 0, 0, 0, 0, 0));
        let blocks = report_blocks(&p);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].ssrc, 9);
    }

    #[test]
    fn paquet_tronque_ou_invalide() {
        let rr = receiver_report(&[block_bytes(1, 0, 0, 0, 0, 0)]);
        assert!(report_blocks(&rr[..rr.len() - 1]).is_empty());
        assert!(report_blocks(&[0x00, 201, 0, 1, 0, 0, 0, 0]).is_empty());
        assert!(report_blocks(&[]).is_empty());
        // Compte de blocs mensonger : on s'arrête à la longueur du paquet.
        let mut liar = rr.clone();
        liar[0] = 0x80 | 5;
        assert_eq!(report_blocks(&liar).len(), 1);
    }

    #[test]
    fn aller_retour_moins_le_temps_passe_au_sfu() {
        let sent = Instant::now();
        let arrived = sent + Duration::from_millis(1_030);
        // Le SFU a gardé le rapport 1 s : l'aller-retour vaut 30 ms.
        assert_eq!(
            round_trip(sent, arrived, 65_536),
            Some(Duration::from_millis(30))
        );
        // Boucle locale : DLSR (horloge ms du SFU) dépasse le temps écoulé de moins
        // d'1 ms → aller-retour nul, pas absent.
        assert_eq!(
            round_trip(sent, sent + Duration::from_micros(999_400), 65_536),
            Some(Duration::ZERO)
        );
        // Incohérent : le SFU dit avoir gardé nettement plus que le temps écoulé.
        assert_eq!(
            round_trip(sent, sent + Duration::from_millis(10), 65_536),
            None
        );
        assert_eq!(round_trip(arrived, sent, 0), None);
    }

    #[test]
    fn historique_des_sender_reports() {
        let mut h = SenderReportHistory::default();
        let t0 = Instant::now();
        for i in 0..10u64 {
            h.push(NtpTime((i + 1) << 16), t0 + Duration::from_secs(i));
        }
        // Les 8 derniers sont connus, les 2 premiers ont été remplacés.
        assert_eq!(h.sent_at(10), Some(t0 + Duration::from_secs(9)));
        assert_eq!(h.sent_at(3), Some(t0 + Duration::from_secs(2)));
        assert_eq!(h.sent_at(2), None);
        assert_eq!(h.sent_at(0), None);
    }

    #[test]
    fn activite_d_envoi() {
        let activity = SendActivity::new();
        let now = Instant::now();
        assert_eq!(activity.snapshot(now), None);

        let sent = now + Duration::from_millis(5);
        activity.record(u32::MAX - 10, sent, 3, 300);
        let later = sent + Duration::from_millis(20);
        let snap = activity.snapshot(later).expect("un paquet est parti");
        assert_eq!(snap.packets, 3);
        assert_eq!(snap.octets, 300);
        assert_eq!(snap.age, Duration::from_millis(20));
        // 20 ms à 48 kHz = 960 unités, avec passage par zéro.
        assert_eq!(snap.rtp_ts_now(), (u32::MAX - 10).wrapping_add(960));
    }
}

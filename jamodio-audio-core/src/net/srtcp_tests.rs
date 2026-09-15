//! Chiffrement des rapports RTCP, identique sur les deux backends SRTP.

use super::rtcp::{is_rtcp, NtpTime, SenderReport};
use super::rtp::{self, RtpHeader};
use super::srtp::{SrtcpContext, SrtpContext, SrtpParameters};

fn sender_report() -> [u8; SenderReport::LEN] {
    SenderReport {
        ssrc: 0x0102_0304,
        ntp: NtpTime(0x1111_2222_3333_4444),
        rtp_ts: 48_000,
        packets: 400,
        octets: 64_000,
    }
    .to_bytes()
}

#[test]
fn un_rapport_chiffre_se_dechiffre_avec_les_cles_du_pair() {
    let agent = SrtpParameters::generate_aead_aes_256_gcm();
    let sfu = SrtpParameters::generate_aead_aes_256_gcm();
    let mut agent_ctx = SrtcpContext::new(&agent, &sfu).expect("contexte agent");
    let mut sfu_ctx = SrtcpContext::new(&sfu, &agent).expect("contexte SFU");

    let clear = sender_report();
    let mut packet = clear.to_vec();
    agent_ctx.protect_rtcp(&mut packet).expect("chiffrement");
    assert!(packet.len() > clear.len(), "index SRTCP + tag ajoutés");
    assert_ne!(
        &packet[8..SenderReport::LEN],
        &clear[8..],
        "contenu chiffré"
    );
    assert!(
        is_rtcp(&packet),
        "l'en-tête reste lisible pour le démultiplexage"
    );

    sfu_ctx.unprotect_rtcp(&mut packet).expect("déchiffrement");
    assert_eq!(packet, clear);
}

#[test]
fn un_rapport_rejoue_est_refuse() {
    let agent = SrtpParameters::generate_aead_aes_256_gcm();
    let sfu = SrtpParameters::generate_aead_aes_256_gcm();
    let mut agent_ctx = SrtcpContext::new(&agent, &sfu).expect("contexte agent");
    let mut sfu_ctx = SrtcpContext::new(&sfu, &agent).expect("contexte SFU");

    let mut packet = sender_report().to_vec();
    agent_ctx.protect_rtcp(&mut packet).expect("chiffrement");
    let mut replay = packet.clone();
    sfu_ctx
        .unprotect_rtcp(&mut packet)
        .expect("premier passage");
    assert!(sfu_ctx.unprotect_rtcp(&mut replay).is_err());
}

#[test]
fn le_contexte_des_rapports_ne_touche_pas_au_chiffrement_du_son() {
    // Mêmes clés, deux contextes : le son (thread RT) et les rapports (tâche RTCP)
    // s'entrelacent sans que le pair cesse de déchiffrer le son.
    let agent = SrtpParameters::generate_aead_aes_256_gcm();
    let sfu = SrtpParameters::generate_aead_aes_256_gcm();
    let audio = SrtpContext::new(&agent, &sfu).expect("contexte son");
    let mut reports = SrtcpContext::new(&agent, &sfu).expect("contexte rapports");
    let peer_audio = SrtpContext::new(&sfu, &agent).expect("contexte son du pair");
    let mut peer_reports = SrtcpContext::new(&sfu, &agent).expect("contexte rapports du pair");

    for sequence in 0..5u16 {
        let header = RtpHeader {
            payload_type: 111,
            sequence,
            timestamp: u32::from(sequence) * 120,
            ssrc: 0x0102_0304,
            marker: false,
        };
        let clear = rtp::build_packet(&header, &[0xAB; 40]);
        let mut packet = clear.clone();
        audio.protect(&mut packet).expect("chiffrement du son");

        let mut report = sender_report().to_vec();
        reports
            .protect_rtcp(&mut report)
            .expect("chiffrement du rapport");

        peer_audio
            .unprotect(&mut packet)
            .expect("le son se déchiffre");
        assert_eq!(packet, clear);
        peer_reports
            .unprotect_rtcp(&mut report)
            .expect("le rapport se déchiffre");
    }
}

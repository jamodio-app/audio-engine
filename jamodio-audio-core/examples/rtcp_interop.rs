//! Interopérabilité RTCP de la voie B contre un SFU mediasoup RÉEL.
//!
//! Émet un flux RTP chiffré au rythme de l'agent (une trame toutes les 2,5 ms)
//! avec le code de production (`RtpSender`, `SendActivity::record`, tâche
//! `net::uplink`) et imprime chaque seconde, en JSON, ce que les Receiver Reports
//! du SFU disent du flux. Lancé par `server/tools/rtcp-interop.js` du dépôt web,
//! qui crée le PlainTransport avec la configuration de production.
//!
//! ```text
//! rtcp_interop <port_sfu> <clé_sfu_base64> <clé_agent_base64> <ssrc> <secondes> <perte_1_sur_n>
//! ```
//! `perte_1_sur_n` : 0 = aucune perte simulée, 20 = un paquet sur 20 jamais envoyé.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jamodio_audio_core::net::rtp::{self, RtpHeader};
use jamodio_audio_core::net::srtp::{SrtcpContext, SrtpContext, SrtpParameters, AEAD_AES_256_GCM};
use jamodio_audio_core::net::udp::RtpSender;
use jamodio_audio_core::net::uplink;

const FRAME: Duration = Duration::from_micros(2_500);
const SAMPLES_PER_FRAME: u32 = 120;

fn arg<T: std::str::FromStr>(args: &[String], i: usize, name: &str) -> T {
    args.get(i)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("argument {i} manquant ou invalide : {name}"))
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = arg(&args, 1, "port_sfu");
    let sfu = SrtpParameters {
        crypto_suite: AEAD_AES_256_GCM.to_string(),
        key_base64: arg(&args, 2, "clé_sfu_base64"),
    };
    let agent = SrtpParameters {
        crypto_suite: AEAD_AES_256_GCM.to_string(),
        key_base64: arg(&args, 3, "clé_agent_base64"),
    };
    let ssrc: u32 = arg(&args, 4, "ssrc");
    let seconds: u64 = arg(&args, 5, "secondes");
    let drop_one_in: u16 = arg(&args, 6, "perte_1_sur_n");

    let target: SocketAddr = ([127, 0, 0, 1], port).into();
    let srtp = Arc::new(SrtpContext::new(&agent, &sfu).expect("contexte SRTP"));
    let sender = Arc::new(RtpSender::new(target, srtp).await.expect("socket"));
    let srtcp = SrtcpContext::new(&agent, &sfu).expect("contexte SRTCP");
    let reports = uplink::spawn(sender.clone(), srtcp, ssrc);

    let deadline = Instant::now() + Duration::from_secs(seconds);

    // Émission sur un thread dédié, comme le thread d'encodage de l'agent.
    let rt_sender = sender.clone();
    let emitter = std::thread::spawn(move || {
        let payload = [0xF8u8, 0xFF, 0xFE]; // trame Opus quelconque : le SFU ne décode pas
        let (first_sequence, first_timestamp) = rtp::random_start();
        let (mut sequence, mut timestamp) = (first_sequence, first_timestamp);
        let (mut packets_sent, mut octets_sent) = (0u32, 0u32);
        let mut next = Instant::now();
        while Instant::now() < deadline {
            let skipped = drop_one_in > 0 && sequence % drop_one_in == drop_one_in - 1;
            if !skipped {
                let header = RtpHeader {
                    payload_type: 111,
                    sequence,
                    timestamp,
                    ssrc,
                    marker: sequence == first_sequence,
                };
                let packet = rtp::build_packet(&header, &payload);
                let produced_at = Instant::now();
                if let Ok(sent) = rt_sender.send_blocking(packet) {
                    if sent > 0 {
                        packets_sent = packets_sent.wrapping_add(1);
                        octets_sent = octets_sent.wrapping_add(payload.len() as u32);
                        rt_sender.activity().record(
                            timestamp,
                            produced_at,
                            packets_sent,
                            octets_sent,
                        );
                    }
                }
            }
            sequence = sequence.wrapping_add(1);
            timestamp = timestamp.wrapping_add(SAMPLES_PER_FRAME);
            next += FRAME;
            if let Some(wait) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
        packets_sent
    });

    let started = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    while Instant::now() < deadline {
        tick.tick().await;
        let line = match reports.latest() {
            Some(r) => serde_json::json!({
                "t": started.elapsed().as_secs(),
                "rttMs": r.rtt_ms,
                "fractionLostPct": r.fraction_lost_pct,
                "packetsLost": r.packets_lost,
                "jitterMs": r.jitter_ms,
                "reportAgeMs": r.received_at.elapsed().as_millis() as u64,
            }),
            None => serde_json::json!({ "t": started.elapsed().as_secs(), "report": null }),
        };
        println!("{line}");
    }
    let packets_sent = emitter.join().expect("thread d'émission");
    println!(
        "{}",
        serde_json::json!({ "done": true, "packetsSent": packets_sent })
    );
}

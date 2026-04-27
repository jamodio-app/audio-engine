//! SRTP (RFC 3711) — chiffrement + auth du flux RTP agent ↔ SFU.
//!
//! Mediasoup utilise libsrtp2 et expose les clés en wire format JSON :
//! `{ cryptoSuite, keyBase64 }` où `keyBase64` est base64(masterKey || masterSalt).
//! On supporte uniquement AEAD_AES_256_GCM (32 octets clé + 12 octets sel = 44).

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use srtp::{CryptoPolicy, Session, StreamPolicy};
use std::sync::Mutex;

pub const AEAD_AES_256_GCM: &str = "AEAD_AES_256_GCM";
const AEAD_AES_256_GCM_KEY_LEN: usize = 44; // 32 master key + 12 master salt

/// Clés SRTP au format wire mediasoup.
/// `key_base64` se décode en `master_key (32) || master_salt (12)` pour AEAD_AES_256_GCM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SrtpParameters {
    #[serde(rename = "cryptoSuite")]
    pub crypto_suite: String,
    #[serde(rename = "keyBase64")]
    pub key_base64: String,
}

impl SrtpParameters {
    /// Génère un nouveau matériel de clé AEAD_AES_256_GCM cryptographiquement sûr.
    pub fn generate_aead_aes_256_gcm() -> Self {
        let mut buf = [0u8; AEAD_AES_256_GCM_KEY_LEN];
        getrandom::getrandom(&mut buf).expect("getrandom failed");
        Self {
            crypto_suite: AEAD_AES_256_GCM.to_string(),
            key_base64: B64.encode(buf),
        }
    }

    fn decode(&self) -> Result<Vec<u8>, String> {
        if self.crypto_suite != AEAD_AES_256_GCM {
            return Err(format!("unsupported SRTP suite: {}", self.crypto_suite));
        }
        let bytes = B64
            .decode(&self.key_base64)
            .map_err(|e| format!("invalid base64: {e}"))?;
        if bytes.len() != AEAD_AES_256_GCM_KEY_LEN {
            return Err(format!(
                "expected {AEAD_AES_256_GCM_KEY_LEN} bytes, got {}",
                bytes.len()
            ));
        }
        Ok(bytes)
    }
}

/// Contexte SRTP bidirectionnel pour un PlainTransport unique.
/// `tx` chiffre nos paquets sortants avec notre clé locale.
/// `rx` déchiffre les paquets entrants avec la clé du SFU.
///
/// Le `Mutex` permet l'utilisation depuis un `Arc<RtpSender>` partagé entre
/// plusieurs tasks tokio (le verrou n'est jamais tenu à travers un `.await`).
pub struct SrtpContext {
    tx: Mutex<Session>,
    rx: Mutex<Session>,
}

impl SrtpContext {
    /// `local` : clés générées par nous, communiquées au SFU via connect-plain-transport.
    /// `remote` : clés du SFU, reçues via plain-transport-created / plain-consumer-created.
    pub fn new(local: &SrtpParameters, remote: &SrtpParameters) -> Result<Self, String> {
        let local_key = local.decode()?;
        let remote_key = remote.decode()?;
        let policy = CryptoPolicy::aes_gcm_256_16_auth();
        let tx = Session::with_outbound_template(StreamPolicy {
            key: &local_key,
            rtp: policy,
            rtcp: policy,
            ..Default::default()
        })
        .map_err(|e| format!("create outbound SRTP session: {e:?}"))?;
        let rx = Session::with_inbound_template(StreamPolicy {
            key: &remote_key,
            rtp: policy,
            rtcp: policy,
            ..Default::default()
        })
        .map_err(|e| format!("create inbound SRTP session: {e:?}"))?;
        Ok(Self {
            tx: Mutex::new(tx),
            rx: Mutex::new(rx),
        })
    }

    /// Chiffre un paquet RTP en place. Le buffer est étendu avec le tag d'auth (~16 octets) ;
    /// `Vec::with_capacity(MTU)` est recommandé côté caller pour éviter une réallocation.
    pub fn protect(&self, buf: &mut Vec<u8>) -> Result<(), String> {
        let mut tx = self.tx.lock().map_err(|_| "SRTP tx lock poisoned")?;
        tx.protect(buf).map_err(|e| format!("SRTP protect: {e:?}"))
    }

    /// Déchiffre un paquet SRTP en place. Tronque le tag d'auth en cas de succès.
    pub fn unprotect(&self, buf: &mut Vec<u8>) -> Result<(), String> {
        let mut rx = self.rx.lock().map_err(|_| "SRTP rx lock poisoned")?;
        rx.unprotect(buf)
            .map_err(|e| format!("SRTP unprotect: {e:?}"))
    }
}

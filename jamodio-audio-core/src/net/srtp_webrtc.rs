//! SRTP (RFC 3711) — wrapper Windows basé sur `webrtc-srtp` (pure Rust).
//!
//! API publique IDENTIQUE à `srtp_libsrtp.rs` (mac/linux). Le routing se fait
//! dans `net/mod.rs` via `#[cfg(windows)]` + `#[path]`. Le reste du code agent
//! (notamment `net/udp.rs`) ne voit aucune différence.
//!
//! Choix : webrtc-srtp est utilisé en prod par l'écosystème pion-rs depuis
//! 5+ ans, conforme RFC 3711+7714 → interop garantie avec mediasoup côté SFU
//! (mediasoup utilise libsrtp2 mais le wire format SRTP est standardisé).
//! Voir mémoire `srtp_strategy.md` pour le contexte de la décision.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use webrtc_srtp::context::Context;
use webrtc_srtp::option::{srtcp_replay_protection, srtp_replay_protection};
use webrtc_srtp::protection_profile::ProtectionProfile;

/// Taille de la fenêtre anti-replay (paquets). 128 = défaut libsrtp2 (backend
/// mac) → parité de sécurité entre les deux plateformes.
const SRTP_REPLAY_WINDOW: usize = 128;

pub const AEAD_AES_256_GCM: &str = "AEAD_AES_256_GCM";
const MASTER_KEY_LEN: usize = 32;
const MASTER_SALT_LEN: usize = 12;
const COMBINED_LEN: usize = MASTER_KEY_LEN + MASTER_SALT_LEN; // 44

/// Matériel de clé SRTP décodé : `(master_key 32, master_salt 12)`, chacun dans
/// un `Zeroizing` → effacé de la mémoire au drop.
type DecodedSrtpKey = (zeroize::Zeroizing<Vec<u8>>, zeroize::Zeroizing<Vec<u8>>);

/// Clés SRTP au format wire mediasoup (identique à libsrtp wrapper).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SrtpParameters {
    #[serde(rename = "cryptoSuite")]
    pub crypto_suite: String,
    #[serde(rename = "keyBase64")]
    pub key_base64: String,
}

impl SrtpParameters {
    pub fn generate_aead_aes_256_gcm() -> Self {
        let mut buf = [0u8; COMBINED_LEN];
        getrandom::getrandom(&mut buf).expect("getrandom failed");
        Self {
            crypto_suite: AEAD_AES_256_GCM.to_string(),
            key_base64: B64.encode(buf),
        }
    }

    /// Décode les 44 octets en (master_key 32, master_salt 12) — webrtc-srtp
    /// veut les deux séparés (libsrtp les concatène en interne).
    /// Clé + salt décodés dans des `Zeroizing` → effacés de la mémoire au drop
    /// (le `Context` a copié/dérivé le matériel en interne).
    fn decode(&self) -> Result<DecodedSrtpKey, String> {
        if self.crypto_suite != AEAD_AES_256_GCM {
            return Err(format!("unsupported SRTP suite: {}", self.crypto_suite));
        }
        let bytes = zeroize::Zeroizing::new(
            B64.decode(&self.key_base64)
                .map_err(|e| format!("invalid base64: {e}"))?,
        );
        if bytes.len() != COMBINED_LEN {
            return Err(format!(
                "expected {COMBINED_LEN} bytes, got {}",
                bytes.len()
            ));
        }
        let key = zeroize::Zeroizing::new(bytes[..MASTER_KEY_LEN].to_vec());
        let salt = zeroize::Zeroizing::new(bytes[MASTER_KEY_LEN..].to_vec());
        Ok((key, salt))
    }
}

/// Contexte SRTP bidirectionnel pour un PlainTransport unique.
///
/// webrtc-srtp impose un `Context` UNIDIRECTIONNEL (encrypt OU decrypt, pas
/// les deux) — on instancie donc deux Context séparés. Comportement
/// fonctionnellement identique au `Session` libsrtp côté wire.
pub struct SrtpContext {
    tx: Mutex<Context>,
    rx: Mutex<Context>,
}

impl SrtpContext {
    /// `local` : clés générées par nous, communiquées au SFU via connect-plain-transport.
    /// `remote` : clés du SFU, reçues via plain-transport-created / plain-consumer-created.
    pub fn new(local: &SrtpParameters, remote: &SrtpParameters) -> Result<Self, String> {
        let (local_key, local_salt) = local.decode()?;
        let (remote_key, remote_salt) = remote.decode()?;

        let tx = Context::new(
            &local_key[..],
            &local_salt[..],
            ProtectionProfile::AeadAes256Gcm,
            None,
            None,
        )
        .map_err(|e| format!("create outbound SRTP context: {e}"))?;

        // Anti-replay sur le contexte ENTRANT : sans ces options,
        // webrtc-srtp installe `srtp_no_replay_protection()` (cf. sources
        // 0.17.1) → un attaquant on-path pourrait rejouer des paquets SRTP
        // capturés. Le backend mac (libsrtp2) a sa fenêtre replay active par
        // défaut ; on aligne Windows dessus pour une sécurité identique.
        let rx = Context::new(
            &remote_key[..],
            &remote_salt[..],
            ProtectionProfile::AeadAes256Gcm,
            Some(srtp_replay_protection(SRTP_REPLAY_WINDOW)),
            Some(srtcp_replay_protection(SRTP_REPLAY_WINDOW)),
        )
        .map_err(|e| format!("create inbound SRTP context: {e}"))?;

        Ok(Self {
            tx: Mutex::new(tx),
            rx: Mutex::new(rx),
        })
    }

    /// Chiffre un paquet RTP. Le buffer est remplacé en place par le SRTP
    /// correspondant (header + ciphertext + auth tag). webrtc-srtp retourne
    /// un `Bytes` neuf → on copie ensuite dans `buf`. Coût : 1 alloc + 1 copy
    /// (~200 octets). Négligeable vs le coût crypto AES-GCM (5-15 µs/paquet).
    pub fn protect(&self, buf: &mut Vec<u8>) -> Result<(), String> {
        let mut tx = self.tx.lock().map_err(|_| "SRTP tx lock poisoned")?;
        let encrypted = tx
            .encrypt_rtp(buf.as_slice())
            .map_err(|e| format!("SRTP encrypt: {e}"))?;
        buf.clear();
        buf.extend_from_slice(&encrypted);
        Ok(())
    }

    /// Déchiffre un paquet SRTP. Comme `protect`, on copie le résultat.
    pub fn unprotect(&self, buf: &mut Vec<u8>) -> Result<(), String> {
        let mut rx = self.rx.lock().map_err(|_| "SRTP rx lock poisoned")?;
        let decrypted = rx
            .decrypt_rtp(buf.as_slice())
            .map_err(|e| format!("SRTP decrypt: {e}"))?;
        buf.clear();
        buf.extend_from_slice(&decrypted);
        Ok(())
    }
}

//! CatCast wire protocol.
//!
//! On the wire (the broker only ever sees this):
//! ```json
//! { "v": 1, "nonce": "<base64-24>", "ct": "<base64-aead>" }
//! ```
//!
//! Plaintext after decrypt:
//! ```json
//! { "target": "<stage-name>", "ts": <unix-ms>, "msg": <Message> }
//! ```
//!
//! Keys come from a per-stage PSK (the stage name) via Argon2id with a
//! fixed, documented salt — derive once, cache. AEAD is XChaCha20-Poly1305.

use base64::Engine;
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

pub const PROTO_VERSION: u8 = 1;

/// Replays older than this (in ms) are silently dropped after a successful
/// decrypt. Big enough for human latency, small enough that capture+replay
/// against the broker has a tight window.
pub const REPLAY_WINDOW_MS: i64 = 60_000;

/// Salt for the Argon2id KDF. Documented as part of the protocol — changing
/// it would break compatibility with all existing stages. Picked once,
/// versioned via `PROTO_VERSION` if we ever need to rotate.
const KDF_SALT: &[u8] = b"catcast-v1-salt!";

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("envelope JSON parse error: {0}")]
    EnvelopeJson(serde_json::Error),
    #[error("plaintext JSON parse error: {0}")]
    PlaintextJson(serde_json::Error),
    #[error("base64 decode error: {0}")]
    Base64(base64::DecodeError),
    #[error("invalid nonce length")]
    BadNonce,
    #[error("decryption failed (wrong key or tampered ciphertext)")]
    Decrypt,
    #[error("kdf error: {0}")]
    Kdf(String),
    #[error("unsupported envelope version: {0}")]
    UnsupportedVersion(u8),
    #[error("message addressed to {actual:?}, not {expected:?}")]
    WrongTarget { expected: String, actual: String },
    #[error("message timestamp {ts} outside replay window of now={now}")]
    Replay { ts: i64, now: i64 },
}

/// What the broker sees. Opaque to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u8,
    pub nonce: String, // base64
    pub ct: String,    // base64
}

/// What's inside the ciphertext.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plaintext {
    pub target: String,
    pub ts: i64,
    pub msg: Message,
}

/// All messages exchanged in either direction. Tagged enum (`type`/`data`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Message {
    // CLI -> stage
    Pause,
    Play,
    Nav { url: String },
    NavTimed { url: String, duration_secs: u64 },
    Manual { on: bool },
    SetConfig { yaml: String },
    SetLogic { rhai: String },
    GetState,

    // Stage -> CLI (single State carries everything)
    State(catcast_core::State),
}

/// A symmetric key derived from a PSK. Cached; never serialised.
pub struct Key([u8; 32]);

impl Key {
    pub fn from_psk(psk: &str) -> Result<Self, ProtoError> {
        use argon2::{Algorithm, Argon2, Params, Version};
        let params =
            Params::new(19_456, 2, 1, Some(32)).map_err(|e| ProtoError::Kdf(e.to_string()))?;
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut out = [0u8; 32];
        argon
            .hash_password_into(psk.as_bytes(), KDF_SALT, &mut out)
            .map_err(|e| ProtoError::Kdf(e.to_string()))?;
        Ok(Self(out))
    }

    fn aead(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(self.0.as_slice().into())
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

pub fn encrypt(key: &Key, target: &str, msg: &Message) -> Result<String, ProtoError> {
    let pt = Plaintext {
        target: target.to_string(),
        ts: chrono::Utc::now().timestamp_millis(),
        msg: msg.clone(),
    };
    let pt_bytes = serde_json::to_vec(&pt).map_err(ProtoError::PlaintextJson)?;
    let mut nonce_bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ct = key
        .aead()
        .encrypt(nonce, pt_bytes.as_ref())
        .map_err(|_| ProtoError::Decrypt)?;
    let env = Envelope {
        v: PROTO_VERSION,
        nonce: base64::engine::general_purpose::STANDARD.encode(nonce_bytes),
        ct: base64::engine::general_purpose::STANDARD.encode(ct),
    };
    serde_json::to_string(&env).map_err(ProtoError::EnvelopeJson)
}

/// Try to decrypt one envelope with one key. Returns the inner plaintext on
/// success, or [`ProtoError::Decrypt`] when the key doesn't match (use that
/// to keep trying the next configured key on the CLI).
pub fn decrypt(key: &Key, envelope: &str) -> Result<Plaintext, ProtoError> {
    let env: Envelope = serde_json::from_str(envelope).map_err(ProtoError::EnvelopeJson)?;
    if env.v != PROTO_VERSION {
        return Err(ProtoError::UnsupportedVersion(env.v));
    }
    let nonce_bytes = base64::engine::general_purpose::STANDARD
        .decode(&env.nonce)
        .map_err(ProtoError::Base64)?;
    if nonce_bytes.len() != 24 {
        return Err(ProtoError::BadNonce);
    }
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ct = base64::engine::general_purpose::STANDARD
        .decode(&env.ct)
        .map_err(ProtoError::Base64)?;
    let pt_bytes = key
        .aead()
        .decrypt(nonce, ct.as_ref())
        .map_err(|_| ProtoError::Decrypt)?;
    let pt: Plaintext = serde_json::from_slice(&pt_bytes).map_err(ProtoError::PlaintextJson)?;
    let now = chrono::Utc::now().timestamp_millis();
    if (now - pt.ts).abs() > REPLAY_WINDOW_MS {
        return Err(ProtoError::Replay { ts: pt.ts, now });
    }
    Ok(pt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let key = Key::from_psk("kitchen").unwrap();
        let msg = Message::Nav {
            url: "https://example.com".into(),
        };
        let env = encrypt(&key, "kitchen", &msg).unwrap();
        let pt = decrypt(&key, &env).unwrap();
        assert_eq!(pt.target, "kitchen");
        match pt.msg {
            Message::Nav { url } => assert_eq!(url, "https://example.com"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn wrong_key_fails_silently() {
        let k1 = Key::from_psk("kitchen").unwrap();
        let k2 = Key::from_psk("lobby").unwrap();
        let env = encrypt(&k1, "kitchen", &Message::Pause).unwrap();
        assert!(matches!(decrypt(&k2, &env), Err(ProtoError::Decrypt)));
    }

    #[test]
    fn envelope_has_no_name_in_plaintext() {
        let key = Key::from_psk("kitchen").unwrap();
        let env_str = encrypt(&key, "kitchen", &Message::Pause).unwrap();
        // The wire envelope must not leak the target name.
        assert!(!env_str.contains("kitchen"));
        // Sanity: it does decode to JSON with v/nonce/ct fields.
        let env: Envelope = serde_json::from_str(&env_str).unwrap();
        assert_eq!(env.v, PROTO_VERSION);
        assert!(!env.nonce.is_empty());
        assert!(!env.ct.is_empty());
    }
}

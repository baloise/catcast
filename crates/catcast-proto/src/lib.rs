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
//!
//! Relay/mirror frames use a second key, derived with HKDF-SHA256 from a
//! random 32-byte secret that `catc relay pair` hands to both stages (see
//! [`Key::from_frame_secret`]). Screen content is never encrypted under a
//! guessable name.

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

/// HKDF `info` for the relay/mirror frame key.
const FRAME_KEY_INFO: &[u8] = b"catcast-frame-v1";

/// Parse the hex frame secret carried by `SetRelay` / `SetMirror`.
pub fn parse_frame_secret(hex: &str) -> Result<[u8; 32], ProtoError> {
    let hex = hex.trim();
    let bad = || ProtoError::Kdf("frame secret must be 64 hex characters".into());
    if hex.len() != 64 {
        return Err(bad());
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| bad())?;
    }
    Ok(out)
}

/// Lowercase hex of `bytes`.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

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

/// How the body at [`Message::Update`]'s `url` is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateEncoding {
    /// The bytes of the binary as they are.
    Raw,
    /// Standard base64 text (catproxy's `/b64/` route); whitespace ignored.
    Base64,
}

/// One hop of the kiosk webview's top-level navigation history
/// (`Message::TraceData`). `ts` is unix milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEntry {
    pub ts: i64,
    pub kind: TraceKind,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceKind {
    /// The scheduler asked the webview to go here.
    Intended,
    /// The webview started a top-level navigation here. Server redirects
    /// (corporate proxy logins, SSO hops) show up as further `Started`
    /// entries without a new `Intended`.
    Started,
    /// A top-level document finished loading at this URL.
    Loaded,
}

/// Edge length in pixels of the square tiles a relay cuts its screenshots
/// into. Part of the protocol: a relay and a mirror must agree, and every
/// [`Message::Frame`] states it explicitly so a future change is visible.
pub const TILE: u32 = 128;

/// Bytes of a tile hash (truncated SHA-256 of the tile's pixels).
pub const TILE_HASH_LEN: usize = 16;

/// One tile in a [`Message::FrameTiles`] batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TileData {
    /// Lowercase hex of the [`TILE_HASH_LEN`]-byte tile hash.
    pub hash: String,
    /// Base64 PNG of the tile.
    pub png: String,
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
    // ─── CLI -> stage: scheduler / content ───────────────────────────────
    Pause,
    Play,
    Nav {
        url: String,
    },
    NavTimed {
        url: String,
        duration_secs: u64,
    },
    /// Step the rotation one slot backward (wraps from index 0 to the last
    /// entry). Useful for operators previewing a rotation manually. Works
    /// regardless of pause mode — bypasses the auto-advance gate.
    Back,
    /// Step the rotation one slot forward (wraps from the last entry back to
    /// index 0). See [`Back`].
    Forward,
    SetConfig {
        yaml: String,
    },
    SetLogic {
        rhai: String,
    },
    GetState,
    /// Ask the stage to return its currently-loaded config.yaml. The reply is
    /// a `ConfigData` (mirroring `GetState`/`State`); no `Reply` is sent.
    GetConfig,
    /// Ask the stage to return its currently-loaded logic.rhai. Reply is a
    /// `LogicData`; no `Reply` is sent.
    GetLogic,
    /// Ask the stage for its recent top-level navigations (intended URL,
    /// every navigation start including redirects, finished loads). Reply
    /// is a `TraceData`; no `Reply` is sent.
    GetTrace,

    // ─── CLI -> stage: physical-machine / window actions ─────────────────
    /// Navigate the kiosk back to its bundled about page. The stage captures
    /// its own `AboutUrl` at startup (platform-specific:
    /// `tauri://localhost/` on Linux, `http://tauri.localhost/` on Windows),
    /// so the CLI doesn't have to know which. Equivalent to pressing F1 at
    /// the physical kiosk.
    NavAbout,
    /// Install (or replace) the Startup-folder shortcut on the stage's OS.
    AutostartInstall,
    /// Remove the Startup-folder shortcut if present.
    AutostartUninstall,
    /// Set or toggle the kiosk window's fullscreen state. `Some(true)`
    /// enters, `Some(false)` exits, `None` toggles the current state.
    Fullscreen {
        on: Option<bool>,
    },
    /// Set or toggle the WebKit/WebView2 inspector. `Some(true)` opens,
    /// `Some(false)` closes, `None` toggles.
    DevTools {
        on: Option<bool>,
    },
    /// Cleanly terminate the catstage process.
    Shutdown,
    /// Replace the running catstage binary. The stage downloads `url`
    /// (base64 text or raw bytes, per `encoding`), verifies the SHA-256,
    /// swaps the file in beside itself, relaunches with its own original
    /// arguments and exits. The URL is transport, the hash is the integrity
    /// anchor, and the AEAD is the authorisation: only a PSK holder can send
    /// this.
    ///
    /// Unlike every other command this yields TWO `Reply`s: an immediate ack
    /// (or a single `ok=false` when validation fails and nothing will
    /// happen), then a terminal one once the new process is alive or the
    /// update was rolled back. The new process's unsolicited `State` is the
    /// definitive "back up" signal.
    Update {
        version: String,
        url: String,
        /// Lowercase hex SHA-256 of the decoded binary.
        sha256: String,
        encoding: UpdateEncoding,
        /// Hex Ed25519 signature over the binary (the release `.sig`
        /// sidecar). Verified against the key compiled into the stage. A
        /// stage that enforces signatures refuses the update when this is
        /// absent; older stages ignore the field. `#[serde(default)]` keeps
        /// the wire backward compatible.
        #[serde(default)]
        sig: Option<String>,
    },
    /// Begin a broker-streamed update: the operator has already downloaded
    /// and verified the binary, and pushes it as `UpdateChunk`s instead of
    /// pointing the stage at a URL. For kiosks whose proxy blocks the
    /// download. Always signed — streaming exists to move trusted bytes.
    UpdateStream {
        version: String,
        /// Raw (decoded) length, for a sanity check before assembling.
        total_len: u64,
        /// Lowercase hex SHA-256 of the assembled binary.
        sha256: String,
        /// Hex Ed25519 signature over the assembled binary.
        sig: String,
        chunk_count: u32,
        /// Raw bytes per chunk before base64 (the last chunk may be shorter).
        chunk_bytes: u32,
    },
    /// One chunk of a streamed update; `data` is base64 of the raw slice at
    /// index `seq`.
    UpdateChunk {
        seq: u32,
        data: String,
    },
    /// All chunks sent — assemble, verify and install.
    UpdateStreamEnd,

    // ─── CLI -> stage: relay / mirror pairing ─────────────────────────────
    /// Make this stage a relay that pushes screenshots of what it shows to
    /// the mirror stage `to`. `to: None` stops relaying. The secret is the
    /// hex of 32 random bytes; frames are encrypted with
    /// [`Key::from_frame_secret`] of it, addressed to the mirror's name.
    SetRelay {
        to: Option<String>,
        #[serde(default)]
        secret_hex: Option<String>,
        /// Delay between a finished page load and the capture, so pages
        /// that fetch their data after `load` have painted.
        settle_secs: u64,
        /// Re-capture interval while the relay stays on one page.
        every_secs: u64,
    },
    /// Make this stage a mirror that shows frames from a paired relay.
    /// `None` forgets the pairing.
    SetMirror {
        #[serde(default)]
        secret_hex: Option<String>,
    },
    /// Set the mirror's tile-cache cap in bytes. Persisted on the mirror and
    /// applied at once (the cache shrinks if needed). The `Reply` reports
    /// the cap and the space in use.
    SetMirrorCache {
        cap_bytes: u64,
    },

    // ─── Relay <-> mirror (frame key only) ───────────────────────────────
    /// A new screenshot: geometry plus the hash of every tile, row-major.
    /// The tiles the mirror lacks follow as [`FrameTiles`](Self::FrameTiles)
    /// (pushed, or pulled with [`FrameNeed`](Self::FrameNeed)); the frame
    /// is shown once [`FrameEnd`](Self::FrameEnd) arrives and every tile
    /// is present.
    Frame {
        seq: u64,
        width: u32,
        height: u32,
        tile: u32,
        /// Base64 of the concatenated [`TILE_HASH_LEN`]-byte tile hashes.
        hashes: String,
        captured_ts: i64,
        source_url: String,
    },
    FrameTiles {
        seq: u64,
        tiles: Vec<TileData>,
    },
    FrameEnd {
        seq: u64,
    },
    /// Mirror -> relay: send these tiles (hex hashes) of frame `seq`. An
    /// empty list asks the relay to repeat its current frame, which a
    /// mirror does after (re)connecting.
    FrameNeed {
        seq: u64,
        tiles: Vec<String>,
    },

    // ─── Stage -> CLI ────────────────────────────────────────────────────
    /// Snapshot of the stage's current State. Sent unsolicited on connect
    /// and in reply to `GetState`. Doubles as a liveness signal.
    State(catcast_core::State),
    /// Per-command outcome. Stages emit exactly one `Reply` for every
    /// command they receive (Pause, Play, Nav, AutostartInstall, …) so the
    /// CLI knows whether the action landed; `Update` is the documented
    /// exception with an ack followed by a terminal reply. `ok=false` carries a
    /// human-readable error string in `message`; `ok=true` may carry a
    /// short success note ("installed at /path", "shortcut removed",
    /// "paused", …) or an empty string when there is nothing useful to
    /// say. `GetState`/`State`/`Reply` themselves do not produce replies.
    Reply {
        ok: bool,
        message: String,
    },
    /// Reply to `GetConfig`. `yaml` is `None` when no config has been pushed
    /// to the stage yet.
    ConfigData {
        yaml: Option<String>,
    },
    /// Reply to `GetLogic`. `rhai` is `None` when no logic has been pushed
    /// to the stage yet.
    LogicData {
        rhai: Option<String>,
    },
    /// Reply to `GetTrace`: newest entry last, bounded by the stage's ring
    /// buffer. `State::actual_url` says where the webview is right now;
    /// this says how it got there.
    TraceData {
        entries: Vec<TraceEntry>,
    },
    /// Stage → CLI during a streamed update: these chunk indices are still
    /// missing (dropped on a reconnect); the CLI resends them. An empty list
    /// is not sent — completion is signalled by the normal `Reply` flow.
    UpdateNeed {
        missing: Vec<u32>,
    },
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

    /// Key for relay/mirror frames: HKDF-SHA256 over a 32-byte random
    /// secret. No password hardening needed — the input has full entropy.
    pub fn from_frame_secret(secret: &[u8; 32]) -> Self {
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(KDF_SALT), secret);
        let mut out = [0u8; 32];
        hk.expand(FRAME_KEY_INFO, &mut out)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Self(out)
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
    fn frame_key_is_deterministic_and_not_the_name_key() {
        let secret = [7u8; 32];
        let a = Key::from_frame_secret(&secret);
        let b = Key::from_frame_secret(&secret);
        let env = encrypt(&a, "mirror", &Message::FrameEnd { seq: 1 }).unwrap();
        assert!(decrypt(&b, &env).is_ok());
        let other = Key::from_frame_secret(&[8u8; 32]);
        assert!(matches!(decrypt(&other, &env), Err(ProtoError::Decrypt)));
        let by_name = Key::from_psk("mirror").unwrap();
        assert!(matches!(decrypt(&by_name, &env), Err(ProtoError::Decrypt)));
    }

    #[test]
    fn frame_secret_hex_round_trip() {
        let secret: [u8; 32] = core::array::from_fn(|i| i as u8 * 7);
        let hex = to_hex(&secret);
        assert_eq!(hex.len(), 64);
        assert_eq!(parse_frame_secret(&hex).unwrap(), secret);
        assert!(parse_frame_secret("abc").is_err());
        assert!(parse_frame_secret(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn relay_messages_round_trip() {
        let key = Key::from_frame_secret(&[1u8; 32]);
        let msgs = [
            Message::Frame {
                seq: 9,
                width: 1920,
                height: 1080,
                tile: TILE,
                hashes: "AAAA".into(),
                captured_ts: 5,
                source_url: "https://x/".into(),
            },
            Message::FrameTiles {
                seq: 9,
                tiles: vec![TileData {
                    hash: "00".repeat(TILE_HASH_LEN),
                    png: "iVBO".into(),
                }],
            },
            Message::FrameEnd { seq: 9 },
            Message::FrameNeed {
                seq: 9,
                tiles: vec![],
            },
            Message::SetRelay {
                to: Some("m".into()),
                secret_hex: Some("00".repeat(32)),
                settle_secs: 3,
                every_secs: 60,
            },
            Message::SetMirror { secret_hex: None },
            Message::SetMirrorCache { cap_bytes: 1 << 28 },
        ];
        for m in msgs {
            let env = encrypt(&key, "m", &m).unwrap();
            let back = decrypt(&key, &env).unwrap().msg;
            assert_eq!(
                serde_json::to_string(&back).unwrap(),
                serde_json::to_string(&m).unwrap()
            );
        }
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

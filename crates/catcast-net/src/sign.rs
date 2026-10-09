//! Release-binary signatures (Ed25519).
//!
//! CI signs every published binary with the CatCast release private key (a
//! GitHub Actions secret); the matching public key is compiled into catstage
//! and catc below. An update is installed only if its detached signature
//! verifies against this key, so an operator — or anyone who learns a stage
//! name — can push only the exact bytes CI built and signed, never an
//! arbitrary binary. The signature is raw Ed25519 (PureEdDSA) over the whole
//! binary, exactly what `openssl pkeyutl -sign -rawin` emits, so CI needs no
//! extra tooling to produce it.

use ed25519_dalek::{Signature, VerifyingKey};

/// The CatCast release signing public key (raw 32-byte Ed25519). The private
/// counterpart lives only in the `CATCAST_RELEASE_KEY_PEM` CI secret. Rotate
/// both together: generate a new key, replace these bytes, update the secret.
pub const RELEASE_PUBKEY: [u8; 32] = [
    0xab, 0x0c, 0x6d, 0xf6, 0x20, 0xef, 0x78, 0x6b, 0xee, 0x71, 0xfa, 0xc9, 0x29, 0x76, 0xbf, 0x94,
    0x1f, 0xae, 0x86, 0xb7, 0x83, 0x2f, 0x26, 0x8d, 0x93, 0xdc, 0x8d, 0x74, 0x8f, 0x43, 0x86, 0xf4,
];

#[derive(Debug, PartialEq, Eq)]
pub enum SignatureError {
    /// The embedded public key is not a valid Ed25519 point (programmer error).
    BadKey,
    /// The signature string is not valid hex.
    BadSigHex,
    /// The signature decoded to something other than 64 bytes.
    BadSigLen(usize),
    /// The signature did not verify against the key and message.
    Invalid,
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadKey => write!(f, "embedded release public key is invalid"),
            Self::BadSigHex => write!(f, "signature is not valid hex"),
            Self::BadSigLen(n) => write!(f, "signature is {n} bytes, expected 64"),
            Self::Invalid => write!(f, "signature does not match the release key"),
        }
    }
}

impl std::error::Error for SignatureError {}

/// Verify a detached hex signature over `bytes` against the embedded release
/// key. `Ok(())` means the bytes are an untampered official release build.
pub fn verify_release(bytes: &[u8], sig_hex: &str) -> Result<(), SignatureError> {
    verify_with(&RELEASE_PUBKEY, bytes, sig_hex)
}

/// Like [`verify_release`] but against an explicit key. Exists so tests can
/// exercise the path with a throwaway key without touching the release key.
pub fn verify_with(pubkey: &[u8; 32], bytes: &[u8], sig_hex: &str) -> Result<(), SignatureError> {
    let vk = VerifyingKey::from_bytes(pubkey).map_err(|_| SignatureError::BadKey)?;
    let raw = decode_hex(sig_hex).ok_or(SignatureError::BadSigHex)?;
    let arr: [u8; 64] = raw
        .as_slice()
        .try_into()
        .map_err(|_| SignatureError::BadSigLen(raw.len()))?;
    let sig = Signature::from_bytes(&arr);
    vk.verify_strict(bytes, &sig)
        .map_err(|_| SignatureError::Invalid)
}

/// Decode hex, ignoring surrounding whitespace/newlines (sidecar files often
/// carry a trailing newline).
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let s: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    // `% 2` rather than `is_multiple_of`: the workspace MSRV is 1.82.
    #[allow(clippy::manual_is_multiple_of)]
    if s.len() % 2 != 0 {
        return None;
    }
    let val = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    s.chunks(2)
        .map(|p| Some(val(p[0])? << 4 | val(p[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A throwaway key + openssl `pkeyutl -sign -rawin` signature over a known
    // message. Locks in that the exact command CI runs produces signatures
    // this verifier accepts — if the wire format of either side ever drifts,
    // this fails. These are NOT the release key.
    const VEC_PUB: [u8; 32] = [
        0xc9, 0x5a, 0xd8, 0x88, 0xa1, 0x92, 0x42, 0x35, 0xe8, 0xfb, 0x1b, 0xeb, 0x79, 0xd0, 0x36,
        0xc6, 0xc5, 0x6e, 0x6d, 0x40, 0xd4, 0x07, 0x34, 0x7b, 0x98, 0xd9, 0x59, 0x63, 0x6c, 0x76,
        0xe7, 0x9f,
    ];
    const VEC_SIG: &str = "c48a7dcd717904c4359114fc1a221766186285ea7070972f54b4c0b34024369e\
                           5237fa13f59ca2619951c19b72d8db9b4d599911961a1c4c0fe77cbefe39b903";
    const VEC_MSG: &[u8] = b"catcast-signing-vector-v1";

    #[test]
    fn verifies_openssl_raw_ed25519() {
        assert_eq!(verify_with(&VEC_PUB, VEC_MSG, VEC_SIG), Ok(()));
    }

    #[test]
    fn rejects_tampered_message() {
        assert_eq!(
            verify_with(&VEC_PUB, b"catcast-signing-vector-v2", VEC_SIG),
            Err(SignatureError::Invalid)
        );
    }

    #[test]
    fn rejects_wrong_key() {
        assert_eq!(
            verify_with(&RELEASE_PUBKEY, VEC_MSG, VEC_SIG),
            Err(SignatureError::Invalid)
        );
    }

    #[test]
    fn rejects_malformed_signature() {
        assert_eq!(verify_with(&VEC_PUB, VEC_MSG, "zz"), Err(SignatureError::BadSigHex));
        assert_eq!(
            verify_with(&VEC_PUB, VEC_MSG, "abcd"),
            Err(SignatureError::BadSigLen(2))
        );
    }

    #[test]
    fn tolerates_whitespace_in_sig() {
        let spaced = format!("  {VEC_SIG}\n");
        assert_eq!(verify_with(&VEC_PUB, VEC_MSG, &spaced), Ok(()));
    }

    #[test]
    fn release_pubkey_is_valid() {
        assert!(VerifyingKey::from_bytes(&RELEASE_PUBKEY).is_ok());
    }
}

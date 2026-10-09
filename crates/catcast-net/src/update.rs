//! Shared self-update primitives for catstage and catc.
//!
//! The pieces here are process-independent: stage a download next to a
//! running binary, verify it (size, SHA-256, platform magic number, and —
//! new — an Ed25519 release signature), and swap it in with a rename that
//! works even while the old image is mapped on Windows. The orchestration
//! that is specific to each binary (what to relaunch, when to exit) stays in
//! the respective crate.
//!
//! A second delivery path assembles a binary from chunks received over the
//! broker ([`ChunkAssembler`]) instead of an HTTP download, for kiosks whose
//! proxy refuses the file: catc downloads and verifies it on the operator
//! box, then streams the bytes through the end-to-end-encrypted channel.

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use catcast_proto::UpdateEncoding;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Anything larger than this is not a CatCast binary.
pub const MAX_BYTES: u64 = 1024 * 1024 * 1024;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Antivirus scanners briefly hold freshly written executables; renames are
/// retried a few times before giving up.
pub const RENAME_RETRIES: usize = 5;
pub const RENAME_BACKOFF: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
pub struct Paths {
    pub exe: PathBuf,
    pub new: PathBuf,
    pub old: PathBuf,
}

/// Locate the running executable and derive its staging/backup siblings.
pub fn paths() -> Result<Paths> {
    let exe = std::env::current_exe().context("locate the running executable")?;
    Ok(paths_for(&exe))
}

/// `catstage.exe` → `catstage.exe.new` / `catstage.exe.old` (same directory,
/// so renames never cross a volume).
pub fn paths_for(exe: &Path) -> Paths {
    let sibling = |suffix: &str| {
        let mut name = exe
            .file_name()
            .unwrap_or(OsStr::new("catcast"))
            .to_os_string();
        name.push(suffix);
        exe.with_file_name(name)
    };
    Paths {
        exe: exe.to_path_buf(),
        new: sibling(".new"),
        old: sibling(".old"),
    }
}

/// Writes the decoded binary while hashing it. Base64 input is decoded per
/// 4-character group; up to three characters carry over between chunks.
pub struct Sink<W: Write> {
    out: W,
    hasher: Sha256,
    carry: Vec<u8>,
    written: u64,
    head: Vec<u8>,
    encoding: UpdateEncoding,
}

pub struct Done {
    pub sha256: String,
    pub len: u64,
    /// First bytes of the file, for the magic-number check.
    pub head: Vec<u8>,
}

impl<W: Write> Sink<W> {
    pub fn new(out: W, encoding: UpdateEncoding) -> Self {
        Self {
            out,
            hasher: Sha256::new(),
            carry: Vec::new(),
            written: 0,
            head: Vec::new(),
            encoding,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<()> {
        match self.encoding {
            UpdateEncoding::Raw => self.write(chunk),
            UpdateEncoding::Base64 => {
                // Tolerate line-wrapped input (`base64` without -w0).
                self.carry
                    .extend(chunk.iter().copied().filter(|b| !b.is_ascii_whitespace()));
                let aligned = self.carry.len() - self.carry.len() % 4;
                if aligned > 0 {
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(&self.carry[..aligned])
                        .context("decode base64 body")?;
                    self.carry.drain(..aligned);
                    self.write(&decoded)?;
                }
                Ok(())
            }
        }
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.written += bytes.len() as u64;
        if self.written > MAX_BYTES {
            bail!("download exceeds {} bytes", MAX_BYTES);
        }
        if self.head.len() < 4 {
            let take = (4 - self.head.len()).min(bytes.len());
            self.head.extend_from_slice(&bytes[..take]);
        }
        self.hasher.update(bytes);
        self.out.write_all(bytes).context("write staging file")
    }

    pub fn finish(mut self) -> Result<Done> {
        if !self.carry.is_empty() {
            // `% 4` rather than `is_multiple_of`: the workspace MSRV is 1.82.
            #[allow(clippy::manual_is_multiple_of)]
            let rem = self.carry.len() % 4;
            if rem != 0 {
                bail!(
                    "truncated base64 body ({} trailing characters)",
                    self.carry.len()
                );
            }
            let rest = std::mem::take(&mut self.carry);
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&rest)
                .context("decode base64 tail")?;
            self.write(&decoded)?;
        }
        self.out.flush().context("flush staging file")?;
        Ok(Done {
            sha256: format!("{:x}", self.hasher.finalize()),
            len: self.written,
            head: self.head,
        })
    }
}

/// Size, SHA-256 and platform magic-number checks. Integrity only — call
/// [`verify_signature`] for authenticity.
pub fn verify(done: &Done, expected_sha256: &str) -> Result<()> {
    if done.len == 0 {
        bail!("downloaded file is empty");
    }
    let expected = expected_sha256.trim().to_ascii_lowercase();
    if done.sha256 != expected {
        bail!("sha256 mismatch: expected {expected}, got {}", done.sha256);
    }
    if !looks_like_executable(std::env::consts::OS, &done.head) {
        bail!(
            "downloaded file is not a {} executable (starts with {:02x?})",
            std::env::consts::OS,
            done.head
        );
    }
    Ok(())
}

/// Verify the Ed25519 release signature over the staged file at `path`.
/// Reads the whole file (a binary is small); PureEdDSA needs the full
/// message. Returns the caller-facing error on any failure.
pub fn verify_signature_file(path: &Path, sig_hex: &str) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("read {} for signature", path.display()))?;
    crate::sign::verify_release(&bytes, sig_hex)
        .map_err(|e| anyhow::anyhow!("signature check failed: {e}"))
}

/// Verify the Ed25519 release signature over in-memory `bytes`.
pub fn verify_signature(bytes: &[u8], sig_hex: &str) -> Result<()> {
    crate::sign::verify_release(bytes, sig_hex)
        .map_err(|e| anyhow::anyhow!("signature check failed: {e}"))
}

pub fn looks_like_executable(os: &str, head: &[u8]) -> bool {
    match os {
        "windows" => head.starts_with(b"MZ"),
        "macos" => matches!(
            head,
            [0xcf, 0xfa, 0xed, 0xfe, ..] | [0xca, 0xfe, 0xba, 0xbe, ..]
        ),
        _ => head.starts_with(b"\x7fELF"),
    }
}

/// Download `url` into `sink`, decoding as `sink`'s encoding dictates.
pub async fn download(url: &str, sink: &mut Sink<File>) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(DOWNLOAD_TIMEOUT)
        .user_agent(concat!("catcast-net/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build http client")?;
    let res = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = res.status();
    if !status.is_success() {
        bail!("{url}: HTTP {status}");
    }
    let mut body = res.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.context("read body")?;
        sink.push(&chunk)?;
    }
    Ok(())
}

/// Native `Unblock-File`: drop the Mark-of-the-Web stream on Windows (a
/// no-op for programmatic downloads, harmless otherwise); make the file
/// executable on Unix. Best effort.
pub fn unblock(path: &Path) {
    #[cfg(windows)]
    {
        let mut ads = path.as_os_str().to_os_string();
        ads.push(":Zone.Identifier");
        match fs::remove_file(&ads) {
            Ok(()) => eprintln!("catcast: removed Zone.Identifier from {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("catcast: could not remove Zone.Identifier: {e}"),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o755)) {
            eprintln!("catcast: chmod 755 {}: {e}", path.display());
        }
    }
}

pub fn rename_retry(from: &Path, to: &Path) -> Result<()> {
    let mut attempt = 0;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if attempt + 1 < RENAME_RETRIES => {
                eprintln!(
                    "catcast: rename {} → {} failed ({e}), retrying",
                    from.display(),
                    to.display()
                );
                attempt += 1;
                std::thread::sleep(RENAME_BACKOFF);
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("rename {} → {}", from.display(), to.display()))
            }
        }
    }
}

/// running `exe` → `.old`, `.new` → `exe`. If the second step fails the first
/// is undone, so the previous binary stays in place.
pub fn swap(p: &Paths) -> Result<()> {
    let _ = fs::remove_file(&p.old); // leftover from an earlier update
    rename_retry(&p.exe, &p.old).context("move the running binary aside")?;
    if let Err(e) = rename_retry(&p.new, &p.exe) {
        let _ = rename_retry(&p.old, &p.exe);
        return Err(e.context("move the new binary into place (previous one restored)"));
    }
    Ok(())
}

/// After a failed relaunch: `exe` (the bad new file) → `.new`, `.old` → `exe`.
pub fn rollback(p: &Paths) -> Result<()> {
    let _ = fs::remove_file(&p.new);
    rename_retry(&p.exe, &p.new).context("move the failed binary aside")?;
    rename_retry(&p.old, &p.exe).context("restore the previous binary")?;
    let _ = fs::remove_file(&p.new);
    Ok(())
}

/// Reassembles a binary streamed over the broker as a sequence of raw-byte
/// chunks. Pure and sync so it is unit-testable; the async send/receive loop
/// lives in catc and catstage. Chunks may arrive out of order or be re-sent;
/// [`missing`](Self::missing) drives the resend request.
pub struct ChunkAssembler {
    chunks: Vec<Option<Vec<u8>>>,
    received: usize,
}

impl ChunkAssembler {
    pub fn new(count: usize) -> Self {
        Self {
            chunks: (0..count).map(|_| None).collect(),
            received: 0,
        }
    }

    /// Store one chunk's raw bytes. Returns `true` if it was newly stored,
    /// `false` for an out-of-range index or a duplicate.
    pub fn add(&mut self, seq: usize, bytes: Vec<u8>) -> bool {
        match self.chunks.get_mut(seq) {
            Some(slot @ None) => {
                *slot = Some(bytes);
                self.received += 1;
                true
            }
            _ => false,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.received == self.chunks.len()
    }

    /// Indices not yet received, for a resend request.
    pub fn missing(&self) -> Vec<u32> {
        self.chunks
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.is_none().then_some(i as u32))
            .collect()
    }

    /// Concatenate the chunks in order. Returns `None` unless complete.
    pub fn assemble(&self) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }
        let mut out = Vec::new();
        for c in &self.chunks {
            out.extend_from_slice(c.as_ref().expect("complete implies all present"));
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded(n: usize) -> Vec<u8> {
        let mut x: u32 = 0x9e37_79b9;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x & 255) as u8
            })
            .collect()
    }

    fn sha_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn paths_are_siblings_with_suffixes() {
        let p = paths_for(Path::new("/opt/cat/catstage.exe"));
        assert_eq!(p.new, Path::new("/opt/cat/catstage.exe.new"));
        assert_eq!(p.old, Path::new("/opt/cat/catstage.exe.old"));
        let p = paths_for(Path::new("/opt/cat/catc"));
        assert_eq!(p.old, Path::new("/opt/cat/catc.old"));
    }

    #[test]
    fn base64_sink_decodes_at_any_chunking() {
        let input = seeded(1024 * 1024 + 5);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&input);
        for size in [1usize, 2, 3, 4, 5, 7, 4093, 65537] {
            let mut sink = Sink::new(Vec::new(), UpdateEncoding::Base64);
            for chunk in encoded.as_bytes().chunks(size) {
                sink.push(chunk).unwrap();
            }
            let done = sink.finish().unwrap();
            assert_eq!(done.len, input.len() as u64, "chunk {size}");
            assert_eq!(done.sha256, sha_hex(&input), "chunk {size}");
            assert_eq!(done.head, input[..4]);
        }
    }

    #[test]
    fn raw_sink_hashes_and_keeps_head() {
        let input = b"MZ\x90\x00rest of a pe file".to_vec();
        let mut out = Vec::new();
        let done = {
            let mut sink = Sink::new(&mut out, UpdateEncoding::Raw);
            sink.push(&input[..1]).unwrap();
            sink.push(&input[1..]).unwrap();
            sink.finish().unwrap()
        };
        assert_eq!(out, input);
        assert_eq!(done.head, b"MZ\x90\x00");
        assert_eq!(done.sha256, sha_hex(&input));
    }

    #[test]
    fn verify_checks_hash_size_and_magic() {
        let head = if cfg!(windows) {
            b"MZ\x90\x00".to_vec()
        } else if cfg!(target_os = "macos") {
            vec![0xcf, 0xfa, 0xed, 0xfe]
        } else {
            b"\x7fELF".to_vec()
        };
        let ok = Done {
            sha256: "ab".repeat(32),
            len: 10,
            head,
        };
        assert!(
            verify(&ok, &"AB".repeat(32)).is_ok(),
            "case-insensitive hex"
        );
        assert!(verify(&ok, &"cd".repeat(32)).is_err(), "wrong hash");
        let empty = Done {
            sha256: "ab".repeat(32),
            len: 0,
            head: ok.head.clone(),
        };
        assert!(verify(&empty, &"ab".repeat(32)).is_err(), "empty file");
        let notexe = Done {
            sha256: "ab".repeat(32),
            len: 10,
            head: b"<htm".to_vec(),
        };
        assert!(
            verify(&notexe, &"ab".repeat(32)).is_err(),
            "not an executable"
        );
    }

    #[test]
    fn assembler_reorders_dedups_and_reports_gaps() {
        let mut a = ChunkAssembler::new(4);
        assert_eq!(a.missing(), vec![0, 1, 2, 3]);
        assert!(a.add(2, vec![2]));
        assert!(!a.add(2, vec![2]), "duplicate is not newly stored");
        assert!(!a.add(9, vec![9]), "out of range");
        assert!(a.add(0, vec![0]));
        assert_eq!(a.missing(), vec![1, 3]);
        assert!(a.assemble().is_none());
        assert!(a.add(3, vec![3, 3]));
        assert!(a.add(1, vec![1]));
        assert!(a.is_complete());
        assert_eq!(a.assemble().unwrap(), vec![0, 1, 2, 3, 3]);
    }
}

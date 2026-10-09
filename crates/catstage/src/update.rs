//! Self-update for catstage: stage a new binary, verify it, swap it in,
//! relaunch, exit.
//!
//! The reusable primitives (download, hashing Sink, size/magic verify,
//! Ed25519 signature check, swap/rollback) live in [`catcast_net::update`];
//! this module keeps the stage-specific policy: the one-update-at-a-time
//! lock, signature enforcement, the two delivery paths (HTTP URL and
//! broker-streamed chunks), and the relaunch/liveness/rollback dance.
//!
//! Driven by [`catcast_proto::Message::Update`] (URL) and `UpdateStream` /
//! `UpdateChunk` / `UpdateStreamEnd` (streamed). Integrity is the SHA-256
//! pinned by the operator; authenticity is the release signature verified
//! against the key compiled into catcast-net. Nothing here runs on a timer.

use anyhow::{bail, Context, Result};
use catcast_net::update::{self as core, ChunkAssembler, Paths, Sink};
use catcast_proto::UpdateEncoding;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// One update at a time per process.
static UPDATING: AtomicBool = AtomicBool::new(false);

/// How long the replacement must stay alive before we trust it.
const LIVENESS_WINDOW: Duration = Duration::from_secs(3);
/// Delay before deleting leftovers at boot.
const STALE_CLEANUP_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct Request {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub encoding: UpdateEncoding,
    /// Hex Ed25519 signature over the binary. Required: an update with no
    /// signature is refused (older releases predate signing — push those by
    /// hand). `None` only reaches here from an older CLI.
    pub sig: Option<String>,
}

/// Validate inputs and open the staging file before any bytes move. On
/// success the update lock is held; [`perform`] releases it on failure.
pub fn begin(req: &Request) -> Result<(Paths, File)> {
    if req.sha256.len() != 64 || !req.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("sha256 must be 64 hex characters");
    }
    if !(req.url.starts_with("https://") || req.url.starts_with("http://")) {
        bail!("url must be http(s)");
    }
    acquire(|| core::paths().and_then(|p| Ok((p.clone(), preflight(&p)?))))
}

/// Acquire the single-update lock, run `f`, and release the lock if `f`
/// failed (success hands the lock to the caller, released by `perform` /
/// `finish_stream`).
fn acquire<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    if UPDATING.swap(true, Ordering::SeqCst) {
        bail!("an update is already in progress");
    }
    let out = f();
    if out.is_err() {
        UPDATING.store(false, Ordering::SeqCst);
    }
    out
}

/// Creating the staging file is the writability test: an MSI install under
/// Program Files fails here with a clear message instead of halfway through.
fn preflight(paths: &Paths) -> Result<File> {
    match File::create(&paths.new) {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => bail!(
            "install directory {} is not writable (MSI / Program Files install?) — update manually",
            paths
                .exe
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ),
        Err(e) => Err(e).with_context(|| format!("create {}", paths.new.display())),
    }
}

/// A signature is mandatory. Verify `sig` over the staged file; refuse when
/// it is absent so a missing-signature downgrade cannot slip an unsigned
/// binary past the check.
fn enforce_signature(staged: &Path, sig: Option<&str>) -> Result<()> {
    let sig = sig.ok_or_else(|| {
        anyhow::anyhow!(
            "update has no signature — the release predates signed builds, or the CLI is too \
             old; install it by hand or upgrade catc"
        )
    })?;
    core::verify_signature_file(staged, sig)
}

/// Download, verify, swap, relaunch (URL path). `Ok` means the new process is
/// alive and the caller should exit; `Err` means the previous binary is still
/// running and nothing is lost.
pub async fn perform(
    req: Request,
    paths: Paths,
    file: File,
    launch_args: Vec<OsString>,
) -> Result<()> {
    let result = perform_inner(&req, &paths, file, launch_args).await;
    if result.is_err() {
        let _ = fs::remove_file(&paths.new);
        UPDATING.store(false, Ordering::SeqCst);
    }
    result
}

async fn perform_inner(
    req: &Request,
    paths: &Paths,
    file: File,
    launch_args: Vec<OsString>,
) -> Result<()> {
    let mut sink = Sink::new(file, req.encoding);
    core::download(&req.url, &mut sink).await?;
    let done = sink.finish()?;
    core::verify(&done, &req.sha256)?;
    enforce_signature(&paths.new, req.sig.as_deref())?;
    eprintln!(
        "catstage: downloaded v{} ({} bytes, sha256 + signature ok)",
        req.version, done.len
    );
    finish_install(paths, &req.version, launch_args).await
}

// ── Broker-streamed updates ────────────────────────────────────────────────

/// Metadata from `Message::UpdateStream`, carried through chunk receipt.
#[derive(Debug, Clone)]
pub struct StreamMeta {
    pub version: String,
    pub total_len: u64,
    pub sha256: String,
    pub sig: String,
    pub chunk_count: u32,
    pub chunk_bytes: u32,
}

/// In-progress streamed update. Holds the update lock for its lifetime;
/// dropping it releases the lock (so a superseding `UpdateStream` or an
/// abandoned transfer frees it). Lives in managed state across the many
/// inbound chunk messages.
pub struct StreamState {
    meta: StreamMeta,
    asm: ChunkAssembler,
}

impl Drop for StreamState {
    fn drop(&mut self) {
        UPDATING.store(false, Ordering::SeqCst);
    }
}

impl StreamState {
    pub fn add_chunk(&mut self, seq: u32, bytes: Vec<u8>) -> bool {
        self.asm.add(seq as usize, bytes)
    }
    pub fn is_complete(&self) -> bool {
        self.asm.is_complete()
    }
    pub fn missing(&self) -> Vec<u32> {
        self.asm.missing()
    }
    pub fn version(&self) -> &str {
        &self.meta.version
    }
}

/// Begin receiving a streamed update. Acquires the update lock and validates
/// the announced metadata; the caller stores the returned state and feeds it
/// chunks. A chunk count of zero or a bad hash length is rejected up front.
pub fn begin_stream(meta: StreamMeta) -> Result<StreamState> {
    if meta.sha256.len() != 64 || !meta.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("sha256 must be 64 hex characters");
    }
    if meta.chunk_count == 0 {
        bail!("stream announced zero chunks");
    }
    if meta.total_len > core::MAX_BYTES {
        bail!("announced size {} exceeds the limit", meta.total_len);
    }
    // Geometry sanity: `chunk_count` chunks of `chunk_bytes` must be able to
    // hold exactly `total_len` (last chunk 1..=chunk_bytes). Catches a
    // mismatched sender before any bytes are buffered.
    let cb = meta.chunk_bytes as u64;
    let cc = meta.chunk_count as u64;
    if cb == 0 || meta.total_len == 0 {
        bail!("chunk_bytes and total_len must be non-zero");
    }
    let max = cb.saturating_mul(cc);
    let min = cb.saturating_mul(cc - 1) + 1;
    if meta.total_len > max || meta.total_len < min {
        bail!(
            "announced geometry inconsistent: {} bytes does not fit {} chunk(s) of {}",
            meta.total_len,
            cc,
            cb
        );
    }
    acquire(|| {
        let asm = ChunkAssembler::new(meta.chunk_count as usize);
        Ok(StreamState { meta, asm })
    })
}

/// All chunks received: assemble, write the staging file, verify size /
/// SHA-256 / magic / signature, then swap and relaunch. Consumes the state
/// (releasing the lock on the way out, via `finish_install` or the error
/// path). `Ok` means the new process is alive and the caller should exit.
pub async fn finish_stream(state: StreamState, launch_args: Vec<OsString>) -> Result<()> {
    let result = finish_stream_inner(&state, launch_args).await;
    // On failure, drop clears the lock and the staging file is removed below.
    if result.is_err() {
        if let Ok(p) = core::paths() {
            let _ = fs::remove_file(&p.new);
        }
    } else {
        // Success handed the lock to the new process' lifetime already via
        // finish_install keeping UPDATING set until exit; forget the guard so
        // Drop does not clear it under the relaunching process.
        std::mem::forget(state);
    }
    result
}

async fn finish_stream_inner(state: &StreamState, launch_args: Vec<OsString>) -> Result<()> {
    let bytes = state.asm.assemble().ok_or_else(|| {
        anyhow::anyhow!(
            "stream incomplete: {} chunk(s) missing",
            state.missing().len()
        )
    })?;
    if bytes.len() as u64 != state.meta.total_len {
        bail!(
            "assembled {} bytes, stream announced {}",
            bytes.len(),
            state.meta.total_len
        );
    }
    let paths = core::paths()?;
    let file = preflight(&paths)?;
    let mut sink = Sink::new(file, UpdateEncoding::Raw);
    sink.push(&bytes)?;
    let done = sink.finish()?;
    core::verify(&done, &state.meta.sha256)?;
    enforce_signature(&paths.new, Some(&state.meta.sig))?;
    eprintln!(
        "catstage: received v{} over the broker ({} bytes, sha256 + signature ok)",
        state.meta.version, done.len
    );
    finish_install(&paths, &state.meta.version, launch_args).await
}

// ── Install tail shared by both paths ───────────────────────────────────────

/// Unblock, swap, relaunch, and confirm liveness. On any failure rolls back
/// to the previous binary and returns the error. The staged file at
/// `paths.new` is expected to be fully written and verified.
async fn finish_install(paths: &Paths, version: &str, launch_args: Vec<OsString>) -> Result<()> {
    core::unblock(&paths.new);

    let p = paths.clone();
    tokio::task::spawn_blocking(move || core::swap(&p))
        .await
        .context("swap task")??;

    let mut child = match spawn_replacement(&paths.exe, &launch_args) {
        Ok(c) => c,
        Err(e) => {
            let p = paths.clone();
            let restored = tokio::task::spawn_blocking(move || core::rollback(&p)).await;
            return Err(e.context(match restored {
                Ok(Ok(())) => "rolled back to the previous binary".to_string(),
                other => format!("AND rollback failed: {other:?}"),
            }));
        }
    };

    let deadline = Instant::now() + LIVENESS_WINDOW;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().context("poll new process")? {
            let p = paths.clone();
            let restored = tokio::task::spawn_blocking(move || core::rollback(&p)).await;
            match restored {
                Ok(Ok(())) => bail!(
                    "new binary exited with {status} within {}s — rolled back",
                    LIVENESS_WINDOW.as_secs()
                ),
                other => bail!(
                    "new binary exited with {status} within {}s AND rollback failed: {other:?}",
                    LIVENESS_WINDOW.as_secs()
                ),
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = version;
    Ok(())
}

fn spawn_replacement(exe: &Path, args: &[OsString]) -> Result<Child> {
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()
        .with_context(|| format!("launch {}", exe.display()))
}

/// Delete `.old` / `.new` leftovers a while after boot. Best effort.
pub fn spawn_stale_cleanup() {
    tokio::spawn(async {
        tokio::time::sleep(STALE_CLEANUP_DELAY).await;
        let Ok(p) = core::paths() else { return };
        for f in [&p.old, &p.new] {
            match fs::remove_file(f) {
                Ok(()) => eprintln!("catstage: removed stale {}", f.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("catstage: could not remove {}: {e}", f.display()),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use catcast_net::update::paths_for;
    use std::path::PathBuf;

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "catstage-update-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn swap_then_rollback_restores_previous_binary() {
        let dir = tempdir("swap");
        let p = paths_for(&dir.join("catstage"));
        fs::write(&p.exe, b"old").unwrap();
        fs::write(&p.new, b"new").unwrap();
        core::swap(&p).unwrap();
        assert_eq!(fs::read(&p.exe).unwrap(), b"new");
        assert_eq!(fs::read(&p.old).unwrap(), b"old");
        core::rollback(&p).unwrap();
        assert_eq!(fs::read(&p.exe).unwrap(), b"old");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn begin_rejects_bad_input_without_taking_the_lock() {
        let bad_hash = Request {
            version: "1.0.0".into(),
            url: "https://x/".into(),
            sha256: "nope".into(),
            encoding: UpdateEncoding::Raw,
            sig: None,
        };
        assert!(begin(&bad_hash).is_err());
        let bad_url = Request {
            sha256: "0".repeat(64),
            url: "ftp://x/".into(),
            ..bad_hash
        };
        assert!(begin(&bad_url).is_err());
        assert!(!UPDATING.load(Ordering::SeqCst));
    }

    #[test]
    fn enforce_signature_refuses_missing() {
        let dir = tempdir("sig");
        let f = dir.join("catstage.new");
        fs::write(&f, b"MZ..").unwrap();
        assert!(enforce_signature(&f, None).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn begin_stream_validates_and_releases_on_bad_input() {
        let meta = |sha: &str, count: u32| StreamMeta {
            version: "1.0.0".into(),
            total_len: 10,
            sha256: sha.into(),
            sig: "00".repeat(64),
            chunk_count: count,
            chunk_bytes: 8,
        };
        assert!(begin_stream(meta("nope", 2)).is_err());
        assert!(
            !UPDATING.load(Ordering::SeqCst),
            "lock released on bad hash"
        );
        assert!(begin_stream(meta(&"0".repeat(64), 0)).is_err());
        assert!(
            !UPDATING.load(Ordering::SeqCst),
            "lock released on zero chunks"
        );
        // Valid: acquires the lock; dropping the state releases it.
        let st = begin_stream(meta(&"0".repeat(64), 2)).unwrap();
        assert!(UPDATING.load(Ordering::SeqCst));
        drop(st);
        assert!(!UPDATING.load(Ordering::SeqCst), "drop releases the lock");
    }
}

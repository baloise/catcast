//! Self-update: download a new catstage binary, verify it, swap it in
//! beside the running one, relaunch, exit.
//!
//! Driven by [`catcast_proto::Message::Update`]. The design in one breath:
//!
//! - The file is written next to the running executable as `<exe>.new`, so
//!   the final rename is atomic (same volume) and "is this directory
//!   writable?" doubles as the MSI / Program-Files detection.
//! - The body may arrive as base64 text (catproxy's `/b64/` route, which is
//!   how executables get past content-sniffing corporate proxies) or raw.
//! - Integrity is the SHA-256 pinned into the command by the operator;
//!   a platform magic-number check catches "that was an HTML error page".
//! - Windows cannot overwrite a running image but can rename it: the
//!   running binary becomes `<exe>.old`, `.new` becomes `<exe>`, the new
//!   process is spawned with the process's own original arguments, and if it
//!   is still alive after a few seconds the old process exits. If it died,
//!   `.old` is renamed back. `.old` survives one generation and is deleted
//!   on the next boot, so a manual rename is always an option.
//! - A programmatic download carries no Mark-of-the-Web, but the
//!   `Zone.Identifier` stream is removed anyway (what `Unblock-File` does).
//!
//! Nothing here runs on a timer. An update happens only when an operator
//! sends the command.

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use catcast_proto::UpdateEncoding;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// One update at a time per process.
static UPDATING: AtomicBool = AtomicBool::new(false);

/// Anything larger than this is not a catstage binary.
const MAX_BYTES: u64 = 1024 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Antivirus scanners briefly hold freshly written executables; renames
/// are retried a few times before giving up.
const RENAME_RETRIES: usize = 5;
const RENAME_BACKOFF: Duration = Duration::from_millis(200);
/// How long the replacement must stay alive before we trust it.
const LIVENESS_WINDOW: Duration = Duration::from_secs(3);
/// Delay before deleting leftovers at boot: long enough for the previous
/// process to have exited (Windows cannot delete a mapped image).
const STALE_CLEANUP_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct Request {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub encoding: UpdateEncoding,
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub exe: PathBuf,
    pub new: PathBuf,
    pub old: PathBuf,
}

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
            .unwrap_or(OsStr::new("catstage"))
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

/// Everything that can be rejected before any bytes move. On success the
/// update lock is held and the staging file is open; [`perform`] releases
/// the lock on failure.
pub fn begin(req: &Request) -> Result<(Paths, File)> {
    if req.sha256.len() != 64 || !req.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("sha256 must be 64 hex characters");
    }
    if !(req.url.starts_with("https://") || req.url.starts_with("http://")) {
        bail!("url must be http(s)");
    }
    if UPDATING.swap(true, Ordering::SeqCst) {
        bail!("an update is already in progress");
    }
    let staged = paths().and_then(|p| {
        let f = preflight(&p)?;
        Ok((p, f))
    });
    if staged.is_err() {
        UPDATING.store(false, Ordering::SeqCst);
    }
    staged
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

/// Download, verify, swap, relaunch. `Ok` means the new process is alive and
/// the caller should exit. `Err` means the previous binary is still in place
/// and running; nothing is lost.
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
    download(&req.url, &mut sink).await?;
    let done = sink.finish()?;
    verify(&done, &req.sha256)?;
    eprintln!(
        "catstage: downloaded v{} ({} bytes, sha256 ok)",
        req.version, done.len
    );
    unblock(&paths.new);

    let p = paths.clone();
    tokio::task::spawn_blocking(move || swap(&p))
        .await
        .context("swap task")??;

    let mut child = match spawn_replacement(&paths.exe, &launch_args) {
        Ok(c) => c,
        Err(e) => {
            let p = paths.clone();
            let restored = tokio::task::spawn_blocking(move || rollback(&p)).await;
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
            let restored = tokio::task::spawn_blocking(move || rollback(&p)).await;
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
    Ok(())
}

async fn download(url: &str, sink: &mut Sink<File>) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(DOWNLOAD_TIMEOUT)
        .user_agent(concat!("catstage/", env!("CARGO_PKG_VERSION")))
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

/// Writes the decoded binary while hashing it. Base64 input is decoded
/// per 4-character group; up to three characters carry over between chunks.
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
            // `%` rather than `is_multiple_of`: the workspace MSRV is 1.82.
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

fn looks_like_executable(os: &str, head: &[u8]) -> bool {
    match os {
        "windows" => head.starts_with(b"MZ"),
        "macos" => matches!(
            head,
            [0xcf, 0xfa, 0xed, 0xfe, ..] | [0xca, 0xfe, 0xba, 0xbe, ..]
        ),
        _ => head.starts_with(b"\x7fELF"),
    }
}

/// Native `Unblock-File`: drop the Mark-of-the-Web stream on Windows (a
/// no-op for programmatic downloads, harmless otherwise); make the file
/// executable on Unix. Best effort — a failure here is logged, because the
/// swap and the liveness check decide whether the binary actually runs.
pub fn unblock(path: &Path) {
    #[cfg(windows)]
    {
        let mut ads = path.as_os_str().to_os_string();
        ads.push(":Zone.Identifier");
        match fs::remove_file(&ads) {
            Ok(()) => eprintln!("catstage: removed Zone.Identifier from {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("catstage: could not remove Zone.Identifier: {e}"),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o755)) {
            eprintln!("catstage: chmod 755 {}: {e}", path.display());
        }
    }
}

fn rename_retry(from: &Path, to: &Path) -> Result<()> {
    let mut attempt = 0;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if attempt + 1 < RENAME_RETRIES => {
                eprintln!(
                    "catstage: rename {} → {} failed ({e}), retrying",
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

/// running `exe` → `.old`, `.new` → `exe`. If the second step fails the
/// first is undone, so the previous binary stays in place.
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
        let Ok(p) = paths() else { return };
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
    fn paths_are_siblings_with_suffixes() {
        let p = paths_for(Path::new("/opt/cat/catstage.exe"));
        assert_eq!(p.new, Path::new("/opt/cat/catstage.exe.new"));
        assert_eq!(p.old, Path::new("/opt/cat/catstage.exe.old"));
        let p = paths_for(Path::new("/opt/cat/catstage"));
        assert_eq!(p.old, Path::new("/opt/cat/catstage.old"));
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
            let hash_before = sink.written;
            let done = sink.finish().unwrap();
            assert!(hash_before <= done.len);
            assert_eq!(done.len, input.len() as u64, "chunk {size}");
            assert_eq!(done.sha256, sha_hex(&input), "chunk {size}");
            assert_eq!(done.head, input[..4]);
        }
    }

    #[test]
    fn base64_sink_ignores_line_wrapping_and_rejects_truncation() {
        let input = seeded(300);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&input);
        let wrapped: String = encoded
            .as_bytes()
            .chunks(76)
            .map(|c| format!("{}\r\n", std::str::from_utf8(c).unwrap()))
            .collect();
        let mut sink = Sink::new(Vec::new(), UpdateEncoding::Base64);
        sink.push(wrapped.as_bytes()).unwrap();
        assert_eq!(sink.finish().unwrap().sha256, sha_hex(&input));

        let mut sink = Sink::new(Vec::new(), UpdateEncoding::Base64);
        sink.push(&encoded.as_bytes()[..encoded.len() - 1]).unwrap();
        assert!(sink.finish().is_err());
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
        let ok = Done {
            sha256: "ab".repeat(32),
            len: 10,
            head: if cfg!(windows) {
                b"MZ\x90\x00".to_vec()
            } else if cfg!(target_os = "macos") {
                vec![0xcf, 0xfa, 0xed, 0xfe]
            } else {
                b"\x7fELF".to_vec()
            },
        };
        assert!(
            verify(&ok, &"AB".repeat(32)).is_ok(),
            "case-insensitive hex"
        );
        assert!(verify(&ok, &"cd".repeat(32)).is_err());
        assert!(verify(
            &Done {
                len: 0,
                ..clone(&ok)
            },
            &ok.sha256
        )
        .is_err());
        assert!(verify(
            &Done {
                head: b"<!DO".to_vec(),
                ..clone(&ok)
            },
            &ok.sha256
        )
        .is_err());
    }

    fn clone(d: &Done) -> Done {
        Done {
            sha256: d.sha256.clone(),
            len: d.len,
            head: d.head.clone(),
        }
    }

    #[test]
    fn magic_numbers_per_os() {
        assert!(looks_like_executable("windows", b"MZ\x90\x00"));
        assert!(looks_like_executable("linux", b"\x7fELF"));
        assert!(looks_like_executable("macos", &[0xcf, 0xfa, 0xed, 0xfe]));
        assert!(looks_like_executable("macos", &[0xca, 0xfe, 0xba, 0xbe]));
        assert!(!looks_like_executable("windows", b"<!DOCTYPE"));
        assert!(!looks_like_executable("linux", b"MZ\x90\x00"));
    }

    #[test]
    fn swap_then_rollback_restores_previous_binary() {
        let dir = tempdir("swap");
        let p = paths_for(&dir.join("catstage"));
        fs::write(&p.exe, b"old").unwrap();
        fs::write(&p.new, b"new").unwrap();

        swap(&p).unwrap();
        assert_eq!(fs::read(&p.exe).unwrap(), b"new");
        assert_eq!(fs::read(&p.old).unwrap(), b"old");
        assert!(!p.new.exists());

        rollback(&p).unwrap();
        assert_eq!(fs::read(&p.exe).unwrap(), b"old");
        assert!(!p.old.exists());
        assert!(!p.new.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn swap_without_new_file_leaves_exe_in_place() {
        let dir = tempdir("noswap");
        let p = paths_for(&dir.join("catstage"));
        fs::write(&p.exe, b"old").unwrap();
        assert!(swap(&p).is_err());
        assert_eq!(fs::read(&p.exe).unwrap(), b"old");
        assert!(!p.old.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn begin_rejects_bad_input_without_taking_the_lock() {
        let bad_hash = Request {
            version: "1.0.0".into(),
            url: "https://x/".into(),
            sha256: "nope".into(),
            encoding: UpdateEncoding::Raw,
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
}

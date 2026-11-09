//! `catc config edit` / `catc logic edit`: download a stage's config or
//! logic into a working copy beside `catc.toml`, open it in the user's
//! editor, then validate, diff and offer to push it back — the way
//! `git commit` handles its message.
//!
//! Next to each working copy sits a hidden snapshot of the stage's version
//! as last downloaded or pushed. Comparing copy, snapshot and a fresh
//! download tells "the stage changed elsewhere" apart from "I have edits I
//! never pushed".

use crate::cfg::{self, CatcConfig};
use crate::cli::{cmd_send, fetch_one, resolve_targets, EditOpts, Targets};
use anyhow::{anyhow, bail, Context, Result};
use catcast_core::Config;
use catcast_proto::Message;
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Config,
    Logic,
}

impl Kind {
    fn file_name(self) -> &'static str {
        match self {
            Kind::Config => "config.yaml",
            Kind::Logic => "logic.rhai",
        }
    }

    fn what(self) -> &'static str {
        match self {
            Kind::Config => "config",
            Kind::Logic => "logic",
        }
    }

    fn request(self) -> Message {
        match self {
            Kind::Config => Message::GetConfig,
            Kind::Logic => Message::GetLogic,
        }
    }

    fn set(self, text: String) -> Message {
        match self {
            Kind::Config => Message::SetConfig { yaml: text },
            Kind::Logic => Message::SetLogic { rhai: text },
        }
    }
}

/// Where the working copy and its snapshot live for one stage.
struct Paths {
    copy: PathBuf,
    snapshot: PathBuf,
}

impl Paths {
    fn new(stage: &str, kind: Kind) -> Result<Self> {
        let dir = cfg::stage_dir(stage)?;
        Ok(Self {
            copy: dir.join(kind.file_name()),
            snapshot: dir.join(format!(".{}.remote", kind.file_name())),
        })
    }
}

/// What to start editing from, given the working copy (`local`), the
/// snapshot of the stage's version last seen (`base`) and the stage's
/// version right now (`remote`).
#[derive(Debug, PartialEq, Eq)]
enum Start {
    /// No copy yet, or it already matches the stage.
    Remote,
    /// The copy was unedited but the stage changed since: take the stage's.
    Refreshed,
    /// The stage is unchanged but the copy holds edits never pushed.
    ResumeLocal,
    /// Both moved (or there is no snapshot to tell): the user decides.
    Conflict,
}

fn reconcile(local: Option<&str>, base: Option<&str>, remote: &str) -> Start {
    let Some(local) = local else {
        return Start::Remote;
    };
    if local == remote {
        return Start::Remote;
    }
    match base {
        Some(b) if b == local => Start::Refreshed,
        Some(b) if b == remote => Start::ResumeLocal,
        _ => Start::Conflict,
    }
}

pub async fn run(kind: Kind, o: EditOpts) -> Result<()> {
    let cfg = CatcConfig::load()?;
    let names = resolve_targets(&cfg, &o.targets)?;
    if names.len() != 1 {
        bail!("edit needs exactly one target (--name N)");
    }
    let name = names[0].clone();
    let paths = Paths::new(&name, kind)?;

    let (yaml, rhai) = fetch_one(&cfg, &name, &[kind.request()]).await?;
    let got = match kind {
        Kind::Config => yaml,
        Kind::Logic => rhai,
    };
    let remote = got.ok_or_else(|| {
        anyhow!(
            "{name}: no reply within 2s (stage offline?) — nothing to edit against{}",
            if paths.copy.exists() {
                format!("; your copy is {}", paths.copy.display())
            } else {
                String::new()
            }
        )
    })?;
    let remote = match remote {
        Some(t) => normalize(&t),
        None => {
            println!(
                "{name}: no {} loaded; starting from an empty file",
                kind.what()
            );
            String::new()
        }
    };

    let local = read_opt(&paths.copy)?;
    let base = read_opt(&paths.snapshot)?;
    if let Some(dir) = paths.copy.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    match reconcile(local.as_deref(), base.as_deref(), &remote) {
        Start::Remote => write(&paths.copy, &remote)?,
        Start::Refreshed => {
            println!(
                "{name}: the {} changed on the stage since your last edit; refreshed your copy",
                kind.what()
            );
            write(&paths.copy, &remote)?;
        }
        Start::ResumeLocal => println!("{name}: resuming your unpushed edits"),
        Start::Conflict => {
            let local = local.expect("conflict implies a local copy");
            println!(
                "{name}: your copy and the stage's {} both changed (- yours, + stage):",
                kind.what()
            );
            print!("{}", render_diff(&local, &remote));
            match ask("keep [l]ocal, take [r]emote, or [q]uit? ")?.as_deref() {
                Some("l") => {}
                Some("r") => write(&paths.copy, &remote)?,
                _ => {
                    println!("nothing changed; your copy is {}", paths.copy.display());
                    return Ok(());
                }
            }
        }
    }
    // From here on the snapshot is what the stage holds.
    write(&paths.snapshot, &remote)?;
    println!("editing {}", paths.copy.display());

    let editor = editor(
        std::env::var("VISUAL").ok(),
        std::env::var("EDITOR").ok(),
        cfg!(windows),
    );
    if o.watch {
        watch(kind, &name, &paths, &editor, remote).await
    } else {
        edit_once(kind, &name, &paths, &editor, remote).await
    }
}

/// Git-style: wait for the editor, then validate → diff → confirm → push.
async fn edit_once(
    kind: Kind,
    name: &str,
    paths: &Paths,
    editor: &str,
    remote: String,
) -> Result<()> {
    let kept = || println!("not pushed; your copy is {}", paths.copy.display());
    loop {
        let status = spawn_editor(editor, &paths.copy)?
            .wait()
            .context("wait for the editor")?;
        check_status(editor, status)?;
        let text = read(&paths.copy)?;
        if text == remote {
            println!("no changes");
            return Ok(());
        }
        if let Err(e) = validate(kind, &text) {
            println!("{e:#}");
            match ask("[e]dit again or [q]uit? ")?.as_deref() {
                Some("e") => continue,
                _ => {
                    kept();
                    return Ok(());
                }
            }
        }
        print!("{}", render_diff(&remote, &text));
        match ask(&format!("push to {name}? [Y/n/e] "))?.as_deref() {
            Some("" | "y") => {
                push(kind, name, paths, text)
                    .await
                    .with_context(|| format!("your copy is {}", paths.copy.display()))?;
                return Ok(());
            }
            Some("e") => continue,
            _ => {
                kept();
                return Ok(());
            }
        }
    }
}

/// `--watch`: leave the editor running and offer a push on every save.
async fn watch(
    kind: Kind,
    name: &str,
    paths: &Paths,
    editor: &str,
    mut pushed: String,
) -> Result<()> {
    // The editor is not waited for: GUI editors return at once, and its exit
    // says nothing about whether the user is done.
    let _child = spawn_editor(editor, &paths.copy)?;
    println!("watching — save to see the diff and push; q + Enter to stop");

    // Stdin is read on its own thread so prompts and `q` share one source.
    let (tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line.trim().to_ascii_lowercase()).is_err() {
                break;
            }
        }
    });

    let mut seen = read(&paths.copy)?;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            line = lines.recv() => match line.as_deref() {
                None | Some("q") => break,
                _ => {}
            },
            _ = tick.tick() => {
                // A read can fail mid-save; the next tick catches up.
                let Ok(text) = read(&paths.copy) else { continue };
                if text == seen {
                    continue;
                }
                seen = text.clone();
                if text == pushed {
                    println!("(back to what {name} has)");
                    continue;
                }
                if let Err(e) = validate(kind, &text) {
                    println!("{e:#}\n(fix and save again)");
                    continue;
                }
                print!("{}", render_diff(&pushed, &text));
                print!("push to {name}? [Y/n] ");
                std::io::stdout().flush().ok();
                match lines.recv().await.as_deref() {
                    None | Some("q") => break,
                    Some("" | "y") => match push(kind, name, paths, text.clone()).await {
                        Ok(()) => pushed = text,
                        Err(e) => println!("{e:#}"),
                    },
                    _ => println!("not pushed"),
                }
            }
        }
    }
    if seen != pushed {
        println!(
            "unpushed edits kept in {}; `catc {} edit --name {name}` picks them up",
            paths.copy.display(),
            kind.what()
        );
    }
    Ok(())
}

async fn push(kind: Kind, name: &str, paths: &Paths, text: String) -> Result<()> {
    let targets = Targets {
        names: vec![name.to_string()],
        all: false,
    };
    cmd_send(targets, kind.set(text.clone())).await?;
    write(&paths.snapshot, &text)
}

/// Parse and validate a config, printing warnings; logic is not checked
/// here (the stage reports Rhai errors in its reply).
fn validate(kind: Kind, text: &str) -> Result<()> {
    if kind == Kind::Logic {
        return Ok(());
    }
    let cfg = Config::from_yaml(text).context("invalid config")?;
    for w in cfg.validate().context("invalid config")? {
        println!("warning: {}", w.0);
    }
    Ok(())
}

/// `$VISUAL`, then `$EDITOR`, then the platform's always-there editor.
fn editor(visual: Option<String>, editor: Option<String>, windows: bool) -> String {
    [visual, editor]
        .into_iter()
        .flatten()
        .map(|e| e.trim().to_string())
        .find(|e| !e.is_empty())
        .unwrap_or_else(|| if windows { "notepad" } else { "vi" }.to_string())
}

/// Run the editor through the shell, as git does, so `EDITOR="code --wait"`
/// and editors with arguments work.
fn shell_invocation(editor: &str, path: &str, windows: bool) -> (&'static str, Vec<String>) {
    if windows {
        ("cmd", vec!["/C".into(), format!("{editor} \"{path}\"")])
    } else {
        (
            "sh",
            vec![
                "-c".into(),
                format!("{editor} \"$@\""),
                "sh".into(),
                path.into(),
            ],
        )
    }
}

fn spawn_editor(editor: &str, path: &Path) -> Result<Child> {
    let path = path.to_string_lossy();
    let (prog, args) = shell_invocation(editor, &path, cfg!(windows));
    let mut cmd = Command::new(prog);
    #[cfg(windows)]
    {
        // cmd.exe does its own parsing; hand it the command line untouched.
        use std::os::windows::process::CommandExt;
        cmd.arg(&args[0]).raw_arg(&args[1]);
    }
    #[cfg(not(windows))]
    cmd.args(&args);
    cmd.spawn()
        .with_context(|| format!("start editor {editor:?} (set $VISUAL or $EDITOR)"))
}

fn check_status(editor: &str, status: ExitStatus) -> Result<()> {
    if !status.success() {
        bail!("editor {editor:?} exited with {status}; nothing pushed");
    }
    Ok(())
}

/// Answer to a prompt, trimmed and lower-cased; `None` on end of input, so a
/// closed stdin never confirms a push.
fn ask(prompt: &str) -> Result<Option<String>> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        println!();
        return Ok(None);
    }
    Ok(Some(line.trim().to_ascii_lowercase()))
}

/// Editors on Windows may save CRLF; compare and push LF only.
fn normalize(s: &str) -> String {
    s.replace("\r\n", "\n")
}

fn read(path: &Path) -> Result<String> {
    let s = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(normalize(&s))
}

fn read_opt(path: &Path) -> Result<Option<String>> {
    if path.exists() {
        read(path).map(Some)
    } else {
        Ok(None)
    }
}

fn write(path: &Path, text: &str) -> Result<()> {
    fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Del,
    Add,
}

/// Line diff by longest common subsequence. Configs and logic run to a few
/// hundred lines, so the quadratic table is fine.
fn line_diff<'a>(old: &'a str, new: &'a str) -> Vec<(Op, &'a str)> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(n.max(m));
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            out.push((Op::Keep, a[i]));
            i += 1;
            j += 1;
        } else if j < m && (i == n || lcs[i][j + 1] > lcs[i + 1][j]) {
            // Ties go to the deletion, so `-old` prints before `+new`.
            out.push((Op::Add, b[j]));
            j += 1;
        } else {
            out.push((Op::Del, a[i]));
            i += 1;
        }
    }
    out
}

/// Changed lines with two lines of context, `…` between distant hunks;
/// red/green when stdout is a terminal. Empty when nothing changed.
fn render_diff(old: &str, new: &str) -> String {
    render_diff_with(old, new, std::io::stdout().is_terminal())
}

fn render_diff_with(old: &str, new: &str, color: bool) -> String {
    const CONTEXT: usize = 2;
    let ops = line_diff(old, new);
    let changed: Vec<usize> = (0..ops.len()).filter(|&k| ops[k].0 != Op::Keep).collect();
    if changed.is_empty() {
        return String::new();
    }
    let near = |k: usize| {
        changed
            .iter()
            .any(|&c| k + CONTEXT >= c && k <= c + CONTEXT)
    };
    let mut out = String::new();
    let mut skipped = false;
    for (k, (op, line)) in ops.iter().enumerate() {
        if !near(k) {
            skipped = true;
            continue;
        }
        if skipped || (k > 0 && out.is_empty()) {
            out.push_str("  …\n");
        }
        skipped = false;
        let (mark, on, off) = match op {
            Op::Keep => (' ', "", ""),
            Op::Del => ('-', "\x1b[31m", "\x1b[0m"),
            Op::Add => ('+', "\x1b[32m", "\x1b[0m"),
        };
        if color && !on.is_empty() {
            out.push_str(&format!("{on}{mark} {line}{off}\n"));
        } else {
            out.push_str(&format!("{mark} {line}\n"));
        }
    }
    if skipped {
        out.push_str("  …\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_covers_every_case() {
        assert_eq!(reconcile(None, None, "r"), Start::Remote);
        assert_eq!(reconcile(Some("r"), Some("old"), "r"), Start::Remote);
        assert_eq!(reconcile(Some("b"), Some("b"), "r"), Start::Refreshed);
        assert_eq!(reconcile(Some("mine"), Some("r"), "r"), Start::ResumeLocal);
        assert_eq!(reconcile(Some("mine"), Some("b"), "r"), Start::Conflict);
        // A copy without a snapshot can't be told apart: ask.
        assert_eq!(reconcile(Some("mine"), None, "r"), Start::Conflict);
    }

    #[test]
    fn editor_prefers_visual_then_editor_then_platform() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(editor(s("code --wait"), s("nano"), false), "code --wait");
        assert_eq!(editor(None, s("nano"), false), "nano");
        assert_eq!(editor(s("  "), s("nano"), false), "nano");
        assert_eq!(editor(None, None, false), "vi");
        assert_eq!(editor(None, None, true), "notepad");
    }

    #[test]
    fn editor_runs_through_the_shell() {
        let (prog, args) = shell_invocation("code --wait", "/a b/config.yaml", false);
        assert_eq!(prog, "sh");
        assert_eq!(args, ["-c", "code --wait \"$@\"", "sh", "/a b/config.yaml"]);
        let (prog, args) = shell_invocation("notepad", r"C:\a b\config.yaml", true);
        assert_eq!(prog, "cmd");
        assert_eq!(args, ["/C", r#"notepad "C:\a b\config.yaml""#]);
    }

    #[test]
    fn stage_names_become_one_safe_path_component() {
        assert_eq!(cfg::safe_name("kitchen-2.tv"), "kitchen-2.tv");
        assert_eq!(cfg::safe_name("a/b\\c d"), "a_b_c_d");
        assert_eq!(cfg::safe_name(".."), "_..");
        assert_eq!(cfg::safe_name(""), "_");
    }

    #[test]
    fn diff_shows_changes_with_context_only() {
        assert_eq!(render_diff_with("a\nb\n", "a\nb\n", false), "");
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n";
        let new = "1\n2\n3\n4\nfive\n6\n7\n8\n";
        assert_eq!(
            render_diff_with(old, new, false),
            "  …\n  3\n  4\n- 5\n+ five\n  6\n  7\n  …\n"
        );
        assert_eq!(render_diff_with("", "x\n", false), "+ x\n");
    }

    #[test]
    fn config_validation_catches_a_missing_version() {
        assert!(validate(Kind::Config, "rotation: {}\n").is_err());
        assert!(validate(Kind::Logic, "anything goes").is_ok());
    }

    #[test]
    fn crlf_is_normalized() {
        assert_eq!(normalize("a\r\nb\r\n"), "a\nb\n");
    }
}

//! `catc completion`: TAB completion for bash, zsh, fish and PowerShell.
//!
//! Completion is dynamic: the hook a shell sources at startup calls back
//! into catc on every TAB (`COMPLETE=<shell> catc -- <words>`, handled by
//! `clap_complete::CompleteEnv` in `main`). So the completions always match
//! the catc on PATH, even after `catc self-update`, and stage and alias
//! names come from the same `catc.toml` the command itself would use.
//!
//! `install` writes a small marker-delimited block into the shell's rc
//! file (fish: its own completions file), `uninstall` removes exactly that
//! block, and the bare command reports which shells have it.

use crate::cfg::{Aliases, CatcConfig};
use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use clap_complete::CompletionCandidate;
use std::env;
use std::fmt;
use std::fs;
use std::io::{ErrorKind, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BEGIN: &str = "# >>> catc completion >>>";
const NOTE: &str =
    "# Managed by `catc completion install`; remove with `catc completion uninstall`.";
const END: &str = "# <<< catc completion <<<";

// ---------------------------------------------------------------------------
// Candidates for the dynamic completer

/// Stage names from `catc.toml`, with their role as help. Empty on any
/// error: a TAB must never print one.
pub fn stage_names() -> Vec<CompletionCandidate> {
    let Ok(cfg) = CatcConfig::load() else {
        return Vec::new();
    };
    cfg.stages
        .iter()
        .map(|s| {
            let role = match (s.mirror, s.active) {
                (true, _) => "mirror",
                (false, true) => "active",
                (false, false) => "inactive",
            };
            CompletionCandidate::new(&s.name).help(Some(role.into()))
        })
        .collect()
}

/// Alias names from `aliases.yaml`, with the stored command line as help.
pub fn alias_names() -> Vec<CompletionCandidate> {
    let Ok(aliases) = Aliases::load() else {
        return Vec::new();
    };
    aliases
        .0
        .into_iter()
        .map(|(name, cmd)| CompletionCandidate::new(name).help(Some(cmd.into())))
        .collect()
}

// ---------------------------------------------------------------------------
// Shells

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    /// PowerShell 7+ (`pwsh`).
    Pwsh,
    /// Windows PowerShell 5.1 (`powershell.exe`).
    #[value(name = "powershell")]
    Powershell,
}

impl fmt::Display for Shell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.exe())
    }
}

const ALL: [Shell; 5] = [
    Shell::Bash,
    Shell::Zsh,
    Shell::Fish,
    Shell::Pwsh,
    Shell::Powershell,
];

impl Shell {
    fn exe(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
            Shell::Pwsh => "pwsh",
            Shell::Powershell => "powershell",
        }
    }

    fn is_powershell(self) -> bool {
        matches!(self, Shell::Pwsh | Shell::Powershell)
    }

    /// Listed in the status even when the shell isn't on PATH. Others show
    /// up only when found or already set up.
    fn always_listed(self, windows: bool) -> bool {
        match self {
            Shell::Bash | Shell::Zsh | Shell::Fish => !windows,
            Shell::Pwsh | Shell::Powershell => windows,
        }
    }

    /// The hook, ASCII only (Windows PowerShell reads a BOM-less profile in
    /// the ANSI code page). Guarded, so a shell without catc on PATH starts
    /// cleanly, and with stderr dropped, so a catc too old to know
    /// `COMPLETE` (it prints a usage error and no hook) starts cleanly too.
    pub fn snippet(self) -> &'static str {
        match self {
            // `eval "$(…)"`, not `source <(…)`: bash 3.2 (macOS) silently
            // sources nothing from a process substitution.
            Shell::Bash => {
                r#"command -v catc >/dev/null 2>&1 && eval "$(COMPLETE=bash catc 2>/dev/null)""#
            }
            Shell::Zsh => concat!(
                "if (( $+commands[catc] )); then\n",
                "  (( $+functions[compdef] )) || { autoload -Uz compinit && compinit; }\n",
                "  eval \"$(COMPLETE=zsh catc 2>/dev/null)\"\n",
                "fi",
            ),
            Shell::Fish => "command -q catc; and COMPLETE=fish catc 2>/dev/null | source",
            Shell::Pwsh | Shell::Powershell => concat!(
                "if (Get-Command catc -ErrorAction SilentlyContinue) {\n",
                "  $env:COMPLETE = \"powershell\"; catc 2>$null | Out-String | Invoke-Expression; ",
                "Remove-Item Env:\\COMPLETE\n",
                "}",
            ),
        }
    }

    /// The file the hook goes into. PowerShell asks the shell itself, since
    /// `$PROFILE` moves with OneDrive folder redirection.
    fn target(self, env: &dyn Fn(&str) -> Option<String>) -> Result<Target> {
        let home = || home_dir(env).context("can't find your home directory");
        let path = match self {
            Shell::Bash if cfg!(target_os = "macos") => home()?.join(".bash_profile"),
            Shell::Bash => home()?.join(".bashrc"),
            Shell::Zsh => match env("ZDOTDIR") {
                Some(d) if !d.is_empty() => PathBuf::from(d),
                _ => home()?,
            }
            .join(".zshrc"),
            Shell::Fish => match env("XDG_CONFIG_HOME") {
                Some(d) if !d.is_empty() => PathBuf::from(d),
                _ => home()?.join(".config"),
            }
            .join("fish")
            .join("completions")
            .join("catc.fish"),
            Shell::Pwsh | Shell::Powershell => return ask_powershell(self.exe()),
        };
        Ok(Target { path, policy: None })
    }

    /// How to pick the hook up in the current session after install.
    fn reload_hint(self, path: &str) -> String {
        match self {
            Shell::Bash | Shell::Zsh => format!("open a new terminal, or run:  source {path}"),
            Shell::Fish => "fish picks it up on the next TAB".into(),
            Shell::Pwsh | Shell::Powershell => "open a new window, or run:  . $PROFILE".into(),
        }
    }

    /// How to drop completion from the current session after uninstall.
    fn unload_hint(self) -> &'static str {
        match self {
            Shell::Bash => "running shells keep it until closed, or run:  complete -r catc",
            Shell::Zsh => "running shells keep it until closed, or run:  compdef -d catc",
            Shell::Fish => "running shells keep it until closed, or run:  complete -e catc",
            Shell::Pwsh | Shell::Powershell => "running windows keep it until closed",
        }
    }
}

struct Target {
    path: PathBuf,
    /// PowerShell's effective execution policy, when asked.
    policy: Option<String>,
}

impl Target {
    /// `Restricted` and `AllSigned` both keep an unsigned `$PROFILE` from
    /// running, which silently disables the hook.
    fn blocked_policy(&self) -> Option<&str> {
        self.policy
            .as_deref()
            .filter(|p| p.eq_ignore_ascii_case("Restricted") || p.eq_ignore_ascii_case("AllSigned"))
    }
}

fn ask_powershell(exe: &str) -> Result<Target> {
    if !on_path(exe) {
        bail!("{exe} isn't on PATH, so catc can't ask it where $PROFILE is");
    }
    let out = Command::new(exe)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$PROFILE; (Get-ExecutionPolicy).ToString()",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("run {exe} to find $PROFILE"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    match (out.status.success(), lines.next()) {
        (true, Some(profile)) => Ok(Target {
            path: PathBuf::from(profile),
            policy: lines.next().map(str::to_owned),
        }),
        _ => bail!("{exe} didn't say where $PROFILE is"),
    }
}

fn home_dir(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    // $HOME first: Git Bash on Windows may point it away from %USERPROFILE%.
    match env("HOME") {
        Some(h) if !h.is_empty() => Some(PathBuf::from(h)),
        _ => directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()),
    }
}

fn real_env(key: &str) -> Option<String> {
    env::var(key).ok()
}

/// The shell catc was most likely started from.
fn detect_shell(env: &dyn Fn(&str) -> Option<String>, windows: bool) -> Option<Shell> {
    let from_name = |path: &str| {
        let name = Path::new(path).file_stem()?.to_str()?.to_ascii_lowercase();
        ALL.into_iter().find(|s| s.exe() == name)
    };
    if !windows {
        return env("SHELL").as_deref().and_then(from_name);
    }
    // Git Bash / MSYS2 export these; native shells don't.
    if env("MSYSTEM").is_some() {
        return Some(Shell::Bash);
    }
    if let Some(s) = env("SHELL").as_deref().and_then(from_name) {
        return Some(s);
    }
    // PSModulePath is set machine-wide (cmd.exe sees it too), but each
    // PowerShell prepends its own per-version folders.
    let ps = env("PSModulePath")?.to_ascii_lowercase();
    if ps.split(';').any(|p| {
        p.trim_end_matches('\\')
            .ends_with("\\powershell\\7\\modules")
    }) {
        Some(Shell::Pwsh)
    } else if ps.contains("\\documents\\windowspowershell\\modules") {
        Some(Shell::Powershell)
    } else {
        None
    }
}

/// Whether `name` resolves to an executable on PATH (with PATHEXT on
/// Windows).
fn on_path(name: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    let exts: Vec<String> = if cfg!(windows) {
        env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        Vec::new()
    };
    env::split_paths(&path).any(|dir| {
        let base = dir.join(name);
        if cfg!(windows) {
            base.is_file()
                || exts
                    .iter()
                    .any(|e| dir.join(format!("{name}{e}")).is_file())
        } else {
            is_executable(&base)
        }
    })
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

// ---------------------------------------------------------------------------
// The managed block

/// Byte span of the managed block, from the start of the BEGIN line to the
/// end of the END line (its newline included).
fn block_span(text: &str) -> Result<Option<(usize, usize)>> {
    let mut pos = 0;
    let mut start = None;
    for line in text.split_inclusive('\n') {
        let bare = line.trim();
        match start {
            None if bare == BEGIN => start = Some(pos),
            Some(s) if bare == END => return Ok(Some((s, pos + line.len()))),
            _ => {}
        }
        pos += line.len();
    }
    if start.is_some() {
        bail!("found `{BEGIN}` but no `{END}` after it; fix the file by hand");
    }
    Ok(None)
}

fn render_block(snippet: &str, eol: &str) -> String {
    let mut out = String::new();
    for line in [BEGIN, NOTE]
        .into_iter()
        .chain(snippet.lines())
        .chain([END])
    {
        out.push_str(line);
        out.push_str(eol);
    }
    out
}

/// `text` with the block for `snippet` in it: replaced in place when
/// present, appended otherwise. Nothing outside the markers changes.
fn upsert_block(text: &str, snippet: &str, eol: &str) -> Result<String> {
    let block = render_block(snippet, eol);
    Ok(match block_span(text)? {
        Some((s, e)) => format!("{}{block}{}", &text[..s], &text[e..]),
        None if text.is_empty() || text.ends_with('\n') => format!("{text}{block}"),
        None => format!("{text}{eol}{block}"),
    })
}

/// `text` without the block, or `None` when there is none.
fn remove_block(text: &str) -> Result<Option<String>> {
    Ok(block_span(text)?.map(|(s, e)| format!("{}{}", &text[..s], &text[e..])))
}

/// A live (uncommented) line outside the block that hooks catc in by hand,
/// e.g. `eval "$(COMPLETE=bash catc)"` from the clap docs.
fn hand_made(text: &str) -> bool {
    let outside = match block_span(text) {
        Ok(Some((s, e))) => format!("{}{}", &text[..s], &text[e..]),
        _ => text.to_owned(),
    };
    outside.lines().any(|l| {
        let l = l.trim();
        !l.starts_with('#') && l.contains("COMPLETE") && l.contains("catc")
    })
}

fn eol_for(text: &str, shell: Shell) -> &'static str {
    if text.contains("\r\n") || (text.is_empty() && cfg!(windows) && shell.is_powershell()) {
        "\r\n"
    } else {
        "\n"
    }
}

/// The file's text; empty when it doesn't exist yet.
fn read_text(path: &Path, shell: Shell) -> Result<String> {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).map_err(|_| {
            anyhow::anyhow!(
                "{} isn't UTF-8 (Windows PowerShell saves UTF-16), so catc won't edit it.\n\
                 Add this to it by hand:\n\n{}",
                path.display(),
                shell.snippet()
            )
        }),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

// ---------------------------------------------------------------------------
// Commands

fn resolve(shell: Option<Shell>) -> Result<Shell> {
    if let Some(s) = shell {
        return Ok(s);
    }
    detect_shell(&real_env, cfg!(windows)).ok_or_else(|| {
        anyhow::anyhow!(
            "can't tell which shell you're in; name it: {}",
            ALL.map(|s| s.exe()).join(" | ")
        )
    })
}

pub fn cmd_install(shell: Option<Shell>) -> Result<()> {
    let shell = resolve(shell)?;
    let target = shell.target(&real_env)?;
    let path = &target.path;
    let shown = tilde(path);
    let text = read_text(path, shell)?;
    let had_block = block_span(&text)?.is_some();
    if !had_block && hand_made(&text) {
        println!("✔ {shell}: {shown} already hooks catc in by hand; leaving it alone");
        return Ok(());
    }
    let new = upsert_block(&text, shell.snippet(), eol_for(&text, shell))?;
    if new == text {
        println!("✔ {shell} completion is already installed in {shown}");
    } else {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        // In place, so a symlinked dotfile stays a symlink.
        fs::write(path, &new).with_context(|| format!("write {}", path.display()))?;
        let verb = if had_block { "updated" } else { "installed" };
        println!("✔ {shell} completion {verb} in {shown}");
        println!("  {}", shell.reload_hint(&shown));
    }
    if !on_path("catc") {
        println!("⚠ catc isn't on your PATH yet; completion turns on once it is");
    }
    if let Some(p) = target.blocked_policy() {
        println!("⚠ execution policy {p} keeps $PROFILE from running; to allow it:");
        println!("    Set-ExecutionPolicy -Scope CurrentUser RemoteSigned");
    }
    Ok(())
}

pub fn cmd_uninstall(shell: Option<Shell>) -> Result<()> {
    let shell = resolve(shell)?;
    let target = shell.target(&real_env)?;
    let path = &target.path;
    let shown = tilde(path);
    let text = read_text(path, shell)?;
    match remove_block(&text)? {
        None if hand_made(&text) => println!(
            "○ {shell}: {shown} hooks catc in by hand, outside catc's block; remove that line yourself"
        ),
        None => println!("○ {shell} completion isn't installed; nothing to do"),
        Some(new) => {
            if shell == Shell::Fish && new.trim().is_empty() {
                fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
            } else {
                fs::write(path, &new).with_context(|| format!("write {}", path.display()))?;
            }
            println!("✔ {shell} completion removed from {shown}");
            println!("  {}", shell.unload_hint());
        }
    }
    Ok(())
}

pub fn cmd_print(shell: Shell) -> Result<()> {
    println!("{}", shell.snippet());
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Installed,
    HandMade,
    NotInstalled,
    NotFound,
    Failed(String),
}

#[derive(Debug)]
struct Row {
    shell: Shell,
    found: bool,
    state: State,
    file: Option<String>,
    warning: Option<String>,
}

fn probe(shell: Shell) -> Row {
    let found = on_path(shell.exe());
    let mut row = Row {
        shell,
        found,
        state: State::NotFound,
        file: None,
        warning: None,
    };
    if shell.is_powershell() && !found {
        return row; // nobody to ask where $PROFILE is
    }
    let target = match shell.target(&real_env) {
        Ok(t) => t,
        Err(e) => {
            row.state = State::Failed(format!("{e:#}"));
            return row;
        }
    };
    row.file = Some(tilde(&target.path));
    let text = fs::read_to_string(&target.path).unwrap_or_default();
    row.state = match block_span(&text) {
        Ok(Some(_)) => State::Installed,
        Ok(None) if hand_made(&text) => State::HandMade,
        Ok(None) if found => State::NotInstalled,
        Ok(None) => State::NotFound,
        Err(e) => State::Failed(format!("{e:#}")),
    };
    if matches!(row.state, State::Installed | State::HandMade) {
        if let Some(p) = target.blocked_policy() {
            row.warning = Some(format!(
                "execution policy {p} keeps $PROFILE from running; \
                 fix: Set-ExecutionPolicy -Scope CurrentUser RemoteSigned"
            ));
        }
    }
    row
}

pub fn cmd_status() -> Result<()> {
    let windows = cfg!(windows);
    let current = detect_shell(&real_env, windows);
    // PowerShell takes a moment to start; ask every shell at once.
    let rows: Vec<Row> = std::thread::scope(|s| {
        let handles: Vec<_> = ALL.map(|sh| s.spawn(move || probe(sh))).into();
        handles
            .into_iter()
            .map(|h| h.join().expect("completion probe panicked"))
            .collect()
    });
    let rows: Vec<Row> = rows
        .into_iter()
        .filter(|r| {
            r.shell.always_listed(windows)
                || r.found
                || Some(r.shell) == current
                || matches!(r.state, State::Installed | State::HandMade)
        })
        .collect();
    let color = std::io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none();
    print!("{}", render_status(current, &rows, color));
    Ok(())
}

fn render_status(current: Option<Shell>, rows: &[Row], color: bool) -> String {
    let paint = |code: &str, s: &str| {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    };
    let mut out = String::new();
    let you = match current {
        Some(s) => format!(" · you're in {s}"),
        None => String::new(),
    };
    out.push_str(&format!("🐚 Tab completion for catc{you}\n\n"));

    let label = |r: &Row| -> (&'static str, &'static str, String) {
        // (symbol, ANSI color, state text)
        match &r.state {
            State::Installed if r.warning.is_some() => ("⚠", "33", "installed".into()),
            State::Installed => ("✔", "32", "installed".into()),
            State::HandMade => ("✔", "32", "set up by hand".into()),
            State::NotInstalled => ("○", "0", "not installed".into()),
            State::NotFound => ("·", "2", "not found".into()),
            State::Failed(e) => ("⚠", "33", format!("can't tell: {e}")),
        }
    };
    let name_w = rows.iter().map(|r| r.shell.exe().len()).max().unwrap_or(0);
    let state_w = rows
        .iter()
        .filter(|r| r.file.is_some())
        .map(|r| label(r).2.chars().count())
        .max()
        .unwrap_or(0);
    for r in rows {
        let (sym, code, state) = label(r);
        let marker = if Some(r.shell) == current { "◂" } else { " " };
        let name = format!("{:name_w$} {marker}", r.shell.exe());
        let line = match (&r.file, &r.state) {
            (Some(f), State::NotFound) => format!("{name}  {state:state_w$}  ({f})"),
            (Some(f), _) => format!("{name}  {state:state_w$}  {f}"),
            (None, _) => format!("{name}  {state}"),
        };
        out.push_str(&format!("  {} {}\n", paint(code, sym), paint(code, &line)));
        if let Some(w) = &r.warning {
            out.push_str(&format!("      {}\n", paint("33", &format!("↳ {w}"))));
        }
    }

    // What to run next: the current shell first, then the rest in order.
    let mut order: Vec<&Row> = rows.iter().filter(|r| Some(r.shell) == current).collect();
    order.extend(rows.iter().filter(|r| Some(r.shell) != current));
    let mut next: Vec<(String, String)> = Vec::new();
    for r in order {
        let here = Some(r.shell) == current;
        match r.state {
            State::NotInstalled => next.push((
                format!("catc completion install {}", r.shell),
                if here {
                    "turn it on here".into()
                } else {
                    format!("turn it on for {}", r.shell)
                },
            )),
            State::Installed => next.push((
                format!("catc completion uninstall {}", r.shell),
                format!("turn it off for {}", r.shell),
            )),
            _ => {}
        }
    }
    if !next.is_empty() {
        let w = next.iter().map(|(c, _)| c.len()).max().unwrap_or(0);
        out.push_str(&format!("\n  {}\n", paint("1", "Next")));
        for (cmd, why) in &next {
            out.push_str(&format!(
                "  ▸ {}  {}\n",
                paint("36", &format!("{cmd:w$}")),
                paint("2", why)
            ));
        }
    }

    out.push('\n');
    out.push_str("  Completes subcommands, flags, stage names and alias names.\n");
    out.push_str(&paint(
        "2",
        "  sh, dash and cmd.exe have no programmable completion; use one of the shells above.\n",
    ));
    out
}

/// `path` with the home directory shown as `~`.
fn tilde(path: &Path) -> String {
    if let Some(home) = home_dir(&real_env) {
        if let Ok(rest) = path.strip_prefix(&home) {
            if rest.as_os_str().is_empty() {
                return "~".into();
            }
            let sep = std::path::MAIN_SEPARATOR;
            return format!("~{sep}{}", rest.display());
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SNIP: &str = "eval \"$(COMPLETE=bash catc)\"";

    #[test]
    fn upsert_into_empty_file() {
        let out = upsert_block("", SNIP, "\n").unwrap();
        assert_eq!(out, format!("{BEGIN}\n{NOTE}\n{SNIP}\n{END}\n"));
    }

    #[test]
    fn upsert_adds_missing_newline_and_round_trips() {
        let orig = "alias ll='ls -l'";
        let out = upsert_block(orig, SNIP, "\n").unwrap();
        assert!(out.starts_with("alias ll='ls -l'\n# >>>"));
        assert_eq!(remove_block(&out).unwrap().unwrap(), "alias ll='ls -l'\n");
    }

    #[test]
    fn upsert_then_remove_is_byte_identical() {
        let orig = "export A=1\n\n# end\n";
        let out = upsert_block(orig, SNIP, "\n").unwrap();
        assert_eq!(remove_block(&out).unwrap().unwrap(), orig);
    }

    #[test]
    fn upsert_is_idempotent_and_replaces_in_place() {
        let orig = "a\n";
        let once = upsert_block(orig, "old line", "\n").unwrap();
        let with_tail = format!("{once}b\n");
        let twice = upsert_block(&with_tail, SNIP, "\n").unwrap();
        assert_eq!(twice, format!("a\n{BEGIN}\n{NOTE}\n{SNIP}\n{END}\nb\n"));
        assert_eq!(upsert_block(&twice, SNIP, "\n").unwrap(), twice);
    }

    #[test]
    fn crlf_is_kept() {
        let orig = "Set-Alias g git\r\n";
        let out = upsert_block(orig, "x\ny", eol_for(orig, Shell::Pwsh)).unwrap();
        assert_eq!(
            out,
            format!("Set-Alias g git\r\n{BEGIN}\r\n{NOTE}\r\nx\r\ny\r\n{END}\r\n")
        );
        assert_eq!(remove_block(&out).unwrap().unwrap(), orig);
    }

    #[test]
    fn remove_when_absent_is_none() {
        assert_eq!(remove_block("a\nb\n").unwrap(), None);
    }

    #[test]
    fn unterminated_block_is_an_error() {
        let text = format!("a\n{BEGIN}\nx\n");
        assert!(remove_block(&text).is_err());
        assert!(upsert_block(&text, SNIP, "\n").is_err());
    }

    #[test]
    fn hand_made_lines_are_spotted_outside_the_block_only() {
        assert!(hand_made("source <(COMPLETE=bash catc)\n"));
        assert!(hand_made(
            "$env:COMPLETE = \"powershell\"; catc | Out-String | iex\n"
        ));
        assert!(!hand_made("# eval \"$(COMPLETE=bash catc)\"\n"));
        assert!(!hand_made(&upsert_block("", SNIP, "\n").unwrap()));
        assert!(!hand_made("export COMPLETE=1\n"));
    }

    #[test]
    fn snippets_are_ascii() {
        for s in ALL {
            assert!(s.snippet().is_ascii(), "{s}");
        }
    }

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn detect_unix_from_shell_var() {
        assert_eq!(
            detect_shell(&env_of(&[("SHELL", "/bin/zsh")]), false),
            Some(Shell::Zsh)
        );
        assert_eq!(
            detect_shell(&env_of(&[("SHELL", "/opt/homebrew/bin/fish")]), false),
            Some(Shell::Fish)
        );
        assert_eq!(
            detect_shell(&env_of(&[("SHELL", "/bin/dash")]), false),
            None
        );
        assert_eq!(detect_shell(&env_of(&[]), false), None);
    }

    #[test]
    fn detect_windows_shells() {
        let user5 = r"C:\Users\me\OneDrive - Corp\Documents\WindowsPowerShell\Modules";
        let sys = r"C:\Program Files\WindowsPowerShell\Modules;C:\WINDOWS\system32\WindowsPowerShell\v1.0\Modules";
        let ps5 = format!("{user5};{sys}");
        let ps7 = format!(
            r"C:\Users\me\Documents\PowerShell\Modules;C:\Program Files\PowerShell\Modules;c:\program files\powershell\7\Modules;{sys}"
        );
        assert_eq!(
            detect_shell(
                &env_of(&[("MSYSTEM", "MINGW64"), ("PSModulePath", sys)]),
                true
            ),
            Some(Shell::Bash)
        );
        assert_eq!(
            detect_shell(&env_of(&[("PSModulePath", &ps7)]), true),
            Some(Shell::Pwsh)
        );
        assert_eq!(
            detect_shell(&env_of(&[("PSModulePath", &ps5)]), true),
            Some(Shell::Powershell)
        );
        // cmd.exe: only the machine-wide entries.
        assert_eq!(detect_shell(&env_of(&[("PSModulePath", sys)]), true), None);
    }

    #[test]
    fn status_lists_state_and_next_steps() {
        let rows = vec![
            Row {
                shell: Shell::Bash,
                found: true,
                state: State::Installed,
                file: Some("~/.bashrc".into()),
                warning: None,
            },
            Row {
                shell: Shell::Zsh,
                found: true,
                state: State::NotInstalled,
                file: Some("~/.zshrc".into()),
                warning: None,
            },
            Row {
                shell: Shell::Fish,
                found: false,
                state: State::NotFound,
                file: Some("~/.config/fish/completions/catc.fish".into()),
                warning: None,
            },
        ];
        let out = render_status(Some(Shell::Bash), &rows, false);
        let expected = "\
🐚 Tab completion for catc · you're in bash

  ✔ bash ◂  installed      ~/.bashrc
  ○ zsh     not installed  ~/.zshrc
  · fish    not found      (~/.config/fish/completions/catc.fish)

  Next
  ▸ catc completion uninstall bash  turn it off for bash
  ▸ catc completion install zsh     turn it on for zsh

  Completes subcommands, flags, stage names and alias names.
  sh, dash and cmd.exe have no programmable completion; use one of the shells above.
";
        assert_eq!(out, expected);
    }
}

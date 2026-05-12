//! Autostart installer.
//!
//! Windows  — `.lnk` in `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup`.
//! Linux    — `~/.config/autostart/catstage.desktop` (XDG; honoured by GNOME,
//!            KDE, XFCE, LXQt and most other session managers).
//! macOS    — currently a no-op (LaunchAgent support not implemented).
//!
//! All paths target the *per-user* autostart location; no admin / sudo
//! required. The shortcut/file points at the running `catstage` binary with
//! the same `--socks` / `--name` arguments the running process was launched
//! with — `catc autostart install` populates those from the live `TauriCtx`.

use std::path::PathBuf;

use anyhow::Result;

#[derive(Debug, Clone)]
// macOS install() is a no-op and reads neither field, so without this
// dead_code fires on darwin only.
#[cfg_attr(
    all(not(target_os = "windows"), not(target_os = "linux")),
    allow(dead_code)
)]
pub struct AutostartArgs {
    /// `--socks <wss://...>` that the autostarted process should use.
    pub socks: String,
    /// Optional `--name <stage-name>` override.
    pub name: Option<String>,
}

/// Install (or replace) the Startup-folder shortcut. Returns the path to the
/// shortcut on success, or `Ok(None)` on platforms where autostart is a no-op
/// in this build.
#[cfg(target_os = "windows")]
pub fn install(args: &AutostartArgs) -> Result<Option<PathBuf>> {
    use mslnk::ShellLink;

    let startup = startup_dir()?;
    std::fs::create_dir_all(&startup)?;
    let target = std::env::current_exe()?;
    let lnk_path = startup.join("catstage.lnk");

    let mut args_str = format!("--socks {}", shell_quote(&args.socks));
    if let Some(name) = &args.name {
        args_str.push_str(&format!(" --name {}", shell_quote(name)));
    }

    let mut sl = ShellLink::new(target)?;
    sl.set_arguments(Some(args_str));
    sl.create_lnk(&lnk_path)?;
    Ok(Some(lnk_path))
}

#[cfg(target_os = "linux")]
pub fn install(args: &AutostartArgs) -> Result<Option<PathBuf>> {
    use anyhow::Context;
    let dir = linux_autostart_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let exe = std::env::current_exe().context("locating current executable")?;
    let mut exec = format!(
        "{} --socks {}",
        exe.display(),
        shell_quote_linux(&args.socks)
    );
    if let Some(name) = &args.name {
        exec.push_str(&format!(" --name {}", shell_quote_linux(name)));
    }
    let path = dir.join("catstage.desktop");
    let body = format!(
        "[Desktop Entry]\nType=Application\nName=CatCast\nComment=CatCast digital signage viewer\nExec={exec}\nX-GNOME-Autostart-enabled=true\nTerminal=false\n"
    );
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(Some(path))
}

#[cfg(all(not(target_os = "windows"), not(target_os = "linux")))]
pub fn install(_args: &AutostartArgs) -> Result<Option<PathBuf>> {
    // macOS / other Unix: not implemented for v1. The catcast://about UI
    // surfaces this to the operator.
    Ok(None)
}

/// If a Startup-folder shortcut already exists, return its absolute path.
#[cfg(target_os = "windows")]
pub fn is_installed() -> Option<PathBuf> {
    let p = startup_dir().ok()?.join("catstage.lnk");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
pub fn is_installed() -> Option<PathBuf> {
    let p = linux_autostart_dir().ok()?.join("catstage.desktop");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

#[cfg(all(not(target_os = "windows"), not(target_os = "linux")))]
pub fn is_installed() -> Option<PathBuf> {
    None
}

/// Remove the Startup-folder shortcut if present. Returns `Ok(true)` if a
/// shortcut was removed, `Ok(false)` if there was nothing to remove.
#[cfg(target_os = "windows")]
pub fn uninstall() -> Result<bool> {
    let p = startup_dir()?.join("catstage.lnk");
    if p.exists() {
        std::fs::remove_file(&p)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(target_os = "linux")]
pub fn uninstall() -> Result<bool> {
    let p = linux_autostart_dir()?.join("catstage.desktop");
    if p.exists() {
        std::fs::remove_file(&p)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(all(not(target_os = "windows"), not(target_os = "linux")))]
pub fn uninstall() -> Result<bool> {
    Ok(false)
}

#[cfg(target_os = "windows")]
fn startup_dir() -> Result<PathBuf> {
    let appdata = std::env::var("APPDATA")?;
    Ok(PathBuf::from(appdata).join("Microsoft\\Windows\\Start Menu\\Programs\\Startup"))
}

#[cfg(target_os = "windows")]
fn shell_quote(s: &str) -> String {
    // mslnk arguments are stored as a single string; wrap anything containing
    // whitespace or quotes in double-quotes and escape embedded quotes.
    if s.contains(' ') || s.contains('"') {
        let escaped = s.replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

#[cfg(target_os = "linux")]
fn linux_autostart_dir() -> Result<PathBuf> {
    use anyhow::Context;
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("neither XDG_CONFIG_HOME nor HOME is set")?;
    Ok(base.join("autostart"))
}

#[cfg(target_os = "linux")]
fn shell_quote_linux(s: &str) -> String {
    // .desktop Exec lines use POSIX-like quoting. Wrap in single quotes and
    // escape any embedded single quotes via '\''.
    if s.is_empty()
        || s.chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '$' | '`' | '*' | '?'))
    {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_struct_is_constructable() {
        let a = AutostartArgs {
            socks: "wss://x/r/y".into(),
            name: Some("kitchen".into()),
        };
        assert!(a.socks.starts_with("wss://"));
        assert_eq!(a.name.as_deref(), Some("kitchen"));
    }

    // Linux install/uninstall round-trip under a sandboxed XDG_CONFIG_HOME so
    // we don't touch the developer's real autostart dir.
    #[test]
    #[cfg(target_os = "linux")]
    fn linux_install_uninstall_round_trip() {
        use std::sync::Mutex;
        // Env vars are process-global; keep this test serial against itself
        // if more env-touching tests land.
        static LOCK: Mutex<()> = Mutex::new(());
        let _g = LOCK.lock().unwrap();

        let tmp = std::env::temp_dir().join(format!("catstage-autostart-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: pre-call snapshot above; restored after the test.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &tmp);
        }

        assert!(is_installed().is_none(), "fresh dir should report none");
        let path = install(&AutostartArgs {
            socks: "wss://example/r/test".into(),
            name: Some("kitchen".into()),
        })
        .unwrap()
        .expect("Linux install should return Some(path)");
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("[Desktop Entry]"));
        // No shell metachars in either value, so no surrounding quotes.
        assert!(body.contains("--socks wss://example/r/test"));
        assert!(body.contains("--name kitchen"));
        assert_eq!(is_installed().as_deref(), Some(path.as_path()));

        assert!(uninstall().unwrap());
        assert!(!path.exists());
        assert!(is_installed().is_none());

        // SAFETY: restore prior env state.
        unsafe {
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    #[cfg(all(not(target_os = "windows"), not(target_os = "linux")))]
    fn install_is_noop_on_unsupported_os() {
        let r = install(&AutostartArgs {
            socks: "wss://x".into(),
            name: None,
        })
        .unwrap();
        assert!(r.is_none());
    }
}

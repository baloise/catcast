//! Autostart installer.
//!
//! On Windows we drop a `.lnk` into the per-user Startup folder. The shortcut
//! points at the running `catstage.exe` with the same `--socks` / `--name`
//! arguments the wizard collected. No admin rights required.
//!
//! On non-Windows targets the function is a no-op that returns a friendly
//! message — the about:catcast UI surfaces it to the operator.

use std::path::PathBuf;

use anyhow::Result;

#[derive(Debug, Clone)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
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

#[cfg(not(target_os = "windows"))]
pub fn install(_args: &AutostartArgs) -> Result<Option<PathBuf>> {
    // Linux / macOS: deliberately a no-op for v1. The about:catcast UI shows
    // the operator a copy-pasteable autostart hint instead.
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

#[cfg(not(target_os = "windows"))]
pub fn is_installed() -> Option<PathBuf> {
    None
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

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn install_is_noop_on_non_windows() {
        let r = install(&AutostartArgs {
            socks: "wss://x".into(),
            name: None,
        })
        .unwrap();
        assert!(r.is_none());
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn is_installed_is_none_on_non_windows() {
        assert!(is_installed().is_none());
    }
}

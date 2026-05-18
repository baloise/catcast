//! Autostart installer.
//!
//! Windows  — `.lnk` in `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup`.
//! Linux    — `~/.config/autostart/catstage.desktop` (XDG; honoured by GNOME,
//!            KDE, XFCE, LXQt and most other session managers).
//! macOS    — `~/Library/LaunchAgents/ch.helvetia.catcast.catstage.plist`
//!            (per-user LaunchAgent; launchd loads it at next login).
//!
//! All paths target the *per-user* autostart location; no admin / sudo
//! required. The shortcut/file points at the running `catstage` binary with
//! the same `--socks` / `--name` arguments the running process was launched
//! with — `catc autostart install` populates those from the live `TauriCtx`.

use std::path::PathBuf;

use anyhow::Result;

#[derive(Debug, Clone)]
// On platforms with no autostart implementation, install() is a no-op and
// reads neither field — without this, dead_code fires on those targets only.
#[cfg_attr(
    all(
        not(target_os = "windows"),
        not(target_os = "linux"),
        not(target_os = "macos")
    ),
    allow(dead_code)
)]
pub struct AutostartArgs {
    /// `--socks <wss://...>` that the autostarted process should use.
    pub socks: String,
    /// Optional `--name <stage-name>` override.
    pub name: Option<String>,
    /// Optional `--screen <N>` monitor selection (1-based).
    pub screen: Option<u32>,
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
    if let Some(screen) = args.screen {
        args_str.push_str(&format!(" --screen {screen}"));
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
    if let Some(screen) = args.screen {
        exec.push_str(&format!(" --screen {screen}"));
    }
    let path = dir.join("catstage.desktop");
    let body = format!(
        "[Desktop Entry]\nType=Application\nName=CatCast\nComment=CatCast digital signage viewer\nExec={exec}\nX-GNOME-Autostart-enabled=true\nTerminal=false\n"
    );
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(Some(path))
}

#[cfg(target_os = "macos")]
pub fn install(args: &AutostartArgs) -> Result<Option<PathBuf>> {
    use anyhow::Context;
    let dir = macos_launch_agents_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let exe = std::env::current_exe().context("locating current executable")?;

    let mut program_args = String::new();
    program_args.push_str(&format!(
        "        <string>{}</string>\n",
        xml_escape(&exe.display().to_string())
    ));
    program_args.push_str("        <string>--socks</string>\n");
    program_args.push_str(&format!(
        "        <string>{}</string>\n",
        xml_escape(&args.socks)
    ));
    if let Some(name) = &args.name {
        program_args.push_str("        <string>--name</string>\n");
        program_args.push_str(&format!("        <string>{}</string>\n", xml_escape(name)));
    }
    if let Some(screen) = args.screen {
        program_args.push_str("        <string>--screen</string>\n");
        program_args.push_str(&format!("        <string>{screen}</string>\n"));
    }

    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTD/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20   <key>Label</key>\n\
         \x20   <string>{label}</string>\n\
         \x20   <key>ProgramArguments</key>\n\
         \x20   <array>\n\
         {program_args}\
         \x20   </array>\n\
         \x20   <key>RunAtLoad</key>\n\
         \x20   <true/>\n\
         \x20   <key>KeepAlive</key>\n\
         \x20   <false/>\n\
         </dict>\n\
         </plist>\n",
        label = xml_escape(LAUNCH_AGENT_LABEL),
    );

    let path = dir.join(format!("{LAUNCH_AGENT_LABEL}.plist"));
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(Some(path))
}

#[cfg(all(
    not(target_os = "windows"),
    not(target_os = "linux"),
    not(target_os = "macos")
))]
pub fn install(_args: &AutostartArgs) -> Result<Option<PathBuf>> {
    // Other Unix: not implemented. The catcast://about UI surfaces this to
    // the operator.
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

#[cfg(target_os = "macos")]
pub fn is_installed() -> Option<PathBuf> {
    let p = macos_launch_agents_dir()
        .ok()?
        .join(format!("{LAUNCH_AGENT_LABEL}.plist"));
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

#[cfg(all(
    not(target_os = "windows"),
    not(target_os = "linux"),
    not(target_os = "macos")
))]
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

#[cfg(target_os = "macos")]
pub fn uninstall() -> Result<bool> {
    let p = macos_launch_agents_dir()?.join(format!("{LAUNCH_AGENT_LABEL}.plist"));
    if p.exists() {
        std::fs::remove_file(&p)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(all(
    not(target_os = "windows"),
    not(target_os = "linux"),
    not(target_os = "macos")
))]
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

#[cfg(target_os = "macos")]
const LAUNCH_AGENT_LABEL: &str = "ch.helvetia.catcast.catstage";

#[cfg(target_os = "macos")]
fn macos_launch_agents_dir() -> Result<PathBuf> {
    use anyhow::Context;
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents"))
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_struct_is_constructable() {
        let a = AutostartArgs {
            socks: "wss://x/r/y".into(),
            name: Some("kitchen".into()),
            screen: Some(2),
        };
        assert!(a.socks.starts_with("wss://"));
        assert_eq!(a.name.as_deref(), Some("kitchen"));
        assert_eq!(a.screen, Some(2));
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
            screen: Some(2),
        })
        .unwrap()
        .expect("Linux install should return Some(path)");
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("[Desktop Entry]"));
        // No shell metachars in either value, so no surrounding quotes.
        assert!(body.contains("--socks wss://example/r/test"));
        assert!(body.contains("--name kitchen"));
        assert!(body.contains("--screen 2"));
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
    #[cfg(all(
        not(target_os = "windows"),
        not(target_os = "linux"),
        not(target_os = "macos")
    ))]
    fn install_is_noop_on_unsupported_os() {
        let r = install(&AutostartArgs {
            socks: "wss://x".into(),
            name: None,
            screen: None,
        })
        .unwrap();
        assert!(r.is_none());
    }

    // macOS install/uninstall round-trip under a sandboxed HOME so we don't
    // touch the developer's real LaunchAgents dir.
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_install_uninstall_round_trip() {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _g = LOCK.lock().unwrap();

        let tmp = std::env::temp_dir().join(format!("catstage-autostart-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev_home = std::env::var_os("HOME");
        // SAFETY: pre-call snapshot above; restored after the test.
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        assert!(is_installed().is_none(), "fresh dir should report none");
        let path = install(&AutostartArgs {
            socks: "wss://example/r/test".into(),
            name: Some("kitchen".into()),
            screen: Some(2),
        })
        .unwrap()
        .expect("macOS install should return Some(path)");
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("<key>Label</key>"));
        assert!(body.contains("<string>ch.helvetia.catcast.catstage</string>"));
        assert!(body.contains("<string>--socks</string>"));
        assert!(body.contains("<string>wss://example/r/test</string>"));
        assert!(body.contains("<string>--name</string>"));
        assert!(body.contains("<string>kitchen</string>"));
        assert!(body.contains("<string>--screen</string>"));
        assert!(body.contains("<string>2</string>"));
        assert!(body.contains("<key>RunAtLoad</key>"));
        assert_eq!(is_installed().as_deref(), Some(path.as_path()));

        assert!(uninstall().unwrap());
        assert!(!path.exists());
        assert!(is_installed().is_none());

        // SAFETY: restore prior env state.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_xml_escape_handles_special_chars() {
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
        assert_eq!(xml_escape("plain"), "plain");
    }
}

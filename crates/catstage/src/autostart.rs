//! Auto-start installer for catstage.
//!
//! On Windows we drop a `.lnk` shortcut into the per-user Startup folder, so
//! the stage relaunches at every login with the operator's `--socks`/`--name`
//! preserved. No admin rights, no registry edits.
//!
//! On non-Windows targets we print a friendly note and write nothing —
//! catstage is Windows-only in v1, but this module compiles everywhere so the
//! rest of the crate can be developed on Linux/WSL.

use anyhow::Result;
use std::path::PathBuf;

/// Arguments to persist into the autostart shortcut.
#[derive(Debug, Clone)]
pub struct AutostartArgs {
    pub socks: String,
    pub name: Option<String>,
}

impl AutostartArgs {
    /// Reproduce the command-line the shortcut needs to invoke.
    pub fn to_cli_args(&self) -> Vec<String> {
        let mut v = vec!["--socks".into(), self.socks.clone()];
        if let Some(n) = &self.name {
            v.push("--name".into());
            v.push(n.clone());
        }
        v
    }
}

#[cfg(target_os = "windows")]
pub fn install(args: &AutostartArgs) -> Result<Option<PathBuf>> {
    use anyhow::Context;
    use mslnk::ShellLink;

    let exe = std::env::current_exe().context("locating current executable")?;
    let appdata = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .context("APPDATA env var missing")?;
    let startup = appdata
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup");
    std::fs::create_dir_all(&startup).with_context(|| format!("creating {}", startup.display()))?;
    let lnk_path = startup.join("catstage.lnk");

    let mut link =
        ShellLink::new(&exe).with_context(|| format!("building shortcut for {}", exe.display()))?;
    link.set_arguments(Some(args.to_cli_args().join(" ")));
    link.set_name(Some("CatCast Stage".into()));
    link.create_lnk(&lnk_path)
        .with_context(|| format!("writing {}", lnk_path.display()))?;
    Ok(Some(lnk_path))
}

#[cfg(not(target_os = "windows"))]
pub fn install(args: &AutostartArgs) -> Result<Option<PathBuf>> {
    let cli = std::iter::once("catstage".to_string())
        .chain(args.to_cli_args())
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!(
        "autostart install not implemented for this OS — copy this command \
         into your XDG autostart (or equivalent):\n    {cli}"
    );
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_round_trip() {
        let a = AutostartArgs {
            socks: "wss://x/r/y".into(),
            name: Some("kitchen".into()),
        };
        assert_eq!(
            a.to_cli_args(),
            vec!["--socks", "wss://x/r/y", "--name", "kitchen"]
        );
        let a = AutostartArgs {
            socks: "wss://x/r/y".into(),
            name: None,
        };
        assert_eq!(a.to_cli_args(), vec!["--socks", "wss://x/r/y"]);
    }
}

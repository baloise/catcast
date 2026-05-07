//! catstage — CatCast fullscreen viewer.
//!
//! NOTE: this scaffold is the headless skeleton. The Tauri/WebView2 shell,
//! the about:catcast custom URI scheme handler, the global Ctrl+Alt+M
//! hotkey, the Startup-folder autostart installer, and the Rhai logic
//! engine are all implemented in subsequent commits — see the plan file
//! and the matching pending todos.

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "catstage", version, about = "CatCast fullscreen viewer")]
struct Args {
    /// WebSocket URL of the CatSocks broker, e.g. wss://.../r/<room>.
    #[arg(long)]
    socks: String,

    /// Stage name. Defaults to the machine's hostname.
    #[arg(long)]
    name: Option<String>,

    /// Drop a Startup-folder shortcut so this stage launches at login.
    #[arg(long)]
    install_autostart: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let name = args
        .name
        .or_else(|| hostname::get().ok().and_then(|h| h.into_string().ok()))
        .unwrap_or_else(|| "catstage".into());
    eprintln!(
        "catstage v{} skeleton — name={name:?} socks={:?} install_autostart={}",
        env!("CARGO_PKG_VERSION"),
        args.socks,
        args.install_autostart
    );
    eprintln!("(Tauri shell, scheduler, Rhai engine, hotkey, autostart pending — see plan)");
    Ok(())
}

mod hostname {
    pub fn get() -> std::io::Result<std::ffi::OsString> {
        // Cross-platform-ish: read $HOSTNAME on unixy hosts, fall back to
        // gethostname on Windows. For now we just shell out to keep deps
        // out of the scaffold.
        if let Ok(h) = std::env::var("HOSTNAME") {
            if !h.is_empty() {
                return Ok(h.into());
            }
        }
        if let Ok(h) = std::env::var("COMPUTERNAME") {
            if !h.is_empty() {
                return Ok(h.into());
            }
        }
        std::process::Command::new("hostname").output().map(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            std::ffi::OsString::from(s)
        })
    }
}

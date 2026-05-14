//! catcast://about — the info-only page.
//!
//! Serves as `index.html` of the bundled frontend (so Tauri auto-injects
//! `window.__TAURI__` and the IPC bridge works). The JS in `ui/index.html`
//! invokes `get_snapshot` to populate itself on first paint, then listens
//! for `catcast://state` events for live updates.
//!
//! Note on the design: the about page UI is read-only — no buttons; every
//! operator action is a `catc` command. The Tauri commands that survive
//! on this surface are: `get_snapshot` (the page reads its own data),
//! `cmd_log` (forwards JS console to stderr for diagnostics), and
//! `enter_idle` (visiting the page asserts `Mode::Idle`, so F1 from a
//! rotation URL is equivalent to `catc about`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use catcast_core::State;
use serde::Serialize;
use tauri::{Emitter, Manager};
use tokio::sync::mpsc;

use crate::scheduler;
use crate::Shared as MainShared;

/// Canonical in-memory target for "go to about" actions. Updated by `main.rs`
/// when it sees a finished load of the bundled about page, and used by both
/// the injected F1 handler and Rust-side Idle transitions.
pub struct AboutUrl(pub Mutex<tauri::Url>);

impl AboutUrl {
    pub fn new(url: tauri::Url) -> Self {
        Self(Mutex::new(url))
    }

    pub fn get(&self) -> tauri::Url {
        self.0.lock().expect("AboutUrl poisoned").clone()
    }

    pub fn set(&self, url: tauri::Url) {
        *self.0.lock().expect("AboutUrl poisoned") = url;
    }
}

/// Shared runtime handles for the about page snapshot + the broker dispatch
/// path. `make_snapshot` reads `shared`/`broker_url`/`stage_name`; the
/// `Message::AutostartInstall` handler in main.rs reads `broker_url` and
/// `stage_name` to populate `AutostartArgs`; bootstrap reads `sched_tx` to
/// wire the socks inbound dispatch closure.
pub struct TauriCtx {
    pub shared: Arc<Mutex<MainShared>>,
    pub broker_url: String,
    pub sched_tx: mpsc::Sender<scheduler::Cmd>,
    pub stage_name: String,
    pub screen: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub label: String,
    pub path: Option<String>,
    pub exists: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub state: State,
    pub broker_url: String,
    pub stage_name: String,
    pub files: Vec<FileEntry>,
    pub autostart_path: Option<String>,
}

pub fn make_snapshot(ctx: &TauriCtx) -> Snapshot {
    let state = ctx.shared.lock().expect("Shared poisoned").state.clone();
    Snapshot {
        state,
        broker_url: ctx.broker_url.clone(),
        stage_name: ctx.stage_name.clone(),
        files: file_entries(),
        autostart_path: crate::autostart::is_installed().map(|p| p.display().to_string()),
    }
}

fn file_entries() -> Vec<FileEntry> {
    fn entry(label: &str, p: Result<PathBuf, anyhow::Error>) -> FileEntry {
        match p {
            Ok(path) => FileEntry {
                label: label.into(),
                exists: path.exists(),
                path: Some(path.display().to_string()),
            },
            Err(_) => FileEntry {
                label: label.into(),
                exists: false,
                path: None,
            },
        }
    }
    let exe = std::env::current_exe()
        .map(|p| FileEntry {
            label: "catstage".into(),
            exists: true,
            path: Some(p.display().to_string()),
        })
        .unwrap_or_else(|_| FileEntry {
            label: "catstage".into(),
            exists: false,
            path: None,
        });
    vec![
        entry("config.yaml", crate::persist::config_path()),
        entry("logic.rhai", crate::persist::logic_path()),
        entry("state.json", crate::persist::state_path()),
        exe,
    ]
}

// ---------------------------------------------------------------------------
// Tauri commands — only two survive: the page's read of its own data, and
// the JS console forwarding sink.
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn get_snapshot(ctx: tauri::State<'_, TauriCtx>) -> Result<Snapshot, String> {
    Ok(make_snapshot(&ctx))
}

/// JS console-forwarding sink. The about page wraps `console.log/warn/error`
/// to invoke this command, so we can grep catstage's stderr for JS issues
/// without attaching DevTools. Levels: "log" | "info" | "warn" | "error" |
/// "debug".
#[tauri::command]
pub async fn cmd_log(level: String, msg: String) -> Result<(), String> {
    eprintln!("[js {level}] {msg}");
    Ok(())
}

/// Assert `Mode::Idle` while the about page is showing. Called by the page's
/// first-paint JS so that arriving via F1 (which is a pure-JS
/// `window.location.href` jump and can't speak the broker protocol) still
/// pauses rotation — otherwise the scheduler would yank the operator back
/// to a rotation URL at the next slot boundary. Idempotent when already Idle.
#[tauri::command]
pub async fn enter_idle(ctx: tauri::State<'_, TauriCtx>) -> Result<(), String> {
    ctx.sched_tx
        .send(scheduler::Cmd::Idle)
        .await
        .map_err(|e| e.to_string())
}

/// Exit the catstage process when confirmed by the about page.
#[tauri::command]
pub async fn request_exit(app: tauri::AppHandle) -> Result<(), String> {
    app.exit(0);
    Ok(())
}

/// Push the latest snapshot to the about page (via Tauri event). Called
/// from `SchedEvents` (and from broker dispatch handlers in `main.rs`)
/// after state changes.
pub fn emit_state(app: &tauri::AppHandle, label: &str) {
    let Some(ctx) = app.try_state::<TauriCtx>() else {
        return;
    };
    let snap = make_snapshot(&ctx);
    let _ = app.emit_to(label, "catcast://state", snap);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_entries_include_all_four() {
        let entries = file_entries();
        let labels: Vec<&str> = entries.iter().map(|e| e.label.as_str()).collect();
        assert!(labels.contains(&"config.yaml"));
        assert!(labels.contains(&"logic.rhai"));
        assert!(labels.contains(&"state.json"));
        assert!(labels.contains(&"catstage"));
    }
}

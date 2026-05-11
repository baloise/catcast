//! catcast://about — the info-and-control page.
//!
//! Serves as `index.html` of the bundled frontend (so Tauri auto-injects
//! `window.__TAURI__` and the IPC bridge works). The JS in `ui/index.html`
//! invokes `get_snapshot` to populate itself on first paint, then listens for
//! `catcast://state` events for live updates.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use catcast_core::{Config, State};
use catcast_proto::Message;
use serde::Serialize;
use tauri::{Emitter, Manager};
use tokio::sync::{mpsc, Notify};

/// The URL the window loaded on first paint — captured in `main.rs`'s setup
/// callback and used by hotkey commands to navigate the kiosk back to the
/// about page from a rotation URL. Platform-specific: typically
/// `tauri://localhost/` on Linux/macOS or `http://tauri.localhost/` on Windows.
pub struct AboutUrl(pub tauri::Url);

use crate::logic::{self, LogicHandle, Plan};
use crate::scheduler;
use crate::Shared as MainShared;

/// Runtime handles surfaced to Tauri commands as `tauri::State<TauriCtx>`.
///
/// Kept separate from [`MainShared`] (which the scheduler events also touch)
/// because tauri::State requires `Send + Sync + 'static` and we want a stable
/// boundary between scheduler/socks plumbing and the Tauri command layer.
pub struct TauriCtx {
    pub shared: Arc<Mutex<MainShared>>,
    pub broker_url: String,
    pub sched_tx: mpsc::Sender<scheduler::Cmd>,
    pub logic_handle: Arc<Mutex<Option<LogicHandle>>>,
    /// Kept for later use by `Message::GetState` replies that need to push a
    /// State to the broker.
    #[allow(dead_code)]
    pub out_tx: Arc<Mutex<Option<mpsc::Sender<Message>>>>,
    pub stage_name: String,
    /// `cmd_force_reconnect` notifies this so the socks task drops its
    /// current connection and tries again immediately.
    pub socks_abort: Arc<Notify>,
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
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn get_snapshot(ctx: tauri::State<'_, TauriCtx>) -> Result<Snapshot, String> {
    Ok(make_snapshot(&ctx))
}

#[tauri::command]
pub async fn cmd_pause(ctx: tauri::State<'_, TauriCtx>) -> Result<(), String> {
    ctx.sched_tx
        .send(scheduler::Cmd::Pause)
        .await
        .map_err(to_string)
}

#[tauri::command]
pub async fn cmd_play(ctx: tauri::State<'_, TauriCtx>) -> Result<(), String> {
    ctx.sched_tx
        .send(scheduler::Cmd::Play)
        .await
        .map_err(to_string)
}

#[tauri::command]
pub async fn cmd_manual(on: bool, ctx: tauri::State<'_, TauriCtx>) -> Result<(), String> {
    ctx.sched_tx
        .send(scheduler::Cmd::Manual(on))
        .await
        .map_err(to_string)
}

#[tauri::command]
pub async fn cmd_toggle_fullscreen(window: tauri::Window) -> Result<(), String> {
    let now = window.is_fullscreen().map_err(to_string)?;
    window.set_fullscreen(!now).map_err(to_string)
}

#[tauri::command]
pub async fn cmd_force_reconnect(ctx: tauri::State<'_, TauriCtx>) -> Result<(), String> {
    // Poke the socks task: it drops the current WebSocket (or cuts a backoff
    // sleep short) and reconnects immediately.
    ctx.socks_abort.notify_one();
    Ok(())
}

#[tauri::command]
pub async fn cmd_reload_from_disk(ctx: tauri::State<'_, TauriCtx>) -> Result<(), String> {
    let (yaml, rhai) = {
        let sh = ctx.shared.lock().expect("Shared poisoned");
        (sh.config_yaml.clone(), sh.logic_rhai.clone())
    };
    let Some(yaml) = yaml else {
        return Err("no config.yaml on disk".into());
    };
    let Some(rhai) = rhai else {
        return Err("no logic.rhai on disk".into());
    };
    let plan = build_plan(&yaml, &rhai, &ctx.logic_handle).map_err(|e| format!("{e:#}"))?;
    ctx.sched_tx
        .send(scheduler::Cmd::Rebuild(plan))
        .await
        .map_err(to_string)
}

#[tauri::command]
pub async fn cmd_install_autostart(
    name: String,
    socks: String,
    _ctx: tauri::State<'_, TauriCtx>,
) -> Result<Option<String>, String> {
    let args = crate::autostart::AutostartArgs {
        socks,
        name: if name.is_empty() { None } else { Some(name) },
    };
    crate::autostart::install(&args)
        .map(|opt| opt.map(|p| p.display().to_string()))
        .map_err(to_string)
}

#[tauri::command]
pub async fn cmd_nav(
    url: String,
    window: tauri::Window,
    ctx: tauri::State<'_, TauriCtx>,
) -> Result<(), String> {
    ctx.sched_tx
        .send(scheduler::Cmd::Nav(url.clone()))
        .await
        .map_err(to_string)?;
    let parsed = tauri::Url::parse(&url).map_err(to_string)?;
    let label = window.label().to_string();
    let w = window
        .get_webview_window(&label)
        .ok_or_else(|| "no webview window".to_string())?;
    w.navigate(parsed).map_err(to_string)
}

#[tauri::command]
pub async fn cmd_open_dir(path: String, app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    // "Open dir" semantics: regardless of whether the path is a real file,
    // a not-yet-created file, or a directory, reveal the *directory*. The
    // old code only stripped to the parent when `is_file()` returned true,
    // which silently failed for files that don't exist yet (e.g. config.yaml
    // before the operator has pushed any config).
    let target = PathBuf::from(&path);
    let to_reveal = if target.is_dir() {
        target.clone()
    } else {
        target
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    };
    app.opener()
        .open_path(to_reveal.display().to_string(), None::<&str>)
        .map_err(to_string)
}

/// Internal command: the catcast://about JS keydown handler routes Ctrl+Alt+M
/// here so the Rust side owns the manual-mode toggle and the matching
/// webview navigation.
#[tauri::command]
pub async fn cmd_hotkey_toggle_manual(
    window: tauri::Window,
    ctx: tauri::State<'_, TauriCtx>,
    about: tauri::State<'_, AboutUrl>,
) -> Result<(), String> {
    let (now_manual, target_url) = {
        let s = ctx.shared.lock().expect("Shared poisoned");
        (s.state.manual, s.state.current_url.clone())
    };
    let next = !now_manual;
    ctx.sched_tx
        .send(scheduler::Cmd::Manual(next))
        .await
        .map_err(to_string)?;
    let label = window.label().to_string();
    if next {
        if let Some(w) = window.get_webview_window(&label) {
            w.navigate(about.0.clone()).map_err(to_string)?;
        }
    } else if let Some(url) = target_url {
        if let Ok(parsed) = tauri::Url::parse(&url) {
            if let Some(w) = window.get_webview_window(&label) {
                let _ = w.navigate(parsed);
            }
        }
    }
    Ok(())
}

/// Internal command: Esc inside the about page leaves manual mode. Outside
/// manual mode it's a no-op (kiosk Esc should not exit the app).
#[tauri::command]
pub async fn cmd_hotkey_escape(
    window: tauri::Window,
    ctx: tauri::State<'_, TauriCtx>,
) -> Result<(), String> {
    let (is_manual, target_url) = {
        let s = ctx.shared.lock().expect("Shared poisoned");
        (s.state.manual, s.state.current_url.clone())
    };
    if !is_manual {
        return Ok(());
    }
    ctx.sched_tx
        .send(scheduler::Cmd::Manual(false))
        .await
        .map_err(to_string)?;
    if let Some(url) = target_url {
        if let Ok(parsed) = tauri::Url::parse(&url) {
            let label = window.label().to_string();
            if let Some(w) = window.get_webview_window(&label) {
                let _ = w.navigate(parsed);
            }
        }
    }
    Ok(())
}

/// Quit the catstage process. Surfaced as an `[Exit]` button on the about
/// page for operators who need to bail out without dropping to the OS
/// (since the kiosk window has no decorations and no taskbar).
#[tauri::command]
pub async fn cmd_exit(app: tauri::AppHandle) -> Result<(), String> {
    app.exit(0);
    Ok(())
}

/// JS console-forwarding sink. The about page wraps `console.log/warn/error`
/// to invoke this command in addition to writing to the in-webview console,
/// so we can grep catstage's stderr for JS issues without attaching DevTools.
/// Levels: "log" | "info" | "warn" | "error" | "debug".
#[tauri::command]
pub async fn cmd_log(level: String, msg: String) -> Result<(), String> {
    eprintln!("[js {level}] {msg}");
    Ok(())
}

/// Toggle the WebKit / WebView2 inspector window. Bound to F12 on the about
/// page so an operator can attach DevTools without restarting with --devtools.
#[tauri::command]
pub async fn cmd_toggle_devtools(window: tauri::Window) -> Result<(), String> {
    let label = window.label().to_string();
    let Some(w) = window.get_webview_window(&label) else {
        return Err("no webview window".into());
    };
    if w.is_devtools_open() {
        w.close_devtools();
    } else {
        w.open_devtools();
    }
    Ok(())
}

fn to_string<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Build a Plan from on-disk YAML + Rhai. Mirrors the existing build_plan in
/// main.rs so cmd_reload_from_disk doesn't have to depend on a private fn.
fn build_plan(
    yaml: &str,
    rhai: &str,
    logic_handle: &Arc<Mutex<Option<LogicHandle>>>,
) -> anyhow::Result<Plan> {
    let cfg = Config::from_yaml(yaml).context("parsing config.yaml")?;
    let _ = cfg.validate();
    let (plan, handle) = logic::evaluate(rhai, &cfg).context("evaluating Rhai logic")?;
    *logic_handle.lock().expect("LogicHandle poisoned") = Some(handle);
    Ok(plan)
}

/// Push the latest snapshot to the about page (via Tauri event). Called from
/// `SchedEvents` after state changes.
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

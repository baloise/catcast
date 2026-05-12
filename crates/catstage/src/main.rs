//! catstage — CatCast fullscreen viewer.
//!
//! Tauri 2.x shell. Owns the fullscreen WebView2 window, hosts the
//! `catcast://about` URI scheme, and runs the scheduler/socks/Rhai engine in
//! the same tokio runtime Tauri starts.
//!
//! Window-focused hotkeys (Ctrl+Alt+M, F11, Esc) are caught by a JS keydown
//! listener inside `catcast://about` and forwarded to Rust via `invoke` — chosen
//! because Tauri 2 doesn't yet expose a stable Rust-side window key handler.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use catcast_core::{Config, State};
use catcast_proto::{Key, Message};
use clap::Parser;
use tauri::Manager;

mod about;
mod autostart;
mod logic;
mod persist;
mod scheduler;
mod socks;

use crate::about::TauriCtx;
use crate::logic::{LogicHandle, Plan};

const WINDOW_LABEL: &str = "stage";

/// Build the F1-only escape script for `WebviewWindow::on_page_load`.
/// Pure JS, no IPC dependency — works on any page including external
/// rotation URLs where Tauri doesn't inject `window.__TAURI__`. Operators
/// at the physical kiosk press F1 to get back to the admin page; every
/// other action is a `catc` command from a laptop.
fn build_escape_script(about_url: &str) -> String {
    // Escape just in case the URL ever contains characters that would break
    // the JS string. tauri::Url is well-formed but defence in depth.
    let about_url = about_url.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"
(function () {{
  if (window.__catcastEscapeInstalled) return;
  window.__catcastEscapeInstalled = true;
  const ABOUT_URL = "{about_url}";
  document.addEventListener("keydown", (ev) => {{
    if (ev.key === "F1") {{
      ev.preventDefault();
      window.location.href = ABOUT_URL;
    }}
  }}, true);
}})();
"#
    )
}

#[derive(Parser, Clone)]
#[command(name = "catstage", version, about = "CatCast fullscreen viewer")]
struct Args {
    /// WebSocket URL of the CatSocks broker, e.g. wss://.../r/<room>.
    #[arg(long)]
    socks: String,

    /// Stage name. Defaults to the machine's hostname.
    #[arg(long)]
    name: Option<String>,

    /// Drop a Startup-folder shortcut so this stage launches at login,
    /// then exit. (The catcast://about wizard does the same thing
    /// interactively.)
    #[arg(long)]
    install_autostart: bool,

    /// Open WebKit/WebView2 DevTools alongside the kiosk window. Opt-in
    /// for development; not bound to debug builds so a release binary
    /// can also be inspected when needed.
    #[arg(long)]
    devtools: bool,
}

/// Everything the dispatch path mutates. Held behind one mutex so the socks
/// handler doesn't need a separate lock per field. `pub(crate)` so `about.rs`
/// can use it in `TauriCtx`.
pub(crate) struct Shared {
    pub state: State,
    pub config_yaml: Option<String>,
    pub logic_rhai: Option<String>,
}

fn main() -> Result<()> {
    // Force X11 over Wayland on Linux. WebKitGTK's Wayland path doesn't
    // honour set_fullscreen() reliably on WSLg / Weston — the window opens
    // in a small default size and `Toggle Fullscreen` / F11 only seem to
    // act once. Under X11 (XWayland under WSLg) fullscreen works correctly.
    // No-op on Windows / macOS, where the binary doesn't read GDK_BACKEND.
    //
    // `set_var` is safe here: we're still single-threaded — Tauri's runtime
    // hasn't been built, no background work has been spawned.
    #[cfg(target_os = "linux")]
    if std::env::var_os("GDK_BACKEND").is_none() {
        // SAFETY: pre-runtime, single-threaded; no other thread is reading env.
        unsafe {
            std::env::set_var("GDK_BACKEND", "x11");
        }
    }

    let args = Args::parse();
    let name = args.name.clone().unwrap_or_else(default_name);

    if args.install_autostart {
        if let Some(path) = autostart::install(&autostart::AutostartArgs {
            socks: args.socks.clone(),
            name: args.name.clone(),
        })? {
            eprintln!("catstage: autostart installed at {}", path.display());
        }
        return Ok(());
    }

    eprintln!("catstage v{} starting as {name}", env!("CARGO_PKG_VERSION"),);

    let broker_url = args.socks.clone();
    let stage_name = name.clone();
    let open_devtools = args.devtools;

    tauri::Builder::default()
        // Install a tiny F1 → back-to-about handler on every page load.
        // It's the only cross-origin shortcut we ship; everything else
        // operators do via `catc`. See `build_escape_script` for the
        // (intentionally minimal) JS body.
        .on_page_load(|webview, payload| {
            if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                let about_url = webview
                    .app_handle()
                    .try_state::<about::AboutUrl>()
                    .map(|s| s.0.to_string())
                    .unwrap_or_else(|| "tauri://localhost/".into());
                let _ = webview.eval(build_escape_script(&about_url));
            }
        })
        .invoke_handler(tauri::generate_handler![
            about::get_snapshot,
            about::cmd_log,
        ])
        .setup(move |app| {
            // Force fullscreen at runtime in addition to the config-time
            // request. WSLg / Wayland in particular tend to ignore the
            // config-time `fullscreen: true` because the compositor isn't
            // ready when the window is created.
            //
            // Also capture the initial URL (the bundled index.html, served by
            // Tauri at `tauri://localhost/` on Linux or `http://tauri.localhost/`
            // on Windows) so hotkey commands can navigate back to it later
            // without hard-coding a platform-specific URL.
            if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
                let _ = window.set_fullscreen(true);
                if let Ok(url) = window.url() {
                    app.manage(about::AboutUrl(url));
                }
                // Opt-in: only open DevTools when --devtools was passed.
                // Auto-opening on every run was intrusive for normal use.
                if open_devtools {
                    window.open_devtools();
                }
            }

            // Manage TauriCtx synchronously so invoke('get_snapshot') from
            // the about page's first paint sees managed state. The scheduler
            // and socks tasks are still spawned asynchronously below — but
            // we hand them their channel half-ends so the tx halves are in
            // TauriCtx from the very start.
            let (sched_tx, sched_rx) = tokio::sync::mpsc::channel(32);
            let shared = std::sync::Arc::new(std::sync::Mutex::new(Shared {
                state: State::fresh(&stage_name, env!("CARGO_PKG_VERSION")),
                config_yaml: None,
                logic_rhai: None,
            }));
            let logic_handle: std::sync::Arc<std::sync::Mutex<Option<LogicHandle>>> =
                std::sync::Arc::new(std::sync::Mutex::new(None));
            let out_tx: std::sync::Arc<
                std::sync::Mutex<Option<tokio::sync::mpsc::Sender<Message>>>,
            > = std::sync::Arc::new(std::sync::Mutex::new(None));
            let socks_abort = std::sync::Arc::new(tokio::sync::Notify::new());

            app.manage(TauriCtx {
                shared: std::sync::Arc::clone(&shared),
                broker_url: broker_url.clone(),
                sched_tx: sched_tx.clone(),
                stage_name: stage_name.clone(),
            });

            let handle = app.handle().clone();
            let bootstrap_name = stage_name.clone();
            let bootstrap_url = broker_url.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = bootstrap(
                    handle,
                    bootstrap_name,
                    bootstrap_url,
                    shared,
                    logic_handle,
                    out_tx,
                    socks_abort,
                    sched_rx,
                )
                .await
                {
                    eprintln!("catstage: bootstrap failed: {e:#}");
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .map_err(|e| anyhow::anyhow!("tauri runtime: {e}"))
}

/// Wire up scheduler + socks once Tauri's runtime is alive. State containers
/// and the scheduler receiver are created synchronously in `setup()` and
/// passed in, so `TauriCtx` is already managed and commands invoked from the
/// about page's first paint succeed.
#[allow(clippy::too_many_arguments)]
async fn bootstrap(
    app: tauri::AppHandle,
    name: String,
    _broker_url: String,
    shared: Arc<Mutex<Shared>>,
    logic_handle: Arc<Mutex<Option<LogicHandle>>>,
    out_tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<Message>>>>,
    socks_abort: Arc<tokio::sync::Notify>,
    sched_rx: tokio::sync::mpsc::Receiver<scheduler::Cmd>,
) -> Result<()> {
    // Hydrate Shared from disk now that we're in an async context.
    if let Some(s) = persist::load_state()? {
        shared.lock().unwrap().state = s;
    }
    let config_yaml = persist::load_config_yaml()?;
    let logic_rhai = persist::load_logic_rhai()?;
    {
        let mut sh = shared.lock().unwrap();
        sh.config_yaml = config_yaml.clone();
        sh.logic_rhai = logic_rhai.clone();
    }

    if config_yaml.is_none() && logic_rhai.is_none() {
        eprintln!(
            "catstage: no config/logic on disk — catcast://about splash with stage name '{name}'"
        );
    }

    let key = Arc::new(Key::from_psk(&name).context("deriving AEAD key from stage name")?);

    let initial_plan = match (&config_yaml, &logic_rhai) {
        (Some(yaml), Some(rhai)) => match build_plan(yaml, rhai, &logic_handle) {
            Ok(p) => {
                let mut sh = shared.lock().unwrap();
                sh.state.has_config = true;
                sh.state.has_logic = true;
                p
            }
            Err(e) => {
                eprintln!("catstage: ignoring on-disk config/logic, eval failed: {e:#}");
                Plan::default()
            }
        },
        _ => Plan::default(),
    };

    let events = Arc::new(SchedEvents {
        app: app.clone(),
        shared: Arc::clone(&shared),
        out: Arc::clone(&out_tx),
    });

    scheduler::spawn_with_rx(
        sched_rx,
        initial_plan,
        Some(Arc::clone(&logic_handle)),
        events.clone(),
    );

    // sched_tx already lives in TauriCtx (managed synchronously in setup);
    // clone it for the socks inbound dispatch closure.
    let sched_tx_for_dispatch = app
        .state::<crate::about::TauriCtx>()
        .inner()
        .sched_tx
        .clone();

    let handler_shared = Arc::clone(&shared);
    let handler_logic = Arc::clone(&logic_handle);
    let handler_out = Arc::clone(&out_tx);
    let handler_app = app.clone();
    let handler: socks::InboundHandler = Arc::new(move |pt| {
        let pt_msg = pt.msg.clone();
        let shared = Arc::clone(&handler_shared);
        let sched = sched_tx_for_dispatch.clone();
        let logic = Arc::clone(&handler_logic);
        let out = Arc::clone(&handler_out);
        let app = handler_app.clone();
        tokio::spawn(async move {
            dispatch(pt_msg, shared, sched, logic, out, app).await;
        });
    });

    let broker_url = app
        .state::<crate::about::TauriCtx>()
        .inner()
        .broker_url
        .clone();
    let outbound = socks::spawn(
        broker_url,
        name.clone(),
        Arc::clone(&key),
        handler,
        Arc::clone(&socks_abort),
    );
    *out_tx.lock().unwrap() = Some(outbound.clone());

    let snapshot = shared.lock().unwrap().state.clone();
    let _ = outbound.send(Message::State(snapshot)).await;

    Ok(())
}

fn default_name() -> String {
    let raw = gethostname::gethostname();
    raw.to_string_lossy().to_string()
}

struct SchedEvents {
    app: tauri::AppHandle,
    shared: Arc<Mutex<Shared>>,
    out: Arc<Mutex<Option<tokio::sync::mpsc::Sender<Message>>>>,
}

impl scheduler::Events for SchedEvents {
    fn on_url_change(&self, url: &str) {
        // State-equality only gates persist + broadcast (those should be
        // idempotent and cheap to skip). The webview steer must run every
        // time — otherwise a Nav-to-the-already-recorded-URL after a
        // reboot would update nothing and the kiosk would stay on
        // catcast://about even though current_url claims otherwise.
        let mut sh = self.shared.lock().unwrap();
        let changed = sh.state.current_url.as_deref() != Some(url);
        if changed {
            sh.state.current_url = Some(url.to_string());
            sh.state.since = chrono::Utc::now().timestamp_millis();
        }
        let snap = sh.state.clone();
        drop(sh);
        if changed {
            broadcast(&self.out, Message::State(snap));
            if let Err(e) = persist::save_state(&self.shared.lock().unwrap().state) {
                eprintln!("catstage: state save failed: {e:#}");
            }
        }
        steer_webview(&self.app, url);
        about::emit_state(&self.app, WINDOW_LABEL);
    }
    fn on_pause_change(&self, paused: bool) {
        let mut sh = self.shared.lock().unwrap();
        sh.state.paused = paused;
        sh.state.since = chrono::Utc::now().timestamp_millis();
        let snap = sh.state.clone();
        drop(sh);
        broadcast(&self.out, Message::State(snap));
        about::emit_state(&self.app, WINDOW_LABEL);
    }
    fn on_manual_change(&self, manual: bool) {
        let mut sh = self.shared.lock().unwrap();
        sh.state.manual = manual;
        sh.state.since = chrono::Utc::now().timestamp_millis();
        let snap = sh.state.clone();
        drop(sh);
        broadcast(&self.out, Message::State(snap));
        about::emit_state(&self.app, WINDOW_LABEL);
    }
}

/// Drive the kiosk webview to `url` unless we're in manual mode (where the
/// operator owns the URL bar) or unless the URL starts with `about:`
/// (handled by the URI scheme).
fn steer_webview(app: &tauri::AppHandle, url: &str) {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    let Some(ctx) = app.try_state::<TauriCtx>() else {
        return;
    };
    if ctx.shared.lock().unwrap().state.manual {
        return;
    }
    if let Ok(parsed) = tauri::Url::parse(url) {
        if let Err(e) = window.navigate(parsed) {
            eprintln!("catstage: navigate({url}) failed: {e}");
        }
    }
}

fn broadcast(out: &Arc<Mutex<Option<tokio::sync::mpsc::Sender<Message>>>>, msg: Message) {
    let Some(tx) = out.lock().unwrap().clone() else {
        return;
    };
    let _ = tx.try_send(msg);
}

async fn dispatch(
    msg: Message,
    shared: Arc<Mutex<Shared>>,
    sched: tokio::sync::mpsc::Sender<scheduler::Cmd>,
    logic_handle: Arc<Mutex<Option<LogicHandle>>>,
    out: Arc<Mutex<Option<tokio::sync::mpsc::Sender<Message>>>>,
    app: tauri::AppHandle,
) {
    use scheduler::Cmd;

    // Every command produces exactly one Reply back to the CLI so the operator
    // sees success / failure instead of fire-and-forget silence. Variants that
    // are themselves replies (State, Reply) or whose reply *is* the State
    // broadcast (GetState) return None.
    let outcome: Option<Result<String, String>> = match msg {
        Message::Pause => {
            let _ = sched.send(Cmd::Pause).await;
            Some(Ok("paused".into()))
        }
        Message::Play => {
            let _ = sched.send(Cmd::Play).await;
            Some(Ok("playing".into()))
        }
        Message::Nav { url } => {
            let m = format!("navigating to {url}");
            let _ = sched.send(Cmd::Nav(url)).await;
            Some(Ok(m))
        }
        Message::NavTimed { url, duration_secs } => {
            let m = format!("navigating to {url} for {duration_secs}s");
            let _ = sched
                .send(Cmd::TimedNav {
                    url,
                    secs: duration_secs,
                })
                .await;
            Some(Ok(m))
        }
        Message::Manual { on } => {
            let _ = sched.send(Cmd::Manual(on)).await;
            Some(Ok(if on {
                "manual on".into()
            } else {
                "manual off".into()
            }))
        }
        Message::SetConfig { yaml } => {
            Some(set_config(yaml, &shared, &sched, &logic_handle, &app).await)
        }
        Message::SetLogic { rhai } => {
            Some(set_logic(rhai, &shared, &sched, &logic_handle, &app).await)
        }
        Message::GetState => {
            let snap = shared.lock().unwrap().state.clone();
            broadcast(&out, Message::State(snap));
            None
        }
        Message::AutostartInstall => Some(match app.try_state::<crate::about::TauriCtx>() {
            Some(ctx) => {
                let socks = ctx.inner().broker_url.clone();
                let name = Some(ctx.inner().stage_name.clone());
                let res = match autostart::install(&autostart::AutostartArgs { socks, name }) {
                    Ok(Some(p)) => Ok(format!("installed at {}", p.display())),
                    Ok(None) => Ok("no-op on this OS".into()),
                    Err(e) => Err(format!("install failed: {e:#}")),
                };
                about::emit_state(&app, WINDOW_LABEL);
                res
            }
            None => Err("internal: TauriCtx not initialised".into()),
        }),
        Message::AutostartUninstall => {
            let res = match autostart::uninstall() {
                Ok(true) => Ok("shortcut removed".into()),
                Ok(false) => Ok("nothing to remove".into()),
                Err(e) => Err(format!("uninstall failed: {e:#}")),
            };
            about::emit_state(&app, WINDOW_LABEL);
            Some(res)
        }
        Message::Fullscreen { on } => Some(match app.get_webview_window(WINDOW_LABEL) {
            Some(w) => match w.set_fullscreen(on) {
                Ok(()) => Ok(if on {
                    "fullscreen on".into()
                } else {
                    "fullscreen off".into()
                }),
                Err(e) => Err(format!("set_fullscreen({on}) failed: {e}")),
            },
            None => Err("no kiosk window".into()),
        }),
        Message::DevTools { on } => Some(match app.get_webview_window(WINDOW_LABEL) {
            Some(w) => {
                if on {
                    w.open_devtools();
                    Ok("devtools on".into())
                } else {
                    w.close_devtools();
                    Ok("devtools off".into())
                }
            }
            None => Err("no kiosk window".into()),
        }),
        Message::Shutdown => {
            // Reply must reach the CLI before app.exit() tears down the socks
            // task. Send it inline, give the outbound mpsc + websocket a
            // moment to flush, then exit. The CLI uses a slightly larger
            // timeout for Shutdown specifically; see cmd_send_many.
            broadcast(
                &out,
                Message::Reply {
                    ok: true,
                    message: "shutting down".into(),
                },
            );
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            eprintln!("catstage: shutdown requested via broker");
            app.exit(0);
            None
        }
        Message::State(_) | Message::Reply { .. } => None,
    };

    if let Some(res) = outcome {
        let (ok, message) = match res {
            Ok(m) => (true, m),
            Err(m) => (false, m),
        };
        broadcast(&out, Message::Reply { ok, message });
    }
}

async fn set_config(
    yaml: String,
    shared: &Arc<Mutex<Shared>>,
    sched: &tokio::sync::mpsc::Sender<scheduler::Cmd>,
    logic_handle: &Arc<Mutex<Option<LogicHandle>>>,
    app: &tauri::AppHandle,
) -> Result<String, String> {
    let cfg = Config::from_yaml(&yaml).map_err(|e| format!("bad config YAML: {e}"))?;
    let warnings = cfg
        .validate()
        .map_err(|e| format!("config validation error: {e}"))?;
    for w in &warnings {
        eprintln!("catstage: config warning: {}", w.0);
    }
    persist::save_config_yaml(&yaml).map_err(|e| format!("persist config failed: {e:#}"))?;
    {
        let mut sh = shared.lock().unwrap();
        sh.config_yaml = Some(yaml);
        sh.state.has_config = true;
    }
    rebuild_and_send(shared, sched, logic_handle).await;
    about::emit_state(app, WINDOW_LABEL);
    Ok(if warnings.is_empty() {
        "config saved".into()
    } else {
        format!(
            "config saved ({} warning{})",
            warnings.len(),
            if warnings.len() == 1 { "" } else { "s" }
        )
    })
}

async fn set_logic(
    rhai: String,
    shared: &Arc<Mutex<Shared>>,
    sched: &tokio::sync::mpsc::Sender<scheduler::Cmd>,
    logic_handle: &Arc<Mutex<Option<LogicHandle>>>,
    app: &tauri::AppHandle,
) -> Result<String, String> {
    persist::save_logic_rhai(&rhai).map_err(|e| format!("persist logic failed: {e:#}"))?;
    {
        let mut sh = shared.lock().unwrap();
        sh.logic_rhai = Some(rhai);
        sh.state.has_logic = true;
    }
    rebuild_and_send(shared, sched, logic_handle).await;
    about::emit_state(app, WINDOW_LABEL);
    Ok("logic saved".into())
}

async fn rebuild_and_send(
    shared: &Arc<Mutex<Shared>>,
    sched: &tokio::sync::mpsc::Sender<scheduler::Cmd>,
    logic_handle: &Arc<Mutex<Option<LogicHandle>>>,
) {
    let (yaml, rhai) = {
        let sh = shared.lock().unwrap();
        (sh.config_yaml.clone(), sh.logic_rhai.clone())
    };
    let (Some(yaml), Some(rhai)) = (yaml, rhai) else {
        return;
    };
    match build_plan(&yaml, &rhai, logic_handle) {
        Ok(plan) => {
            let _ = sched.send(scheduler::Cmd::Rebuild(plan)).await;
        }
        Err(e) => eprintln!("catstage: plan rebuild failed: {e:#}"),
    }
}

fn build_plan(
    yaml: &str,
    rhai: &str,
    logic_handle: &Arc<Mutex<Option<LogicHandle>>>,
) -> Result<Plan> {
    let cfg = Config::from_yaml(yaml).context("parsing on-disk config.yaml")?;
    let _ = cfg.validate(); // warnings already logged on receive
    let (plan, handle) = logic::evaluate(rhai, &cfg).context("evaluating Rhai logic")?;
    *logic_handle.lock().unwrap() = Some(handle);
    Ok(plan)
}

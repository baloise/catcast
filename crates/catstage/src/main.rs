//! catstage — CatCast fullscreen viewer.
//!
//! Tauri 2.x shell. Owns the fullscreen WebView2 window, hosts the
//! `about:catcast` URI scheme, and runs the scheduler/socks/Rhai engine in
//! the same tokio runtime Tauri starts.
//!
//! Window-focused hotkeys (Ctrl+Alt+M, F11, Esc) are caught by a JS keydown
//! listener inside `about:catcast` and forwarded to Rust via `invoke` — chosen
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
    /// then exit. (The about:catcast wizard does the same thing
    /// interactively.)
    #[arg(long)]
    install_autostart: bool,
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

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            about::get_snapshot,
            about::cmd_pause,
            about::cmd_play,
            about::cmd_manual,
            about::cmd_toggle_fullscreen,
            about::cmd_force_reconnect,
            about::cmd_reload_from_disk,
            about::cmd_install_autostart,
            about::cmd_nav,
            about::cmd_open_dir,
            about::cmd_hotkey_toggle_manual,
            about::cmd_hotkey_escape,
        ])
        .register_uri_scheme_protocol("about", |ctx, req| {
            about_scheme_response(ctx.app_handle(), req)
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = bootstrap(handle, stage_name, broker_url).await {
                    eprintln!("catstage: bootstrap failed: {e:#}");
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .map_err(|e| anyhow::anyhow!("tauri runtime: {e}"))
}

/// Wire up scheduler + socks once Tauri's runtime is alive. Mirrors the old
/// `run()` function but pushes scheduler events through Tauri instead of
/// blocking on ctrl-c at the end.
async fn bootstrap(app: tauri::AppHandle, name: String, broker_url: String) -> Result<()> {
    let initial_state = match persist::load_state()? {
        Some(s) => s,
        None => State::fresh(&name, env!("CARGO_PKG_VERSION")),
    };
    let config_yaml = persist::load_config_yaml()?;
    let logic_rhai = persist::load_logic_rhai()?;

    if config_yaml.is_none() && logic_rhai.is_none() {
        eprintln!(
            "catstage: no config/logic on disk — about:catcast splash with stage name '{name}'"
        );
    }

    let shared = Arc::new(Mutex::new(Shared {
        state: initial_state,
        config_yaml: config_yaml.clone(),
        logic_rhai: logic_rhai.clone(),
    }));

    let key = Arc::new(Key::from_psk(&name).context("deriving AEAD key from stage name")?);

    let logic_handle: Arc<Mutex<Option<LogicHandle>>> = Arc::new(Mutex::new(None));
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

    let out_tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<Message>>>> = Arc::new(Mutex::new(None));

    let events = Arc::new(SchedEvents {
        app: app.clone(),
        shared: Arc::clone(&shared),
        out: Arc::clone(&out_tx),
    });

    let sched_tx = scheduler::spawn(
        initial_plan,
        Some(Arc::clone(&logic_handle)),
        events.clone(),
    );

    let handler_shared = Arc::clone(&shared);
    let handler_sched = sched_tx.clone();
    let handler_logic = Arc::clone(&logic_handle);
    let handler_out = Arc::clone(&out_tx);
    let handler_app = app.clone();
    let handler: socks::InboundHandler = Arc::new(move |pt| {
        let pt_msg = pt.msg.clone();
        let shared = Arc::clone(&handler_shared);
        let sched = handler_sched.clone();
        let logic = Arc::clone(&handler_logic);
        let out = Arc::clone(&handler_out);
        let app = handler_app.clone();
        tokio::spawn(async move {
            dispatch(pt_msg, shared, sched, logic, out, app).await;
        });
    });

    let outbound = socks::spawn(broker_url.clone(), name.clone(), Arc::clone(&key), handler);
    *out_tx.lock().unwrap() = Some(outbound.clone());

    let snapshot = shared.lock().unwrap().state.clone();
    let _ = outbound.send(Message::State(snapshot)).await;

    // Register the Tauri context so commands and the URI scheme handler can
    // reach the scheduler / shared state.
    app.manage(TauriCtx {
        shared: Arc::clone(&shared),
        broker_url,
        sched_tx,
        logic_handle,
        out_tx,
        stage_name: name,
    });

    Ok(())
}

fn default_name() -> String {
    let raw = gethostname::gethostname();
    raw.to_string_lossy().to_string()
}

/// `about:catcast` URI scheme response. Anything else under `about:` 404s.
fn about_scheme_response(
    app: &tauri::AppHandle,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let uri = request.uri().to_string();
    // Tauri normalises `about:catcast` to `about://catcast/...` depending on
    // platform — match on the suffix instead of the full URI.
    if !uri.contains("catcast") {
        return tauri::http::Response::builder()
            .status(404)
            .header("Content-Type", "text/plain")
            .body(b"about: not found".to_vec())
            .unwrap();
    }
    let html = if let Some(ctx) = app.try_state::<TauriCtx>() {
        about::render_about_html(&about::make_snapshot(&ctx))
    } else {
        // setup() hasn't finished managing TauriCtx yet — serve the page with
        // a placeholder blob; the JS bootstraps itself via get_snapshot once
        // TauriCtx exists.
        about::render_about_html(&about::Snapshot {
            state: State::fresh("(loading)", env!("CARGO_PKG_VERSION")),
            broker_url: String::new(),
            stage_name: "(loading)".into(),
            files: Vec::new(),
            autostart_path: None,
        })
    };
    tauri::http::Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(html.into_bytes())
        .unwrap()
}

struct SchedEvents {
    app: tauri::AppHandle,
    shared: Arc<Mutex<Shared>>,
    out: Arc<Mutex<Option<tokio::sync::mpsc::Sender<Message>>>>,
}

impl scheduler::Events for SchedEvents {
    fn on_url_change(&self, url: &str) {
        let mut sh = self.shared.lock().unwrap();
        if sh.state.current_url.as_deref() == Some(url) {
            return;
        }
        sh.state.current_url = Some(url.to_string());
        sh.state.since = chrono::Utc::now().timestamp_millis();
        let snap = sh.state.clone();
        drop(sh);
        broadcast(&self.out, Message::State(snap));
        if let Err(e) = persist::save_state(&self.shared.lock().unwrap().state) {
            eprintln!("catstage: state save failed: {e:#}");
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
        let _ = window.navigate(parsed);
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
    match msg {
        Message::Pause => {
            let _ = sched.send(Cmd::Pause).await;
        }
        Message::Play => {
            let _ = sched.send(Cmd::Play).await;
        }
        Message::Nav { url } => {
            let _ = sched.send(Cmd::Nav(url)).await;
        }
        Message::NavTimed { url, duration_secs } => {
            let _ = sched
                .send(Cmd::TimedNav {
                    url,
                    secs: duration_secs,
                })
                .await;
        }
        Message::Manual { on } => {
            let _ = sched.send(Cmd::Manual(on)).await;
        }
        Message::SetConfig { yaml } => match Config::from_yaml(&yaml) {
            Ok(cfg) => {
                match cfg.validate() {
                    Ok(warnings) => {
                        for w in &warnings {
                            eprintln!("catstage: config warning: {}", w.0);
                        }
                    }
                    Err(e) => {
                        eprintln!("catstage: config validation error: {e}");
                        return;
                    }
                }
                if let Err(e) = persist::save_config_yaml(&yaml) {
                    eprintln!("catstage: persist config failed: {e:#}");
                }
                {
                    let mut sh = shared.lock().unwrap();
                    sh.config_yaml = Some(yaml.clone());
                    sh.state.has_config = true;
                }
                rebuild_and_send(&shared, &sched, &logic_handle).await;
                about::emit_state(&app, WINDOW_LABEL);
            }
            Err(e) => eprintln!("catstage: bad config YAML: {e}"),
        },
        Message::SetLogic { rhai } => {
            if let Err(e) = persist::save_logic_rhai(&rhai) {
                eprintln!("catstage: persist logic failed: {e:#}");
            }
            {
                let mut sh = shared.lock().unwrap();
                sh.logic_rhai = Some(rhai.clone());
                sh.state.has_logic = true;
            }
            rebuild_and_send(&shared, &sched, &logic_handle).await;
            about::emit_state(&app, WINDOW_LABEL);
        }
        Message::GetState => {
            let snap = shared.lock().unwrap().state.clone();
            broadcast(&out, Message::State(snap));
        }
        Message::State(_) => {
            // Other stages on the same room might also emit State — ignore.
        }
    }
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

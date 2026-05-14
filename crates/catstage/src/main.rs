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

fn bundled_about_fallback() -> tauri::Url {
    #[cfg(target_os = "windows")]
    let raw = "http://tauri.localhost/";
    #[cfg(not(target_os = "windows"))]
    let raw = "tauri://localhost/";

    tauri::Url::parse(raw).expect("valid bundled about fallback URL")
}

fn is_blank_url(url: &tauri::Url) -> bool {
    url.as_str().eq_ignore_ascii_case("about:blank")
}

fn is_bundled_about_url(url: &tauri::Url) -> bool {
    if is_blank_url(url) {
        return false;
    }
    #[cfg(target_os = "windows")]
    {
        return url.scheme() == "http" && url.domain() == Some("tauri.localhost");
    }
    #[cfg(not(target_os = "windows"))]
    {
        return url.scheme() == "tauri";
    }
}

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
        r##"
(function () {{
  if (window.__catcastEscapeInstalled) return;
  window.__catcastEscapeInstalled = true;
  const ABOUT_URL = "{about_url}";
  document.addEventListener("keydown", (ev) => {{
    if (ev.key === "F1") {{
      ev.preventDefault();
            // Mark this as an operator-initiated "go to about and pause" action.
            // Startup may also render about briefly; that path must not force Idle.
            try {{
                const u = new URL(ABOUT_URL);
                u.hash = "idle";
                window.location.href = u.toString();
            }} catch (_) {{
                window.location.href = ABOUT_URL + "#idle";
            }}
    }}
  }}, true);
}})();
"##
    )
}

#[derive(Parser, Clone)]
#[command(name = "catstage", version, about = "CatCast fullscreen viewer")]
struct Args {
    /// WebSocket URL of the CatSocks broker, e.g. wss://.../r/<room>.
    /// Required unless `--offline` or `--url` is given.
    #[arg(
        long,
        required_unless_present_any = ["offline", "url"],
        conflicts_with_all = ["offline", "url"],
    )]
    socks: Option<String>,

    /// Run from on-disk config.yaml + logic.rhai without connecting to a
    /// broker. Errors if neither file exists.
    #[arg(long, conflicts_with = "url")]
    offline: bool,

    /// Display a single URL forever, with no broker and no config. Bypasses
    /// rotation; the about page is reachable via F1 for diagnostics.
    #[arg(long, value_name = "URL")]
    url: Option<String>,

    /// Stage name. Defaults to the machine's hostname.
    #[arg(long)]
    name: Option<String>,

    /// Monitor index to use on startup (1-based: 1 = first monitor).
    /// Invalid values fall back to the primary monitor.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    screen: Option<u32>,

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

/// Run-mode dispatch derived from `Args`. Online is the historical default;
/// Offline and SingleUrl skip the broker entirely.
#[derive(Debug, Clone)]
enum RunMode {
    Online { socks: String },
    Offline,
    SingleUrl { url: String },
}

impl RunMode {
    fn is_offline(&self) -> bool {
        !matches!(self, RunMode::Online { .. })
    }

    /// Human-readable broker_url for the about-page snapshot. Online stages
    /// expose the real broker URL; offline modes show a sentinel so the page
    /// makes clear there's no live connection.
    fn broker_url_display(&self) -> String {
        match self {
            RunMode::Online { socks } => socks.clone(),
            RunMode::Offline | RunMode::SingleUrl { .. } => "(offline)".into(),
        }
    }
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
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls ring crypto provider");

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
    let screen = args.screen;

    if args.install_autostart {
        // Autostart shortcut bakes a `--socks` URL into the .lnk command line;
        // it has no offline analogue today. Clap already requires `--socks`
        // unless `--offline`/`--url` is set, so reject the unsupported combo
        // here rather than producing a broken shortcut.
        let socks = args.socks.clone().ok_or_else(|| {
            anyhow::anyhow!("--install-autostart requires --socks (offline autostart not supported)")
        })?;
        if let Some(path) = autostart::install(&autostart::AutostartArgs {
            socks,
            name: args.name.clone(),
            screen,
        })? {
            eprintln!("catstage: autostart installed at {}", path.display());
        }
        return Ok(());
    }

    let run_mode = match (args.offline, args.url.clone(), args.socks.clone()) {
        (true, _, _) => RunMode::Offline,
        (false, Some(url), _) => RunMode::SingleUrl { url },
        (false, None, Some(socks)) => RunMode::Online { socks },
        // clap's `required_unless_present_any` guarantees we never reach this.
        (false, None, None) => unreachable!("clap should have required --socks"),
    };

    // Pre-flight checks happen *before* `tauri::Builder` so a fatal config
    // problem doesn't paint a window we're about to tear down.
    match &run_mode {
        RunMode::Offline => {
            let cfg = persist::config_path()?;
            let logic = persist::logic_path()?;
            if !cfg.exists() && !logic.exists() {
                eprintln!(
                    "catstage: --offline requires on-disk config.yaml or logic.rhai (looked at {} and {}). \
                     Push one with `catc config import` from an online stage, or run without --offline.",
                    cfg.display(),
                    logic.display(),
                );
                std::process::exit(1);
            }
        }
        RunMode::SingleUrl { url } => {
            if let Err(e) = tauri::Url::parse(url) {
                eprintln!("catstage: --url value is not a valid URL: {e}");
                std::process::exit(1);
            }
        }
        RunMode::Online { .. } => {}
    }

    eprintln!(
        "😼🎬 catstage v{} starting as {name}",
        env!("CARGO_PKG_VERSION"),
    );

    let stage_name = name.clone();
    let open_devtools = args.devtools;
    let setup_run_mode = run_mode.clone();

    tauri::Builder::default()
        // Install a tiny F1 → back-to-about handler on every page load.
        // It's the only cross-origin shortcut we ship; everything else
        // operators do via `catc`. See `build_escape_script` for the
        // (intentionally minimal) JS body.
        .on_page_load(|webview, payload| {
            if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                let app = webview.app_handle();
                if let Some(state) = app.try_state::<about::AboutUrl>() {
                    if let Ok(url) = webview.url() {
                        if is_bundled_about_url(&url) {
                            state.set(url);
                        }
                    }
                }
                let about_url = app
                    .try_state::<about::AboutUrl>()
                    .map(|s| s.get().to_string())
                    .unwrap_or_else(|| bundled_about_fallback().to_string());
                let _ = webview.eval(build_escape_script(&about_url));
            }
        })
        .invoke_handler(tauri::generate_handler![
            about::get_snapshot,
            about::cmd_log,
            about::enter_idle,
            about::enter_play,
            about::request_exit,
        ])
        .setup(move |app| {
            // Seed with a deterministic about URL so startup never records
            // `about:blank` as the canonical return target.
            app.manage(about::AboutUrl::new(bundled_about_fallback()));

            // Force fullscreen at runtime in addition to the config-time
            // request. WSLg / Wayland in particular tend to ignore the
            // config-time `fullscreen: true` because the compositor isn't
            // ready when the window is created.
            //
            // If setup-time URL probing already sees the bundled about page,
            // store it. Ignore `about:blank` and non-bundled URLs.
            if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
                if let Some(screen_num) = screen {
                    position_window_on_screen(&window, screen_num);
                }
                let _ = window.set_fullscreen(true);
                if let Ok(url) = window.url() {
                    if is_bundled_about_url(&url) {
                        if let Some(state) = app.try_state::<about::AboutUrl>() {
                            state.set(url);
                        }
                    }
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
                broker_url: setup_run_mode.broker_url_display(),
                offline: setup_run_mode.is_offline(),
                sched_tx: sched_tx.clone(),
                stage_name: stage_name.clone(),
                screen,
            });

            let handle = app.handle().clone();
            let bootstrap_name = stage_name.clone();
            let bootstrap_mode = setup_run_mode.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = bootstrap(
                    handle,
                    bootstrap_name,
                    bootstrap_mode,
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
///
/// `mode` selects the startup path:
/// - `Online`  — historical behavior (load disk, build plan, connect broker).
/// - `Offline` — load disk + build plan as usual but skip the broker entirely.
/// - `SingleUrl` — synthesize a one-entry plan, skip persistence + broker.
#[allow(clippy::too_many_arguments)]
async fn bootstrap(
    app: tauri::AppHandle,
    name: String,
    mode: RunMode,
    shared: Arc<Mutex<Shared>>,
    logic_handle: Arc<Mutex<Option<LogicHandle>>>,
    out_tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<Message>>>>,
    socks_abort: Arc<tokio::sync::Notify>,
    sched_rx: tokio::sync::mpsc::Receiver<scheduler::Cmd>,
) -> Result<()> {
    let initial_plan = match &mode {
        RunMode::Online { .. } | RunMode::Offline => {
            // Hydrate Shared from disk. Online and Offline both consume the
            // same persisted config.yaml / logic.rhai / state.json.
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

            match (&config_yaml, &logic_rhai) {
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
            }
        }
        RunMode::SingleUrl { url } => {
            // Synthesize a one-entry rotation with effectively infinite slot
            // duration so the scheduler never advances past the single URL.
            // No persistence read/write — single-URL kiosks are ephemeral.
            {
                let mut sh = shared.lock().unwrap();
                sh.state.current_url = Some(url.clone());
            }
            Plan {
                default_secs: 1,
                rotation: vec![crate::logic::RotationItem {
                    url: url.clone(),
                    secs: u64::MAX,
                }],
                cron: vec![],
                one_shot: vec![],
            }
        }
    };

    let events = Arc::new(SchedEvents {
        app: app.clone(),
        shared: Arc::clone(&shared),
        out: Arc::clone(&out_tx),
    });

    // Scheduler always boots in Playing; persisted mode is intentionally
    // ignored. A stage left in Paused or Idle when it last exited recovers
    // cleanly. The persisted mode in state.json mostly reflects the most-
    // recent operator action — useful for the about page on next launch but
    // not as a runtime resume target.
    {
        let mut sh = shared.lock().unwrap();
        sh.state.mode = catcast_core::Mode::Playing;
    }
    scheduler::spawn_with_rx(
        sched_rx,
        initial_plan,
        Some(Arc::clone(&logic_handle)),
        events.clone(),
    );

    let socks_url = match &mode {
        RunMode::Online { socks } => socks.clone(),
        // Offline and SingleUrl skip the broker entirely. Leaving `out_tx` as
        // `None` makes `broadcast()` a no-op so dispatch-path State emits
        // (none reach us since no inbound socks frames either) are dropped.
        RunMode::Offline | RunMode::SingleUrl { .. } => {
            let _ = socks_abort; // suppress unused-var warning in offline path
            return Ok(());
        }
    };

    let key = Arc::new(Key::from_psk(&name).context("deriving AEAD key from stage name")?);

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

    let outbound = socks::spawn(
        socks_url,
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

fn position_window_on_screen(window: &tauri::WebviewWindow, screen_num: u32) {
    let requested_idx = screen_num.saturating_sub(1) as usize;

    let target = match window.available_monitors() {
        Ok(monitors) => {
            if let Some(monitor) = monitors.get(requested_idx) {
                Some(monitor.clone())
            } else {
                eprintln!(
                    "catstage: --screen {} unavailable ({} monitor(s)); falling back to primary",
                    screen_num,
                    monitors.len()
                );
                match window.primary_monitor() {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("catstage: primary_monitor() failed: {e}");
                        None
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("catstage: available_monitors() failed: {e}");
            match window.primary_monitor() {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("catstage: primary_monitor() failed: {e}");
                    None
                }
            }
        }
    };

    if let Some(monitor) = target {
        let _ = window.set_fullscreen(false);
        if let Err(e) = window.set_position(monitor.position().to_owned()) {
            eprintln!("catstage: set_position() failed: {e}");
        }
    }
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
    fn on_mode_change(&self, mode: catcast_core::Mode) {
        let mut sh = self.shared.lock().unwrap();
        sh.state.mode = mode;
        sh.state.since = chrono::Utc::now().timestamp_millis();
        let snap = sh.state.clone();
        drop(sh);
        broadcast(&self.out, Message::State(snap.clone()));
        if let Err(e) = persist::save_state(&snap) {
            eprintln!("catstage: state save failed: {e:#}");
        }
        // Idle transitions need the kiosk on the about page; transitions
        // *off* idle re-announce the rotation URL via the scheduler's
        // Cmd::Play handler. We only have to handle the Idle direction
        // here because non-idle modes don't dictate a particular URL.
        if mode == catcast_core::Mode::Idle {
            navigate_to_about(&self.app);
        }
        about::emit_state(&self.app, WINDOW_LABEL);
    }
}

/// Send the kiosk window back to the bundled about page. Used by mode
/// transitions into `Idle`, and by the `NavAbout` dispatch arm (which also
/// emits a Reply). Bypasses `steer_webview`'s gating — Idle *is* the gate
/// now.
fn navigate_to_about(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    let mut target = app
        .try_state::<about::AboutUrl>()
        .map(|s| s.get())
        .unwrap_or_else(bundled_about_fallback);
    if !is_bundled_about_url(&target) {
        target = bundled_about_fallback();
        if let Some(state) = app.try_state::<about::AboutUrl>() {
            state.set(target.clone());
        }
    }
    if let Err(e) = window.navigate(target) {
        eprintln!("catstage: navigate(about) failed: {e}");
    }
}

/// Drive the kiosk webview to `url`. Skipped when the stage is `Idle`
/// (kiosk should stay on about) so rotation index advances under the hood
/// don't yank the operator off the admin page.
fn steer_webview(app: &tauri::AppHandle, url: &str) {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    let Some(ctx) = app.try_state::<TauriCtx>() else {
        return;
    };
    if ctx.shared.lock().unwrap().state.mode == catcast_core::Mode::Idle {
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
            // Pausing from Idle is a no-op trap — there's no current URL to
            // freeze on. Surface the real reason rather than silently
            // accepting and looking stuck.
            if shared.lock().unwrap().state.mode == catcast_core::Mode::Idle {
                Some(Err(
                    "nothing to pause — stage is on the about page (run `catc play`)".into(),
                ))
            } else {
                let _ = sched.send(Cmd::Pause).await;
                Some(Ok(decorate("paused", &shared)))
            }
        }
        Message::Play => {
            let _ = sched.send(Cmd::Play).await;
            Some(Ok(decorate("playing", &shared)))
        }
        Message::Back => Some(step_rotation(&sched, -1, "back").await),
        Message::Forward => Some(step_rotation(&sched, 1, "forward").await),
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
        Message::GetConfig => {
            let yaml = shared.lock().unwrap().config_yaml.clone();
            broadcast(&out, Message::ConfigData { yaml });
            None
        }
        Message::GetLogic => {
            let rhai = shared.lock().unwrap().logic_rhai.clone();
            broadcast(&out, Message::LogicData { rhai });
            None
        }
        Message::NavAbout => {
            // The scheduler flips mode → Idle and the on_mode_change handler
            // does the actual `window.navigate(about_url)`. Dispatch just
            // needs to send the command and craft the reply.
            let _ = sched.send(Cmd::Idle).await;
            Some(Ok("on about".into()))
        }
        Message::AutostartInstall => Some(match app.try_state::<crate::about::TauriCtx>() {
            Some(ctx) => {
                let socks = ctx.inner().broker_url.clone();
                let name = Some(ctx.inner().stage_name.clone());
                let screen = ctx.inner().screen;
                let res = match autostart::install(&autostart::AutostartArgs {
                    socks,
                    name,
                    screen,
                }) {
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
            Some(w) => {
                // None = toggle: read the window's current state and flip it.
                let resolved: Result<bool, String> = match on {
                    Some(v) => Ok(v),
                    None => w
                        .is_fullscreen()
                        .map(|cur| !cur)
                        .map_err(|e| format!("is_fullscreen() failed: {e}")),
                };
                match resolved {
                    Ok(new_on) => match w.set_fullscreen(new_on) {
                        Ok(()) => Ok(if new_on {
                            "fullscreen on".into()
                        } else {
                            "fullscreen off".into()
                        }),
                        Err(e) => Err(format!("set_fullscreen({new_on}) failed: {e}")),
                    },
                    Err(e) => Err(e),
                }
            }
            None => Err("no kiosk window".into()),
        }),
        Message::DevTools { on } => Some(match app.get_webview_window(WINDOW_LABEL) {
            Some(w) => {
                let new_on = on.unwrap_or_else(|| !w.is_devtools_open());
                if new_on {
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
        Message::State(_)
        | Message::Reply { .. }
        | Message::ConfigData { .. }
        | Message::LogicData { .. } => None,
    };

    if let Some(res) = outcome {
        let (ok, message) = match res {
            Ok(m) => (true, m),
            Err(m) => (false, m),
        };
        broadcast(&out, Message::Reply { ok, message });
    }
}

/// Append a load-state hint to a Pause/Play/Manual-off reply so the operator
/// knows whether the scheduler actually has something to do. Without this,
/// `catc play` against a fresh stage returns "playing" even though there's
/// nothing on screen — true at the flag level, useless in practice.
fn decorate(verb: &str, shared: &Arc<Mutex<Shared>>) -> String {
    let sh = shared.lock().unwrap();
    let hint = match (sh.state.has_config, sh.state.has_logic) {
        (true, true) => sh.state.current_url.as_ref().map(|u| format!("on {u}")),
        (false, false) => Some(
            "no config or logic loaded — push them with `catc config import` / `catc logic import`"
                .into(),
        ),
        (false, true) => Some("no config loaded — push one with `catc config import`".into()),
        (true, false) => Some("no logic loaded — push one with `catc logic import`".into()),
    };
    match hint {
        Some(h) => format!("{verb} — {h}"),
        None => verb.into(),
    }
}

/// Send a `Cmd::Step(delta)` to the scheduler and wait for the oneshot reply
/// carrying the URL of the new rotation slot. Used by the Back / Forward
/// dispatch arms. `verb` is "back" / "forward" — used to build the reply
/// text. On empty rotation we return an error reply so the operator sees the
/// real reason ("rotation is empty — push a config first") instead of a
/// silent success.
async fn step_rotation(
    sched: &tokio::sync::mpsc::Sender<scheduler::Cmd>,
    delta: i32,
    verb: &str,
) -> Result<String, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if sched
        .send(scheduler::Cmd::Step { delta, reply: tx })
        .await
        .is_err()
    {
        return Err("scheduler is gone".into());
    }
    match rx.await {
        Ok(Some(url)) => Ok(format!("{verb} — on {url}")),
        Ok(None) => Err(
            "rotation is empty — push a config + logic with `catc config import` / `catc logic import`"
                .into(),
        ),
        Err(_) => Err("scheduler dropped reply before sending".into()),
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
    let rebuild_err = rebuild_and_send(shared, sched, logic_handle).await;
    about::emit_state(app, WINDOW_LABEL);
    let head = if warnings.is_empty() {
        "config saved".to_string()
    } else {
        format!(
            "config saved ({} warning{})",
            warnings.len(),
            if warnings.len() == 1 { "" } else { "s" }
        )
    };
    Ok(match rebuild_err {
        None => head,
        Some(e) => format!("{head}, but plan rebuild failed: {e}"),
    })
}

async fn set_logic(
    rhai: String,
    shared: &Arc<Mutex<Shared>>,
    sched: &tokio::sync::mpsc::Sender<scheduler::Cmd>,
    logic_handle: &Arc<Mutex<Option<LogicHandle>>>,
    app: &tauri::AppHandle,
) -> Result<String, String> {
    // Syntax-check the script *before* we persist or flip `has_logic`. This
    // catches "Expression exceeds maximum complexity" and other parse errors
    // upfront, so the operator sees the failure immediately instead of the
    // stage silently running on an empty plan.
    logic::compile_check(&rhai).map_err(|e| format!("bad rhai: {e:#}"))?;
    persist::save_logic_rhai(&rhai).map_err(|e| format!("persist logic failed: {e:#}"))?;
    {
        let mut sh = shared.lock().unwrap();
        sh.logic_rhai = Some(rhai);
        sh.state.has_logic = true;
    }
    let rebuild_err = rebuild_and_send(shared, sched, logic_handle).await;
    about::emit_state(app, WINDOW_LABEL);
    Ok(match rebuild_err {
        None => "logic saved".into(),
        Some(e) => format!("logic saved, but plan rebuild failed: {e}"),
    })
}

/// Try to (re)build the plan from current `config_yaml` + `logic_rhai` and
/// hand it to the scheduler. Returns `Some(error)` if both pieces are present
/// but evaluation failed (e.g. Rhai runtime error against a real cfg);
/// returns `None` either way when the plan was either rebuilt cleanly or one
/// of the pieces is missing (nothing to do yet). The caller decides whether
/// the runtime error should surface in their reply.
async fn rebuild_and_send(
    shared: &Arc<Mutex<Shared>>,
    sched: &tokio::sync::mpsc::Sender<scheduler::Cmd>,
    logic_handle: &Arc<Mutex<Option<LogicHandle>>>,
) -> Option<String> {
    let (yaml, rhai) = {
        let sh = shared.lock().unwrap();
        (sh.config_yaml.clone(), sh.logic_rhai.clone())
    };
    let (Some(yaml), Some(rhai)) = (yaml, rhai) else {
        return None;
    };
    match build_plan(&yaml, &rhai, logic_handle) {
        Ok(plan) => {
            let _ = sched.send(scheduler::Cmd::Rebuild(plan)).await;
            None
        }
        Err(e) => {
            let msg = format!("{e:#}");
            eprintln!("catstage: plan rebuild failed: {msg}");
            Some(msg)
        }
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

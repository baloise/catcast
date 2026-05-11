//! catstage — CatCast fullscreen viewer (headless skeleton).
//!
//! This binary is intentionally headless for now: the Tauri/WebView2 shell,
//! the `about:catcast` URI handler, and the global Ctrl+Alt+M hotkey are
//! deferred to a follow-up. The rest of the runtime — persistence, socks
//! client, Rhai logic engine, rotation scheduler — runs end-to-end on Linux
//! so we can validate the protocol without standing up a WebKitGTK toolchain.
//!
//! TODO(tauri): wrap this in a Tauri 2.x app, render `about:catcast` and
//! configured URLs in a fullscreen WebView2 window, replace each "would …"
//! stderr line with a real `tauri::Manager::emit` call.
//! TODO(hotkey): register the global Ctrl+Alt+M hotkey to toggle manual mode.

use anyhow::{Context, Result};
use catcast_core::{Config, State};
use catcast_proto::{Key, Message};
use clap::Parser;
use std::sync::{Arc, Mutex};

mod autostart;
mod logic;
mod persist;
mod scheduler;
mod socks;

use crate::logic::{LogicHandle, Plan};

#[derive(Parser)]
#[command(name = "catstage", version, about = "CatCast fullscreen viewer")]
struct Args {
    /// WebSocket URL of the CatSocks broker, e.g. wss://.../r/<room>.
    #[arg(long)]
    socks: String,

    /// Stage name. Defaults to the machine's hostname.
    #[arg(long)]
    name: Option<String>,

    /// Drop a Startup-folder shortcut so this stage launches at login, then exit.
    #[arg(long)]
    install_autostart: bool,
}

/// Everything the dispatch path mutates. Held behind one mutex so the socks
/// handler doesn't need a separate lock per field.
struct Shared {
    state: State,
    config_yaml: Option<String>,
    logic_rhai: Option<String>,
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

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(args, name))
}

async fn run(args: Args, name: String) -> Result<()> {
    eprintln!(
        "catstage v{} ready as {name}; about:catcast would show the splash",
        env!("CARGO_PKG_VERSION"),
    );

    // Load disk state.
    let initial_state = match persist::load_state()? {
        Some(s) => s,
        None => State::fresh(&name, env!("CARGO_PKG_VERSION")),
    };
    let config_yaml = persist::load_config_yaml()?;
    let logic_rhai = persist::load_logic_rhai()?;

    if config_yaml.is_none() && logic_rhai.is_none() {
        eprintln!(
            "catstage: no config/logic on disk — would show stage name '{name}' big and centred"
        );
    }

    let shared = Arc::new(Mutex::new(Shared {
        state: initial_state,
        config_yaml: config_yaml.clone(),
        logic_rhai: logic_rhai.clone(),
    }));

    // Pre-derive AEAD key for this stage's PSK.
    let key = Arc::new(Key::from_psk(&name).context("deriving AEAD key from stage name")?);

    // Build the initial plan from disk state (if any).
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

    // Wire scheduler events to update shared state and broadcast.
    // We need the outbound socks sender, but the scheduler is spawned *before*
    // the socks client; punt via an Arc<Mutex<Option<Sender>>>.
    let out_tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<Message>>>> = Arc::new(Mutex::new(None));
    let events = Arc::new(SchedEvents {
        shared: Arc::clone(&shared),
        out: Arc::clone(&out_tx),
    });

    let sched_tx = scheduler::spawn(
        initial_plan,
        Some(Arc::clone(&logic_handle)),
        events.clone(),
    );

    // Build the socks inbound handler.
    let handler_shared = Arc::clone(&shared);
    let handler_sched = sched_tx.clone();
    let handler_logic = Arc::clone(&logic_handle);
    let handler_out = Arc::clone(&out_tx);
    let handler: socks::InboundHandler = Arc::new(move |pt| {
        let pt_msg = pt.msg.clone();
        let shared = Arc::clone(&handler_shared);
        let sched = handler_sched.clone();
        let logic = Arc::clone(&handler_logic);
        let out = Arc::clone(&handler_out);
        tokio::spawn(async move {
            dispatch(pt_msg, shared, sched, logic, out).await;
        });
    });

    let outbound = socks::spawn(args.socks.clone(), name.clone(), Arc::clone(&key), handler);
    *out_tx.lock().unwrap() = Some(outbound.clone());

    // Send an unsolicited State on connect so listening CLIs catch up.
    // The socks task queues this until the connection is up.
    let snapshot = shared.lock().unwrap().state.clone();
    let _ = outbound.send(Message::State(snapshot)).await;

    // Keep the runtime alive forever; the scheduler + socks tasks own the
    // actual work.
    tokio::signal::ctrl_c().await.ok();
    eprintln!("catstage: shutting down");
    Ok(())
}

fn default_name() -> String {
    let raw = gethostname::gethostname();
    raw.to_string_lossy().to_string()
}

struct SchedEvents {
    shared: Arc<Mutex<Shared>>,
    out: Arc<Mutex<Option<tokio::sync::mpsc::Sender<Message>>>>,
}

impl scheduler::Events for SchedEvents {
    fn on_url_change(&self, url: &str) {
        eprintln!("catstage: would navigate to {url}");
        let mut sh = self.shared.lock().unwrap();
        if sh.state.current_url.as_deref() != Some(url) {
            sh.state.current_url = Some(url.to_string());
            sh.state.since = chrono::Utc::now().timestamp_millis();
            let snap = sh.state.clone();
            drop(sh);
            broadcast(&self.out, Message::State(snap));
            // Persist asynchronously-ish — fire-and-forget.
            if let Err(e) = persist::save_state(&self.shared.lock().unwrap().state) {
                eprintln!("catstage: state save failed: {e:#}");
            }
        }
    }
    fn on_pause_change(&self, paused: bool) {
        let mut sh = self.shared.lock().unwrap();
        sh.state.paused = paused;
        sh.state.since = chrono::Utc::now().timestamp_millis();
        let snap = sh.state.clone();
        drop(sh);
        broadcast(&self.out, Message::State(snap));
    }
    fn on_manual_change(&self, manual: bool) {
        eprintln!(
            "catstage: manual mode {}",
            if manual { "on" } else { "off" }
        );
        let mut sh = self.shared.lock().unwrap();
        sh.state.manual = manual;
        sh.state.since = chrono::Utc::now().timestamp_millis();
        let snap = sh.state.clone();
        drop(sh);
        broadcast(&self.out, Message::State(snap));
    }
}

fn broadcast(out: &Arc<Mutex<Option<tokio::sync::mpsc::Sender<Message>>>>, msg: Message) {
    let Some(tx) = out.lock().unwrap().clone() else {
        return;
    };
    // Don't block — drop if the channel is full or the socks task is gone.
    let _ = tx.try_send(msg);
}

async fn dispatch(
    msg: Message,
    shared: Arc<Mutex<Shared>>,
    sched: tokio::sync::mpsc::Sender<scheduler::Cmd>,
    logic_handle: Arc<Mutex<Option<LogicHandle>>>,
    out: Arc<Mutex<Option<tokio::sync::mpsc::Sender<Message>>>>,
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

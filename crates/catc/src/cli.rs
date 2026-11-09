use crate::cfg::{Aliases, CatcConfig, Stage};
use crate::default_logic::{self, LogicStatus, DEFAULT_LOGIC};
use crate::net::{keys_for, Broker};
use crate::release;
use anyhow::{anyhow, bail, Context, Result};
use catcast_core::Config;
use catcast_proto::{Message, TraceKind};
use clap::{Args, Parser, Subcommand};
use std::collections::HashMap;
use std::fs;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "catc", version, about = "😼 CatCast CLI")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Write `catc.toml` with the broker URL.
    Init {
        #[arg(long)]
        socks: String,
    },
    /// Manage the local stage registry (catc.toml).
    Stage {
        #[command(subcommand)]
        sub: StageCmd,
    },
    /// Broker, stages and their mirrors in one table: up/down, last seen,
    /// version, uptime, broker link, frame age, mirror cache, where each
    /// kiosk is. Probes every registered stage and every mirror a relay
    /// names.
    Status,

    /// Freeze rotation on the current URL.
    Pause(Targets),
    /// Resume rotation.
    Play(Targets),
    /// Step the rotation one slot backward.
    Back(Targets),
    /// Step the rotation one slot forward.
    Forward(Targets),
    /// Navigate to a URL. With `--for <dur>`, auto-resumes rotation after.
    Nav {
        url: String,
        /// Optional duration like `5m`, `30s`. If set, rotation resumes after.
        #[arg(long)]
        r#for: Option<String>,
        #[command(flatten)]
        targets: Targets,
    },
    /// Edit, import or export the rotation config (YAML).
    Config {
        #[command(subcommand)]
        sub: ConfigCmd,
    },
    /// Edit, import or export the rotation logic (Rhai).
    Logic {
        #[command(subcommand)]
        sub: LogicCmd,
    },
    /// Import or export config + logic together.
    State {
        #[command(subcommand)]
        sub: StateCmd,
    },

    /// Define and run command aliases.
    Alias {
        #[command(subcommand)]
        sub: AliasCmd,
    },

    /// Install or remove the Startup-folder shortcut.
    Autostart {
        #[command(subcommand)]
        sub: AutostartCmd,
    },
    /// Set the kiosk's fullscreen state. Omit `on|off` to toggle.
    Fullscreen {
        on_off: Option<OnOff>,
        #[command(flatten)]
        targets: Targets,
    },
    /// Open the WebKit/WebView2 inspector. Omit `on|off` to toggle.
    Devtools {
        on_off: Option<OnOff>,
        #[command(flatten)]
        targets: Targets,
    },
    /// Park the kiosk on its about page (equivalent to F1 at the screen). Stops rotation.
    About(Targets),
    /// Show where the kiosk webview actually is and its recent navigation
    /// hops — redirects to proxy logins / SSO included (needs exactly one target).
    Trace(Targets),
    /// Cleanly stop the stage process.
    Shutdown(Targets),
    /// Pair a stage inside the network (relay) with a stage outside it
    /// (mirror, started with `catstage --mirror`).
    Relay {
        #[command(subcommand)]
        sub: RelayCmd,
    },
    /// Print catc's version and the latest published release.
    Version,
    /// Update this catc binary in place to a release (verifies SHA-256 and
    /// the Ed25519 signature before swapping).
    SelfUpdate {
        /// Release to install, e.g. v0.3.0. Default: the latest release.
        #[arg(long)]
        version: Option<String>,
        /// Reinstall the same version, or downgrade.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub enum AutostartCmd {
    /// Install a Startup-folder shortcut on the stage using its current
    /// --socks / --name args.
    Install {
        #[command(flatten)]
        targets: Targets,
    },
    /// Remove the Startup-folder shortcut if present.
    Uninstall {
        #[command(flatten)]
        targets: Targets,
    },
}

#[derive(Args)]
pub struct StageList {
    /// One or more stage names. Mutually exclusive with `--all`.
    pub names: Vec<String>,
    /// Apply to every registered stage.
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Clone)]
pub struct Targets {
    /// Target stage by name (repeatable). If neither this nor `--all` is
    /// given, the command goes to every active stage except mirrors
    /// (`stage update` includes them).
    #[arg(long = "name")]
    pub names: Vec<String>,
    /// Target every registered stage, regardless of active flag (mirrors
    /// only for `stage update`).
    #[arg(long)]
    pub all: bool,
}

#[derive(Clone, clap::ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// Push a config YAML file to the target stage(s).
    Import {
        file: String,
        #[command(flatten)]
        targets: Targets,
    },
    /// Print the active config to stdout (needs exactly one target).
    Export {
        #[command(flatten)]
        targets: Targets,
    },
    /// Download the config into a working copy next to catc.toml, open it in
    /// $VISUAL / $EDITOR, then validate, diff and offer to push it back
    /// (needs exactly one target).
    Edit(EditOpts),
}

#[derive(Subcommand)]
pub enum LogicCmd {
    /// Push a Rhai logic file to the target stage(s).
    /// If <FILE> is omitted, the built-in default logic is used.
    Import {
        file: Option<String>,
        #[command(flatten)]
        targets: Targets,
    },
    /// Print the active logic to stdout (needs exactly one target).
    Export {
        #[command(flatten)]
        targets: Targets,
    },
    /// Download the logic into a working copy next to catc.toml, open it in
    /// $VISUAL / $EDITOR, then diff and offer to push it back (needs exactly
    /// one target).
    Edit(EditOpts),
}

#[derive(Args)]
pub struct EditOpts {
    /// Don't wait for the editor to close: watch the file and offer a push
    /// on every save until you type `q`. For GUI editors that return at once
    /// (`code`, Notepad++); a terminal editor would fight over the terminal.
    #[arg(long)]
    pub watch: bool,
    #[command(flatten)]
    pub targets: Targets,
}

#[derive(Subcommand)]
pub enum StateCmd {
    /// Push `config.yaml` and `logic.rhai` from <dir> to the target stage(s).
    Import {
        file: String,
        #[command(flatten)]
        targets: Targets,
    },
    /// Write the active config + logic into <dir> (needs exactly one target).
    Export {
        /// Directory to write `config.yaml` and `logic.rhai` into. Created
        /// if it doesn't exist. Symmetric with `state import <dir>`.
        dir: String,
        #[command(flatten)]
        targets: Targets,
    },
}

#[derive(Subcommand)]
pub enum StageCmd {
    /// Register a stage by name (read it off the stage's catcast://about page).
    Add { name: String },
    /// Forget a stage.
    Remove { name: String },
    /// Mark stages active.
    Activate(StageList),
    /// Mark stages inactive.
    Deactivate(StageList),
    /// List configured stages.
    List {
        /// Send a GetState to each and report which respond, with the
        /// version each one runs and whether a newer release exists.
        #[arg(long)]
        probe: bool,
    },
    /// Update the catstage binary on the target stage(s) to a release.
    ///
    /// Each stage downloads the asset (through `update_proxy` from
    /// catc.toml when set), verifies the SHA-256 published with the
    /// release, swaps the binary in beside itself, relaunches with its own
    /// arguments and exits. Stages that are offline, already current, or
    /// too old to understand the command are skipped and reported.
    Update(UpdateOpts),
}

#[derive(Args)]
pub struct UpdateOpts {
    /// Release to install, e.g. `v0.2.0`. Default: the latest release.
    #[arg(long)]
    pub version: Option<String>,
    /// Also reinstall the same version or downgrade.
    #[arg(long)]
    pub force: bool,
    /// How long to wait for the stages to come back up.
    #[arg(long, default_value = "180")]
    pub timeout_secs: u64,
    /// Fetch the binary from this URL instead of the release asset
    /// (needs --sha256; one platform at a time). Base64 text unless --raw.
    #[arg(long, requires = "sha256")]
    pub url: Option<String>,
    /// Expected SHA-256 of the binary when --url is given.
    #[arg(long, requires = "url")]
    pub sha256: Option<String>,
    /// With --url: the URL serves the raw binary, not base64 text.
    #[arg(long, requires = "url")]
    pub raw: bool,
    /// Hex Ed25519 signature of the binary, required with --url (releases
    /// carry a `.sig` sidecar that is fetched automatically).
    #[arg(long, requires = "url")]
    pub sig: Option<String>,
    /// Stream the verified binary to each stage over the broker instead of
    /// having the stage download it. For kiosks whose proxy blocks the
    /// download. catc downloads and verifies the binary locally first.
    #[arg(long)]
    pub stream: bool,
    #[command(flatten)]
    pub targets: Targets,
}

#[derive(Subcommand)]
pub enum RelayCmd {
    /// Make <RELAY> push screenshots of what it shows to <MIRROR>.
    ///
    /// Generates a random frame secret and hands it to both stages over
    /// the encrypted channel; nothing to type but the mirror's name (shown
    /// on its about page as text and QR code). Both stages must be
    /// registered with `catc stage add`. Best on its own broker room, since
    /// every stage in a room receives every frame.
    Pair {
        /// The stage inside the network whose screen is mirrored.
        #[arg(long)]
        relay: String,
        /// The `--mirror` stage outside the network.
        #[arg(long)]
        mirror: String,
        /// Wait after each page load before capturing, e.g. `3s`.
        #[arg(long, default_value = "3s")]
        settle: String,
        /// Re-capture interval while the relay stays on one page.
        #[arg(long, default_value = "60s")]
        every: String,
        /// Accept a mirror name shorter than 16 characters. The frame
        /// secret travels encrypted under that name, so a guessable name
        /// exposes every frame.
        #[arg(long)]
        insecure_name: bool,
    },
    /// Set how much disk <MIRROR> may use for cached tiles (default 64MB).
    ///
    /// More cache means more pages of the relay's rotation come back for
    /// free; the least recently shown tiles are dropped first. Applies at
    /// once and survives restarts.
    Cache {
        #[arg(long)]
        mirror: String,
        /// e.g. `256MB`, `2GB`; a bare number is MB. Minimum 8MB.
        size: String,
    },
    /// Stop <RELAY> from pushing frames (and unpair <MIRROR> if given).
    Off {
        #[arg(long)]
        relay: String,
        #[arg(long)]
        mirror: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum AliasCmd {
    /// Store a catc command line under a short name.
    Add {
        name: String,
        /// The full `catc` command line, e.g. `nav https://x --for 5m`.
        command: Vec<String>,
    },
    /// List stored aliases.
    List,
    /// Run a stored alias by name.
    Run { name: String },
    /// Bulk-load aliases from a YAML file.
    Import { file: String },
    /// Bulk-save aliases to a YAML file.
    Export { file: String },
}

pub async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Init { socks } => cmd_init(socks),
        Cmd::Stage { sub } => match sub {
            StageCmd::Add { name } => cmd_add_stage(name),
            StageCmd::Remove { name } => cmd_remove_stage(name),
            StageCmd::Activate(s) => cmd_set_active(s, true),
            StageCmd::Deactivate(s) => cmd_set_active(s, false),
            StageCmd::List { probe } => cmd_targets_list(probe).await,
            StageCmd::Update(o) => cmd_stage_update(o).await,
        },

        Cmd::Pause(t) => cmd_send(t, Message::Pause).await,
        Cmd::Play(t) => cmd_send(t, Message::Play).await,
        Cmd::Back(t) => cmd_send(t, Message::Back).await,
        Cmd::Forward(t) => cmd_send(t, Message::Forward).await,
        Cmd::Nav {
            url,
            r#for,
            targets,
        } => {
            catcast_core::config::check_url(&url)?;
            let msg = match r#for {
                Some(d) => Message::NavTimed {
                    url,
                    duration_secs: humantime::parse_duration(&d)
                        .with_context(|| format!("parse duration {d:?}"))?
                        .as_secs(),
                },
                None => Message::Nav { url },
            };
            cmd_send(targets, msg).await
        }
        Cmd::Config { sub } => match sub {
            ConfigCmd::Import { file, targets } => cmd_config_import(file, targets).await,
            ConfigCmd::Export { targets } => {
                cmd_export_one(targets, ExportKind::Config, None).await
            }
            ConfigCmd::Edit(o) => crate::edit::run(crate::edit::Kind::Config, o).await,
        },
        Cmd::Logic { sub } => match sub {
            LogicCmd::Import { file, targets } => {
                let rhai = match file {
                    Some(path) => {
                        fs::read_to_string(&path).with_context(|| format!("read {path}"))?
                    }
                    None => DEFAULT_LOGIC.to_owned(),
                };
                cmd_send(targets, Message::SetLogic { rhai }).await
            }
            LogicCmd::Export { targets } => cmd_export_one(targets, ExportKind::Logic, None).await,
            LogicCmd::Edit(o) => crate::edit::run(crate::edit::Kind::Logic, o).await,
        },
        Cmd::State { sub } => match sub {
            StateCmd::Import { file, targets } => {
                // "import state" = push both config and logic in one go.
                let dir = std::path::Path::new(&file);
                let cfg = fs::read_to_string(dir.join("config.yaml"))?;
                let rhai = fs::read_to_string(dir.join("logic.rhai"))?;
                cmd_send_many(
                    targets,
                    vec![Message::SetConfig { yaml: cfg }, Message::SetLogic { rhai }],
                )
                .await
            }
            StateCmd::Export { dir, targets } => {
                cmd_export_one(targets, ExportKind::State, Some(dir)).await
            }
        },

        Cmd::Alias { sub } => match sub {
            AliasCmd::Add { name, command } => cmd_alias_add(name, command),
            AliasCmd::List => cmd_alias_list(),
            AliasCmd::Run { name } => cmd_alias_run(name).await,
            AliasCmd::Import { file } => cmd_alias_import(file),
            AliasCmd::Export { file } => cmd_alias_export(file),
        },
        Cmd::Autostart { sub } => match sub {
            AutostartCmd::Install { targets } => cmd_send(targets, Message::AutostartInstall).await,
            AutostartCmd::Uninstall { targets } => {
                cmd_send(targets, Message::AutostartUninstall).await
            }
        },
        Cmd::Fullscreen { on_off, targets } => {
            cmd_send(
                targets,
                Message::Fullscreen {
                    on: on_off.map(|v| matches!(v, OnOff::On)),
                },
            )
            .await
        }
        Cmd::Devtools { on_off, targets } => {
            cmd_send(
                targets,
                Message::DevTools {
                    on: on_off.map(|v| matches!(v, OnOff::On)),
                },
            )
            .await
        }
        Cmd::About(t) => cmd_send(t, Message::NavAbout).await,
        Cmd::Trace(t) => cmd_trace(t).await,
        Cmd::Shutdown(t) => cmd_send(t, Message::Shutdown).await,
        Cmd::Relay { sub } => match sub {
            RelayCmd::Pair {
                relay,
                mirror,
                settle,
                every,
                insecure_name,
            } => cmd_relay_pair(relay, mirror, &settle, &every, insecure_name).await,
            RelayCmd::Off { relay, mirror } => cmd_relay_off(relay, mirror).await,
            RelayCmd::Cache { mirror, size } => {
                let cap_bytes = parse_size(&size)?;
                send_one_each(vec![(mirror, Message::SetMirrorCache { cap_bytes })]).await
            }
        },
        Cmd::Version => cmd_version().await,
        Cmd::Status => crate::status::cmd_status().await,
        Cmd::SelfUpdate { version, force } => cmd_self_update(version, force).await,
    }
}

fn cmd_init(socks: String) -> Result<()> {
    let mut cfg = CatcConfig::load().unwrap_or_default();
    cfg.socks = socks;
    cfg.save()?;
    println!("wrote {}", crate::cfg::config_path()?.display());
    Ok(())
}

fn cmd_add_stage(name: String) -> Result<()> {
    let mut cfg = CatcConfig::load()?;
    if cfg.stages.iter().any(|s| s.name == name) {
        bail!("stage {name:?} already registered");
    }
    cfg.stages.push(Stage {
        name,
        active: true,
        mirror: false,
    });
    cfg.save()?;
    Ok(())
}

fn cmd_remove_stage(name: String) -> Result<()> {
    let mut cfg = CatcConfig::load()?;
    let before = cfg.stages.len();
    cfg.stages.retain(|s| s.name != name);
    if cfg.stages.len() == before {
        bail!("stage {name:?} not found");
    }
    cfg.save()?;
    Ok(())
}

fn cmd_set_active(s: StageList, active: bool) -> Result<()> {
    let mut cfg = CatcConfig::load()?;
    let names: Vec<String> = if s.all {
        cfg.stages.iter().map(|s| s.name.clone()).collect()
    } else if !s.names.is_empty() {
        s.names
    } else {
        bail!("specify one or more names, or pass --all");
    };
    for n in &names {
        let st = cfg
            .stage_mut(n)
            .ok_or_else(|| anyhow!("stage {n:?} not registered"))?;
        st.active = active;
    }
    cfg.save()?;
    Ok(())
}

/// Targets for a command. Without `--name`, mirrors are left out: they show
/// their relay's screen, so rotation commands and exports are for the relay.
pub(crate) fn resolve_targets(cfg: &CatcConfig, t: &Targets) -> Result<Vec<String>> {
    resolve_targets_scoped(cfg, t, false)
}

fn resolve_targets_scoped(
    cfg: &CatcConfig,
    t: &Targets,
    with_mirrors: bool,
) -> Result<Vec<String>> {
    let pick = |s: &&Stage| with_mirrors || !s.mirror;
    if t.all {
        Ok(cfg
            .stages
            .iter()
            .filter(pick)
            .map(|s| s.name.clone())
            .collect())
    } else if !t.names.is_empty() {
        for n in &t.names {
            if cfg.stage(n).is_none() {
                bail!("stage {n:?} not registered (run `catc stage add {n}`)");
            }
        }
        Ok(t.names.clone())
    } else {
        let active: Vec<String> = cfg
            .stages
            .iter()
            .filter(|s| s.active)
            .filter(pick)
            .map(|s| s.name.clone())
            .collect();
        if active.is_empty() {
            if cfg.stages.iter().any(|s| s.active && s.mirror) {
                bail!("only mirror stages are active — pass --name to address one");
            }
            bail!("no active stages — register some with `catc stage add` or pass --name/--all");
        }
        Ok(active)
    }
}

pub(crate) async fn cmd_send(t: Targets, msg: Message) -> Result<()> {
    cmd_send_many(t, vec![msg]).await
}

/// Send `msgs` to every resolved target and wait for one `Reply` per command
/// per target. Prints `name: ok message` or `name: ERR message` and exits
/// non-zero if any target was offline / unreachable / errored. `Shutdown` is
/// the awkward case: the stage exits before its reply can hit the wire
/// reliably, so we use a longer collect window for that variant only.
async fn cmd_send_many(t: Targets, msgs: Vec<Message>) -> Result<()> {
    use catcast_proto::Message::Reply;
    use std::time::Duration;

    let cfg = CatcConfig::load()?;
    let names = resolve_targets(&cfg, &t)?;
    let keys = keys_for(&names)?;
    let expected_per_target = msgs.len();
    let needs_extra_grace = msgs.iter().any(|m| matches!(m, Message::Shutdown));
    let collect_window = if needs_extra_grace {
        Duration::from_secs(3)
    } else {
        Duration::from_secs(2)
    };

    let mut br = Broker::connect(&cfg.socks).await?;
    for name in &names {
        let key = keys.get(name).expect("key just derived");
        for m in &msgs {
            br.send(key, name, m).await?;
        }
    }

    let plaintexts = br.collect(&keys, collect_window).await;
    br.close().await;

    // Group replies by target name (multiple commands → multiple replies in
    // send order; the broker preserves FIFO per sender).
    let mut by_target: HashMap<String, Vec<(bool, String)>> = HashMap::new();
    for (name, pt) in plaintexts {
        if let Reply { ok, message } = pt.msg {
            by_target.entry(name).or_default().push((ok, message));
        }
    }

    let mut all_ok = true;
    for name in &names {
        let replies = by_target.remove(name).unwrap_or_default();
        if replies.is_empty() {
            println!(
                "{name}: no reply within {}s (stage offline?)",
                collect_window.as_secs()
            );
            all_ok = false;
            continue;
        }
        if replies.len() < expected_per_target {
            // Got some but not all expected replies. Likely a stage crash
            // mid-batch or a Shutdown that beat the rest to app.exit().
            println!(
                "{name}: only {} of {} replies received",
                replies.len(),
                expected_per_target
            );
        }
        for (ok, message) in &replies {
            if *ok {
                println!(
                    "{name}: ok{}",
                    if message.is_empty() {
                        String::new()
                    } else {
                        format!(" — {message}")
                    }
                );
            } else {
                println!("{name}: ERR {message}");
                all_ok = false;
            }
        }
    }

    if !all_ok {
        bail!("one or more targets did not reply or returned an error");
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ExportKind {
    Config,
    Logic,
    State,
}

async fn cmd_config_import(file: String, t: Targets) -> Result<()> {
    let yaml = fs::read_to_string(&file).with_context(|| format!("read {file}"))?;
    let cfg = Config::from_yaml(&yaml)?;
    for w in cfg.validate()? {
        eprintln!("warning: {}", w.0);
    }
    cmd_send(t, Message::SetConfig { yaml }).await
}

/// Ask one stage for its config and/or logic. Each slot is `None` when the
/// stage did not answer within 2s, `Some(None)` when it has nothing loaded.
pub(crate) type Fetched = (Option<Option<String>>, Option<Option<String>>);

pub(crate) async fn fetch_one(
    cfg: &CatcConfig,
    name: &str,
    requests: &[Message],
) -> Result<Fetched> {
    let names = vec![name.to_string()];
    let keys = keys_for(&names)?;
    let key = keys.get(name).expect("key just derived");
    let mut br = Broker::connect(&cfg.socks).await?;
    for req in requests {
        br.send(key, name, req).await?;
    }
    let plaintexts = br.collect(&keys, Duration::from_secs(2)).await;
    br.close().await;

    let mut got_yaml: Option<Option<String>> = None;
    let mut got_rhai: Option<Option<String>> = None;
    for (_n, pt) in plaintexts {
        match pt.msg {
            Message::ConfigData { yaml } => got_yaml = Some(yaml),
            Message::LogicData { rhai } => got_rhai = Some(rhai),
            _ => {}
        }
    }
    Ok((got_yaml, got_rhai))
}

async fn cmd_export_one(t: Targets, kind: ExportKind, dir: Option<String>) -> Result<()> {
    let cfg = CatcConfig::load()?;
    let names = resolve_targets(&cfg, &t)?;
    if names.len() != 1 {
        bail!("export needs exactly one target (--name N)");
    }
    let name = names[0].clone();

    let requests: Vec<Message> = match kind {
        ExportKind::Config => vec![Message::GetConfig],
        ExportKind::Logic => vec![Message::GetLogic],
        ExportKind::State => vec![Message::GetConfig, Message::GetLogic],
    };
    let (got_yaml, got_rhai) = fetch_one(&cfg, &name, &requests).await?;

    match kind {
        ExportKind::Config => {
            let yaml =
                got_yaml.ok_or_else(|| anyhow!("{name}: no reply within 2s (stage offline?)"))?;
            let y = yaml.ok_or_else(|| anyhow!("{name}: no config loaded"))?;
            print!("{y}");
            if !y.ends_with('\n') {
                println!();
            }
        }
        ExportKind::Logic => {
            let rhai =
                got_rhai.ok_or_else(|| anyhow!("{name}: no reply within 2s (stage offline?)"))?;
            let r = rhai.ok_or_else(|| anyhow!("{name}: no logic loaded"))?;
            print!("{r}");
            if !r.ends_with('\n') {
                println!();
            }
        }
        ExportKind::State => {
            let yaml = got_yaml
                .ok_or_else(|| anyhow!("{name}: no config reply within 2s (stage offline?)"))?;
            let rhai = got_rhai
                .ok_or_else(|| anyhow!("{name}: no logic reply within 2s (stage offline?)"))?;
            let y = yaml.ok_or_else(|| anyhow!("{name}: no config loaded"))?;
            let r = rhai.ok_or_else(|| anyhow!("{name}: no logic loaded"))?;
            let dir = dir.expect("State export always carries a dir");
            let dir = std::path::Path::new(&dir);
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            let cfg_path = dir.join("config.yaml");
            let rhai_path = dir.join("logic.rhai");
            fs::write(&cfg_path, &y).with_context(|| format!("write {}", cfg_path.display()))?;
            fs::write(&rhai_path, &r).with_context(|| format!("write {}", rhai_path.display()))?;
            println!("wrote {}", cfg_path.display());
            println!("wrote {}", rhai_path.display());
        }
    }
    Ok(())
}

/// `catc trace`: intended vs. actual URL, then the navigation ring buffer.
/// The only command besides the exports that needs one target, because the
/// output is a per-stage log rather than a one-line verdict.
async fn cmd_trace(t: Targets) -> Result<()> {
    let cfg = CatcConfig::load()?;
    let names = resolve_targets(&cfg, &t)?;
    if names.len() != 1 {
        bail!("trace needs exactly one target (--name N)");
    }
    let name = names[0].clone();
    let keys = keys_for(&names)?;
    let key = keys.get(&name).expect("key just derived");

    let mut br = Broker::connect(&cfg.socks).await?;
    br.send(key, &name, &Message::GetState).await?;
    br.send(key, &name, &Message::GetTrace).await?;
    let plaintexts = br.collect(&keys, Duration::from_secs(2)).await;
    br.close().await;

    let mut state: Option<catcast_core::State> = None;
    let mut entries: Option<Vec<catcast_proto::TraceEntry>> = None;
    for (_n, pt) in plaintexts {
        match pt.msg {
            Message::State(s) => state = Some(s),
            Message::TraceData { entries: e } => entries = Some(e),
            _ => {}
        }
    }
    let state = state.ok_or_else(|| anyhow!("{name}: no reply within 2s (stage offline?)"))?;
    let entries = entries.ok_or_else(|| {
        anyhow!("{name}: state arrived but no trace — stage older than v0.3 (update it)")
    })?;

    println!("intended  {}", state.current_url.as_deref().unwrap_or("—"));
    println!(
        "actual    {}{}",
        state.actual_url.as_deref().unwrap_or("—"),
        if state.stuck() {
            "   <- STUCK (not on the intended host)"
        } else {
            ""
        }
    );
    println!("since     {}", fmt_ts(state.since));
    println!();
    if entries.is_empty() {
        println!("(no navigations recorded yet)");
    }
    for e in &entries {
        let kind = match e.kind {
            TraceKind::Intended => "intended",
            TraceKind::Started => "started ",
            TraceKind::Loaded => "loaded  ",
        };
        println!("{}  {kind}  {}", fmt_ts(e.ts), e.url);
    }
    Ok(())
}

/// Unix-ms as local wall-clock time; the date only when it isn't today.
fn fmt_ts(ms: i64) -> String {
    use chrono::{DateTime, Local, Utc};
    match DateTime::<Utc>::from_timestamp_millis(ms) {
        Some(dt) => {
            let local = dt.with_timezone(&Local);
            if local.date_naive() == Local::now().date_naive() {
                local.format("%H:%M:%S").to_string()
            } else {
                local.format("%Y-%m-%d %H:%M:%S").to_string()
            }
        }
        None => ms.to_string(),
    }
}

/// Last column of `stage list --probe`: the host the webview is actually
/// on, flagged when it is not the intended one.
fn where_column(st: &catcast_core::State) -> String {
    let place = match st
        .actual_url
        .as_deref()
        .and_then(catcast_core::state::url_host)
    {
        None => String::new(),
        Some(h) if st.stuck() => format!("STUCK on {h}"),
        Some(h) => format!("on {h}"),
    };
    let now = chrono::Utc::now().timestamp_millis();
    let role = relay_column(st, now);
    match (place.is_empty(), role.is_empty()) {
        (_, true) => place,
        (true, false) => role,
        (false, false) => format!("{place}\t{role}"),
    }
}

/// `relay→<mirror> frame 12s` / `mirror frame 3m`, empty for plain stages.
fn relay_column(st: &catcast_core::State, now_ms: i64) -> String {
    let frame = match st.last_frame_ts {
        Some(ts) => format!(
            "frame {}",
            humantime::format_duration(std::time::Duration::from_secs(
                (now_ms - ts).max(0) as u64 / 1000
            ))
        ),
        None => "no frame yet".into(),
    };
    match (&st.relay_to, st.mirror) {
        (Some(to), _) => format!("relay→{to} {frame}"),
        (None, true) => format!("mirror {frame}"),
        (None, false) => String::new(),
    }
}

/// Minimum mirror-name length `relay pair` accepts without
/// `--insecure-name`. Generated mirror names have 20.
const MIN_MIRROR_NAME: usize = 16;

fn check_mirror_name(name: &str, insecure_ok: bool) -> Result<()> {
    if name.chars().count() < MIN_MIRROR_NAME && !insecure_ok {
        bail!(
            "mirror name {name:?} is shorter than {MIN_MIRROR_NAME} characters; the frame \
             secret is sent encrypted under it. Start the mirror with `catstage --mirror` \
             and no --name to get a random one, or pass --insecure-name"
        );
    }
    Ok(())
}

/// `256MB`, `2 GB`, `512` (MB), `1.5GB`. Binary units: 1 MB = 2^20 bytes.
fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim().to_ascii_uppercase();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num
        .parse()
        .with_context(|| format!("size {s:?}: expected a number like 256MB"))?;
    let mul: f64 = match unit.trim().trim_end_matches("IB").trim_end_matches('B') {
        "" | "M" => (1u64 << 20) as f64,
        "K" => (1u64 << 10) as f64,
        "G" => (1u64 << 30) as f64,
        "T" => (1u64 << 40) as f64,
        other => bail!("size {s:?}: unknown unit {other:?} (use KB, MB, GB, TB)"),
    };
    if !(n.is_finite() && n > 0.0) {
        bail!("size {s:?} must be positive");
    }
    Ok((n * mul) as u64)
}

fn new_frame_secret() -> String {
    use rand::RngCore;
    let mut secret = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret);
    catcast_proto::to_hex(&secret)
}

async fn cmd_relay_pair(
    relay: String,
    mirror: String,
    settle: &str,
    every: &str,
    insecure_name: bool,
) -> Result<()> {
    check_mirror_name(&mirror, insecure_name)?;
    if relay == mirror {
        bail!("relay and mirror must be different stages");
    }
    let settle =
        humantime::parse_duration(settle).with_context(|| format!("--settle {settle:?}"))?;
    let every = humantime::parse_duration(every).with_context(|| format!("--every {every:?}"))?;
    let secret_hex = new_frame_secret();
    // Mirror first: it must know the secret before the first frame arrives.
    send_one_each(vec![
        (
            mirror.clone(),
            Message::SetMirror {
                secret_hex: Some(secret_hex.clone()),
            },
        ),
        (
            relay.clone(),
            Message::SetRelay {
                to: Some(mirror.clone()),
                secret_hex: Some(secret_hex),
                settle_secs: settle.as_secs(),
                every_secs: every.as_secs(),
            },
        ),
    ])
    .await?;
    mark_mirror(&mirror)?;
    println!(
        "paired: {relay} → {mirror}. Watch it with `catc status`. \
         Frames reach every stage in the broker room; a room of their own keeps them apart."
    );
    Ok(())
}

/// Record in `catc.toml` that `name` is a mirror, registering it if needed,
/// so commands for all active stages leave it alone.
pub(crate) fn mark_mirror(name: &str) -> Result<()> {
    let mut cfg = CatcConfig::load()?;
    match cfg.stage_mut(name) {
        Some(s) if s.mirror => return Ok(()),
        Some(s) => s.mirror = true,
        None => cfg.stages.push(Stage {
            name: name.to_string(),
            active: true,
            mirror: true,
        }),
    }
    cfg.save()
}

async fn cmd_relay_off(relay: String, mirror: Option<String>) -> Result<()> {
    let mut sends = vec![(
        relay,
        Message::SetRelay {
            to: None,
            secret_hex: None,
            settle_secs: 0,
            every_secs: 0,
        },
    )];
    if let Some(m) = mirror {
        sends.push((m, Message::SetMirror { secret_hex: None }));
    }
    send_one_each(sends).await
}

/// Send one message to each named (registered) stage, in order, and print
/// each stage's reply. Fails if any stage errs or stays silent.
async fn send_one_each(sends: Vec<(String, Message)>) -> Result<()> {
    let cfg = CatcConfig::load()?;
    for (n, _) in &sends {
        if cfg.stage(n).is_none() {
            bail!("stage {n:?} not registered (run `catc stage add {n}`)");
        }
    }
    let names: Vec<String> = sends.iter().map(|(n, _)| n.clone()).collect();
    let keys = keys_for(&names)?;
    let mut br = Broker::connect(&cfg.socks).await?;
    let mut all_ok = true;
    for (name, msg) in &sends {
        let key = keys.get(name).expect("key just derived");
        br.send(key, name, msg).await?;
        let mut reply: Option<(bool, String)> = None;
        br.collect_until(&keys, Duration::from_secs(5), |nm, pt| {
            if nm != name {
                return false;
            }
            if let Message::Reply { ok, message } = pt.msg {
                reply = Some((ok, message));
                return true;
            }
            false
        })
        .await;
        match reply {
            Some((true, m)) => println!("{name}: ok {m}"),
            Some((false, m)) => {
                println!("{name}: ERR {m}");
                all_ok = false;
                break;
            }
            None => {
                println!("{name}: no reply within 5s (stage offline?)");
                all_ok = false;
                break;
            }
        }
    }
    br.close().await;
    if !all_ok {
        bail!("not all stages accepted the change");
    }
    Ok(())
}

async fn cmd_targets_list(probe: bool) -> Result<()> {
    let cfg = CatcConfig::load()?;
    if cfg.stages.is_empty() {
        println!("(no stages registered — run `catc stage add <name>`)");
        return Ok(());
    }
    if !probe {
        for s in &cfg.stages {
            let role = if s.mirror { "\tmirror" } else { "" };
            println!(
                "{}\t{}{role}",
                if s.active { "active" } else { "off   " },
                s.name
            );
        }
        return Ok(());
    }
    let names: Vec<String> = cfg.stages.iter().map(|s| s.name.clone()).collect();
    let keys = keys_for(&names)?;
    let mut br = Broker::connect(&cfg.socks).await?;
    for n in &names {
        let key = keys.get(n).unwrap();
        br.send(key, n, &Message::GetState).await?;
    }
    // The release lookup runs alongside the 2s reply window, so it adds no
    // wall time. Failure only blanks the status column.
    let (replies, latest) = tokio::join!(
        br.collect(&keys, Duration::from_secs(2)),
        release::lookup(&cfg.update_url)
    );
    br.close().await;

    // Anything that decrypts under a stage's key proves it is up; the State
    // payload (when that is what came back) carries version + platform.
    let mut seen: HashMap<String, Option<catcast_core::State>> = HashMap::new();
    for (name, pt) in replies {
        let slot = seen.entry(name).or_default();
        if let Message::State(s) = pt.msg {
            crate::status::keep_newest(slot, s);
        }
    }

    match &latest {
        Some(Ok(v)) => println!("latest release: v{v}"),
        Some(Err(e)) => println!("latest release: unavailable ({e:#})"),
        None => {}
    }
    for s in &cfg.stages {
        let act = if s.active { "active" } else { "off   " };
        match seen.get(&s.name) {
            None => println!("down\t{act}\t{}\t-\t-", s.name),
            Some(None) => println!("up  \t{act}\t{}\t?\t?", s.name),
            Some(Some(st)) => {
                let platform = if st.platform.is_empty() {
                    "-"
                } else {
                    st.platform.as_str()
                };
                let status = match &latest {
                    Some(Ok(v)) => release::freshness(&st.version, v).label(v),
                    _ => String::new(),
                };
                println!(
                    "up  \t{act}\t{}\tv{}\t{platform}\t{status}\t{}",
                    s.name,
                    st.version,
                    where_column(st)
                );
            }
        }
    }
    Ok(())
}

/// Operator-triggered stage update. Pre-probes the targets so a stage that
/// cannot take the command is explained rather than timing out, sends one
/// `Update` per selected stage, then follows each through ack → terminal
/// reply → the new process's `State`.
async fn cmd_stage_update(o: UpdateOpts) -> Result<()> {
    use catcast_proto::UpdateEncoding;
    use semver::Version;

    let cfg = CatcConfig::load()?;
    let names = resolve_targets_scoped(&cfg, &o.targets, true)?;
    let keys = keys_for(&names)?;

    let target: Version = match &o.version {
        Some(v) => {
            let v = v.trim();
            Version::parse(v.strip_prefix('v').unwrap_or(v))
                .with_context(|| format!("parse --version {v:?}"))?
        }
        None => match release::lookup(&cfg.update_url).await {
            Some(r) => r.context("look up the latest release (or pass --version)")?,
            None => bail!(
                "update_url is empty in catc.toml — pass --version together with --url/--sha256"
            ),
        },
    };

    // Pre-probe: who is up, which version, which platform.
    let mut br = Broker::connect(&cfg.socks).await?;
    for n in &names {
        br.send(
            keys.get(n).expect("key just derived"),
            n,
            &Message::GetState,
        )
        .await?;
    }
    let mut states: HashMap<String, catcast_core::State> = HashMap::new();
    for (name, pt) in br.collect(&keys, Duration::from_secs(2)).await {
        if let Message::State(s) = pt.msg {
            states.insert(name, s);
        }
    }
    let mut selected: Vec<(String, String)> = Vec::new();
    let mut previous: HashMap<String, String> = HashMap::new();
    for n in &names {
        match release::plan_target(states.get(n), &target, o.force) {
            release::Decision::Skip(why) => println!("{n}: skipped — {why}"),
            release::Decision::Update => {
                selected.push((n.clone(), states[n].platform.clone()));
                previous.insert(n.clone(), states[n].version.clone());
            }
        }
    }
    // Stages already on the target that are not being reinstalled.
    let target_str = target.to_string();
    let mut on_target: Vec<String> = names
        .iter()
        .filter(|n| states.get(*n).is_some_and(|s| s.version == target_str))
        .filter(|n| !selected.iter().any(|(s, _)| s == *n))
        .cloned()
        .collect();
    if selected.is_empty() {
        hint_stale_logic(&mut br, &keys, &on_target).await?;
        br.close().await;
        bail!("nothing to update");
    }

    // One asset URL + hash + signature per platform among the selected stages.
    let mut per_platform: HashMap<String, (String, String, Option<String>)> = HashMap::new();
    for (_, platform) in &selected {
        if per_platform.contains_key(platform) {
            continue;
        }
        let entry = match (&o.url, &o.sha256) {
            (Some(u), Some(h)) => {
                let distinct: std::collections::HashSet<&String> =
                    selected.iter().map(|(_, p)| p).collect();
                if distinct.len() > 1 {
                    bail!("--url/--sha256 apply to one platform at a time; narrow the targets");
                }
                (u.clone(), h.trim().to_ascii_lowercase(), o.sig.clone())
            }
            _ => {
                let raw = release::asset_url(&cfg.update_url, &target, platform);
                let hash = release::fetch_sha256(&cfg.update_url, &target, platform)
                    .await
                    .with_context(|| format!("checksum for {platform}"))?;
                let sig = release::fetch_sig_of(
                    &cfg.update_url,
                    release::Binary::Catstage,
                    &target,
                    platform,
                )
                .await
                .with_context(|| format!("signature for {platform}"))?;
                let url = match &cfg.update_proxy {
                    Some(px) => release::proxied_url(px, &raw),
                    None => raw,
                };
                (url, hash, sig)
            }
        };
        per_platform.insert(platform.clone(), entry);
    }
    let encoding = if o.raw || (o.url.is_none() && cfg.update_proxy.is_none()) {
        UpdateEncoding::Raw
    } else {
        UpdateEncoding::Base64
    };

    // Streamed delivery: catc downloads and verifies each binary locally, then
    // pushes the bytes to each stage over the end-to-end-encrypted broker.
    if o.stream {
        let mut raws: HashMap<String, Vec<u8>> = HashMap::new();
        for (_, platform) in &selected {
            if raws.contains_key(platform) {
                continue;
            }
            let (url, hash, sig) = &per_platform[platform];
            let sig = sig.clone().ok_or_else(|| {
                anyhow!("{platform}: no signature — streaming needs a signed release (or --url with --sig)")
            })?;
            let bytes = fetch_and_verify_local(url, encoding, hash, &sig)
                .await
                .with_context(|| format!("prepare {platform} binary"))?;
            println!(
                "prepared {platform} binary: {} bytes, sha256 + signature ok",
                bytes.len()
            );
            raws.insert(platform.clone(), bytes);
        }
        let mut all_ok = true;
        for (n, platform) in &selected {
            let (_, hash, sig) = &per_platform[platform];
            let sig = sig.clone().expect("signature checked above");
            let bytes = &raws[platform];
            println!(
                "{n}: streaming v{target} ({} bytes) over the broker…",
                bytes.len()
            );
            match stream_to_stage(
                &mut br,
                &keys,
                n,
                bytes,
                target.to_string(),
                hash.clone(),
                sig,
                Duration::from_secs(o.timeout_secs),
            )
            .await
            {
                Ok(()) => println!("{n}: updated to v{target}"),
                Err(e) => {
                    println!("{n}: ERR {e:#}");
                    all_ok = false;
                }
            }
        }
        br.close().await;
        if !all_ok {
            bail!("one or more stages were not updated");
        }
        return Ok(());
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Phase {
        Sent,
        Acked,
        Restarting,
        BackUp,
        Failed,
    }
    let mut phase: HashMap<String, Phase> = HashMap::new();
    for (n, platform) in &selected {
        let (url, sha256, sig) = per_platform[platform].clone();
        br.send(
            keys.get(n).expect("key just derived"),
            n,
            &Message::Update {
                version: target.to_string(),
                url,
                sha256,
                encoding,
                sig,
            },
        )
        .await?;
        println!("{n}: update to v{target} sent");
        phase.insert(n.clone(), Phase::Sent);
    }

    let started = std::time::Instant::now();
    // Takes `phase` explicitly so the completion checks below can read it
    // between calls.
    let handle = |phase: &mut HashMap<String, Phase>, name: &str, pt: catcast_proto::Plaintext| {
        let Some(ph) = phase.get_mut(name) else {
            return;
        };
        match pt.msg {
            Message::Reply { ok, message } => match (*ph, ok) {
                (Phase::Sent, true) => {
                    *ph = Phase::Acked;
                    println!("{name}: {message}");
                }
                (Phase::Acked, true) => {
                    *ph = Phase::Restarting;
                    println!("{name}: {message}");
                }
                (Phase::Sent | Phase::Acked, false) => {
                    *ph = Phase::Failed;
                    println!("{name}: ERR {message}");
                }
                _ => {}
            },
            Message::State(s) if s.version == target_str => {
                // A State carrying the target version proves the new process
                // is up — unless this is a same-version reinstall, where only
                // a State after the terminal reply can be the new process.
                let unambiguous = previous.get(name) != Some(&s.version);
                if *ph == Phase::Restarting || (*ph == Phase::Acked && unambiguous) {
                    *ph = Phase::BackUp;
                    println!(
                        "{name}: back up as v{} after {}s",
                        s.version,
                        started.elapsed().as_secs()
                    );
                }
            }
            _ => {}
        }
    };

    // Phase A: every stage acks (or refuses) within seconds. A stage that
    // does not understand the command drops it silently — that is what a
    // missing ack means.
    br.collect_until(&keys, Duration::from_secs(10), |name, pt| {
        handle(&mut phase, name, pt);
        !phase.values().any(|p| *p == Phase::Sent)
    })
    .await;
    for (n, p) in phase.iter_mut() {
        if *p == Phase::Sent {
            println!("{n}: no ack within 10s — stage too old for remote update?");
            *p = Phase::Failed;
        }
    }

    // Phase B: the download, swap and relaunch. Stages that reported
    // "restarting" are nudged with a GetState every couple of seconds so
    // confirmation does not wait for the new process's next spontaneous
    // State broadcast (which can be a whole rotation slot away).
    let deadline = started + Duration::from_secs(o.timeout_secs);
    let done = |p: &Phase| matches!(p, Phase::BackUp | Phase::Failed);
    loop {
        let now = std::time::Instant::now();
        if now >= deadline || phase.values().all(done) {
            break;
        }
        let window = (deadline - now).min(Duration::from_secs(2));
        br.collect_until(&keys, window, |name, pt| {
            handle(&mut phase, name, pt);
            phase.values().all(done)
        })
        .await;
        for (n, p) in &phase {
            if *p == Phase::Restarting {
                br.send(
                    keys.get(n).expect("key just derived"),
                    n,
                    &Message::GetState,
                )
                .await?;
            }
        }
    }
    on_target.extend(
        selected
            .iter()
            .map(|(n, _)| n)
            .filter(|n| phase[*n] == Phase::BackUp)
            .cloned(),
    );
    hint_stale_logic(&mut br, &keys, &on_target).await?;
    br.close().await;

    let mut all_ok = true;
    for n in selected.iter().map(|(n, _)| n) {
        match phase[n] {
            Phase::BackUp => {}
            Phase::Failed => all_ok = false,
            _ => {
                all_ok = false;
                println!(
                    "{n}: no confirmation within {}s — check with `catc stage list --probe`",
                    o.timeout_secs
                );
            }
        }
    }
    if !all_ok {
        bail!("one or more stages were not updated");
    }
    Ok(())
}

/// Point out stages whose logic is a verbatim default from an older release
/// (or missing). Custom logic is neither reported nor touched: refreshing it
/// is the operator's call, so this only prints the command to run.
async fn hint_stale_logic(
    br: &mut Broker,
    keys: &HashMap<String, catcast_proto::Key>,
    names: &[String],
) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    for n in names {
        br.send(
            keys.get(n).expect("key just derived"),
            n,
            &Message::GetLogic,
        )
        .await?;
    }
    let mut logic: HashMap<String, Option<String>> = HashMap::new();
    br.collect_until(keys, Duration::from_secs(2), |name, pt| {
        if let Message::LogicData { rhai } = pt.msg {
            logic.insert(name.to_owned(), rhai);
        }
        names.iter().all(|n| logic.contains_key(n))
    })
    .await;
    for n in names {
        match logic.get(n).map(|r| default_logic::classify(r.as_deref())) {
            Some(LogicStatus::OutdatedDefault(releases)) => println!(
                "{n}: logic is the default shipped with {releases}; \
                 refresh it with `catc logic import --name {n}`"
            ),
            Some(LogicStatus::Missing) => println!(
                "{n}: no logic loaded; push the default with `catc logic import --name {n}`"
            ),
            _ => {}
        }
    }
    Ok(())
}

/// Download `url` into memory, decode per `encoding`, and verify the SHA-256
/// and Ed25519 signature. No platform magic check here — the bytes may be for
/// another OS (streaming to a Windows kiosk from Linux); the receiving stage
/// checks its own magic. Returns the raw binary.
async fn fetch_and_verify_local(
    url: &str,
    encoding: catcast_proto::UpdateEncoding,
    sha256: &str,
    sig: &str,
) -> Result<Vec<u8>> {
    use catcast_net::update::{self as core, Sink};
    use std::fs::File;

    let tmp = std::env::temp_dir().join(format!(
        "catc-stream-{}-{}.bin",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    let mut sink = Sink::new(f, encoding);
    let res = async {
        core::download(url, &mut sink).await?;
        let done = sink.finish()?;
        let want = sha256.trim().to_ascii_lowercase();
        if done.sha256 != want {
            bail!("sha256 mismatch: expected {want}, got {}", done.sha256);
        }
        let bytes = std::fs::read(&tmp).with_context(|| format!("read {}", tmp.display()))?;
        core::verify_signature(&bytes, sig)?;
        Ok(bytes)
    }
    .await;
    let _ = std::fs::remove_file(&tmp);
    res
}

/// Stream a verified binary to one stage: announce, send chunks, then
/// resend whatever the stage reports missing until it installs or we time
/// out. 48 KiB raw per chunk keeps each broker frame well under a megabyte.
#[allow(clippy::too_many_arguments)]
async fn stream_to_stage(
    br: &mut Broker,
    keys: &HashMap<String, catcast_proto::Key>,
    name: &str,
    raw: &[u8],
    version: String,
    sha256: String,
    sig: String,
    timeout: Duration,
) -> Result<()> {
    use base64::Engine as _;
    const CHUNK: usize = 48 * 1024;

    let key = keys.get(name).expect("key for target");
    let chunks: Vec<&[u8]> = raw.chunks(CHUNK).collect();
    let b64 = |c: &[u8]| base64::engine::general_purpose::STANDARD.encode(c);

    br.send(
        key,
        name,
        &Message::UpdateStream {
            version: version.clone(),
            total_len: raw.len() as u64,
            sha256,
            sig,
            chunk_count: chunks.len() as u32,
            chunk_bytes: CHUNK as u32,
        },
    )
    .await?;
    for (i, c) in chunks.iter().enumerate() {
        br.send(
            key,
            name,
            &Message::UpdateChunk {
                seq: i as u32,
                data: b64(c),
            },
        )
        .await?;
    }
    br.send(key, name, &Message::UpdateStreamEnd).await?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::time::Instant::now() >= deadline {
            bail!("timed out waiting for the stage to install");
        }
        let mut need: Vec<u32> = Vec::new();
        let mut result: Option<std::result::Result<(), String>> = None;
        let window = (deadline - std::time::Instant::now()).min(Duration::from_secs(5));
        br.collect_until(keys, window, |nm, pt| {
            if nm != name {
                return false;
            }
            match pt.msg {
                Message::UpdateNeed { missing } => {
                    need = missing;
                    true
                }
                Message::Reply { ok: false, message } => {
                    result = Some(Err(message));
                    true
                }
                Message::Reply { ok: true, message } => {
                    if message.contains("restarting") {
                        result = Some(Ok(()));
                        true
                    } else {
                        println!("  {nm}: {message}");
                        false
                    }
                }
                Message::State(s) if s.version == version => {
                    result = Some(Ok(()));
                    true
                }
                _ => false,
            }
        })
        .await;

        if let Some(r) = result {
            return r.map_err(|m| anyhow!(m));
        }
        if !need.is_empty() {
            for seq in need {
                if let Some(c) = chunks.get(seq as usize) {
                    br.send(key, name, &Message::UpdateChunk { seq, data: b64(c) })
                        .await?;
                }
            }
            br.send(key, name, &Message::UpdateStreamEnd).await?;
        } else {
            // Quiet window: nudge so the post-install State is seen promptly.
            br.send(key, name, &Message::GetState).await?;
        }
    }
}

async fn cmd_self_update(version: Option<String>, force: bool) -> Result<()> {
    use catcast_net::update as core;
    use catcast_proto::UpdateEncoding;
    use semver::Version;

    let me = env!("CARGO_PKG_VERSION");
    let me_v = Version::parse(me).expect("own version parses");
    let cfg = CatcConfig::load().unwrap_or_default();
    let platform = catcast_core::platform_tag();

    let target: Version = match &version {
        Some(v) => {
            let v = v.trim();
            Version::parse(v.strip_prefix('v').unwrap_or(v))
                .with_context(|| format!("parse --version {v:?}"))?
        }
        None => match release::lookup(&cfg.update_url).await {
            Some(r) => r.context("look up the latest release (or pass --version)")?,
            None => bail!("update_url is empty in catc.toml — pass --version"),
        },
    };

    if target == me_v && !force {
        println!("catc is already v{me}");
        return Ok(());
    }
    if target < me_v && !force {
        bail!("v{target} is older than the running v{me}; pass --force to downgrade");
    }

    let raw_url = release::asset_url_of(&cfg.update_url, release::Binary::Catc, &target, &platform);
    let hash = release::fetch_sha256_of(&cfg.update_url, release::Binary::Catc, &target, &platform)
        .await
        .context("fetch catc checksum")?;
    let sig = release::fetch_sig_of(&cfg.update_url, release::Binary::Catc, &target, &platform)
        .await
        .context("fetch catc signature")?
        .ok_or_else(|| anyhow!("v{target} has no .sig sidecar — download catc yourself"))?;
    let (url, encoding) = match &cfg.update_proxy {
        Some(px) => (release::proxied_url(px, &raw_url), UpdateEncoding::Base64),
        None => (raw_url, UpdateEncoding::Raw),
    };

    let paths = core::paths()?;
    let file = std::fs::File::create(&paths.new).with_context(|| {
        format!(
            "create {} (is the directory writable?)",
            paths.new.display()
        )
    })?;
    let staged = async {
        let mut sink = core::Sink::new(file, encoding);
        core::download(&url, &mut sink).await?;
        let done = sink.finish()?;
        core::verify(&done, &hash)?; // our own platform → magic check applies
        core::verify_signature_file(&paths.new, &sig)?;
        anyhow::Ok(done.len)
    }
    .await;
    let len = match staged {
        Ok(len) => len,
        Err(e) => {
            let _ = std::fs::remove_file(&paths.new);
            return Err(e.context("stage the new catc"));
        }
    };
    println!("downloaded catc v{target} ({len} bytes, sha256 + signature ok)");

    core::unblock(&paths.new);
    core::swap(&paths).context("swap the catc binary")?;

    // Confirm the swapped-in binary reports the target version; roll back if not.
    let out = std::process::Command::new(&paths.exe)
        .arg("--version")
        .output()
        .context("run the new catc --version")?;
    let reported = String::from_utf8_lossy(&out.stdout);
    if !reported.contains(&target.to_string()) {
        core::rollback(&paths).context("roll back after a failed version check")?;
        bail!("new catc reported {reported:?}, expected v{target} — rolled back");
    }
    let _ = std::fs::remove_file(&paths.old);
    println!("catc updated to v{target}");
    Ok(())
}

async fn cmd_version() -> Result<()> {
    let me = env!("CARGO_PKG_VERSION");
    println!("catc v{me}");
    let cfg = CatcConfig::load().unwrap_or_default();
    match release::lookup(&cfg.update_url).await {
        None => println!("release check disabled (update_url is empty)"),
        Some(Err(e)) => println!("latest release: unavailable ({e:#})"),
        Some(Ok(v)) => println!(
            "latest release: v{v} ({})",
            release::freshness(me, &v).label(&v)
        ),
    }
    Ok(())
}

fn cmd_alias_add(name: String, command: Vec<String>) -> Result<()> {
    if command.is_empty() {
        bail!("alias needs a command");
    }
    let mut a = Aliases::load()?;
    a.0.insert(name, shell_join(&command));
    a.save()
}

fn cmd_alias_list() -> Result<()> {
    let a = Aliases::load()?;
    for (k, v) in &a.0 {
        println!("{k}\t{v}");
    }
    Ok(())
}

async fn cmd_alias_run(name: String) -> Result<()> {
    let a = Aliases::load()?;
    let line =
        a.0.get(&name)
            .ok_or_else(|| anyhow!("alias {name:?} not found"))?
            .clone();
    let parts = shell_split(&line);
    let mut argv = vec!["catc".to_string()];
    argv.extend(parts);
    let cli = Cli::try_parse_from(argv)?;
    Box::pin(run(cli)).await
}

fn cmd_alias_import(file: String) -> Result<()> {
    let s = fs::read_to_string(&file)?;
    let inner: std::collections::BTreeMap<String, String> = serde_yaml::from_str(&s)?;
    let mut a = Aliases::load()?;
    a.0.extend(inner);
    a.save()
}

fn cmd_alias_export(file: String) -> Result<()> {
    let a = Aliases::load()?;
    fs::write(&file, serde_yaml::to_string(&a.0)?)?;
    Ok(())
}

/// Inverse of [`shell_split`]: arguments with spaces, quotes or
/// backslashes are single-quoted (a `'` inside becomes `'\''`), so a URL
/// stored by `alias add` comes back from `alias run` unchanged.
fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            let plain = !a.is_empty()
                && !a
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '\'' | '"' | '\\'));
            if plain {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Tiny shell-style splitter: handles double quotes, single quotes,
/// backslash escapes outside quotes. Sufficient for alias bodies.
fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
            }
            (Some(_), c) => cur.push(c),
            (None, '\'') | (None, '"') => quote = Some(c),
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(stages: &[(&str, bool, bool)]) -> CatcConfig {
        CatcConfig {
            stages: stages
                .iter()
                .map(|(n, active, mirror)| Stage {
                    name: n.to_string(),
                    active: *active,
                    mirror: *mirror,
                })
                .collect(),
            ..CatcConfig::default()
        }
    }

    fn targets(names: &[&str], all: bool) -> Targets {
        Targets {
            names: names.iter().map(|n| n.to_string()).collect(),
            all,
        }
    }

    #[test]
    fn aliases_keep_urls_with_special_characters() {
        for url in [
            "https://x.example/menu.html#geek,flags",
            "https://x.example/a?b=1&c=2#x",
            "https://x.example/p%20q?x=%2F&y=a+b",
            "https://x.example/a?q=it's&r=\"quoted\"&s=back\\slash",
            "https://x.example/a?q=two words",
            "https://x.example/ümlaut?ä=ö#ß",
        ] {
            let argv: Vec<String> = ["nav", url, "--for", "2m", "--name", "CH-WD-K000ljay7"]
                .map(String::from)
                .to_vec();
            assert_eq!(shell_split(&shell_join(&argv)), argv, "{url}");
        }
        // Plain words stay readable in aliases.yaml.
        let argv = ["nav", "https://x.example/a?b=1#c"].map(String::from);
        assert_eq!(shell_join(&argv), "nav https://x.example/a?b=1#c");
    }

    #[test]
    fn default_targets_skip_mirrors_but_updates_reach_them() {
        let cfg = registry(&[
            ("kiosk", true, false),
            ("m1rror", true, true),
            ("tv", false, false),
        ]);
        assert_eq!(
            resolve_targets(&cfg, &targets(&[], false)).unwrap(),
            ["kiosk"]
        );
        assert_eq!(
            resolve_targets(&cfg, &targets(&[], true)).unwrap(),
            ["kiosk", "tv"]
        );
        assert_eq!(
            resolve_targets_scoped(&cfg, &targets(&[], false), true).unwrap(),
            ["kiosk", "m1rror"]
        );
        // Named explicitly, a mirror is addressed like any stage.
        assert_eq!(
            resolve_targets(&cfg, &targets(&["m1rror"], false)).unwrap(),
            ["m1rror"]
        );
    }

    #[test]
    fn only_mirrors_active_says_so() {
        let cfg = registry(&[("m1rror", true, true)]);
        let e = resolve_targets(&cfg, &targets(&[], false)).unwrap_err();
        assert!(e.to_string().contains("only mirror stages"), "{e}");
    }

    #[test]
    fn registry_without_mirror_key_still_parses() {
        let cfg: CatcConfig =
            toml::from_str("socks = \"wss://x/r/y\"\n[[stages]]\nname = \"kiosk\"\n").unwrap();
        assert!(!cfg.stages[0].mirror);
        let out = toml::to_string(&cfg).unwrap();
        assert!(!out.contains("mirror"), "plain stages stay unmarked: {out}");
    }

    #[test]
    fn short_mirror_names_need_an_explicit_opt_in() {
        assert!(check_mirror_name("lobby-tv", false).is_err());
        assert!(check_mirror_name("lobby-tv", true).is_ok());
        assert!(check_mirror_name("abcdefghjkmnpqrstuvw", false).is_ok());
    }

    #[test]
    fn sizes_parse_with_binary_units() {
        assert_eq!(parse_size("256MB").unwrap(), 256 << 20);
        assert_eq!(parse_size("256").unwrap(), 256 << 20);
        assert_eq!(parse_size("2 GB").unwrap(), 2 << 30);
        assert_eq!(parse_size("1.5gib").unwrap(), 3 << 29);
        assert_eq!(parse_size("512k").unwrap(), 512 << 10);
        assert!(parse_size("lots").is_err());
        assert!(parse_size("5XB").is_err());
        assert!(parse_size("0MB").is_err());
    }

    #[test]
    fn frame_secrets_are_random_hex() {
        let a = new_frame_secret();
        assert_eq!(catcast_proto::parse_frame_secret(&a).unwrap().len(), 32);
        assert_ne!(a, new_frame_secret());
    }

    #[test]
    fn relay_column_shows_role_and_frame_age() {
        let mut st = catcast_core::State::fresh("r", "0.4.1");
        assert_eq!(relay_column(&st, 0), "");
        st.relay_to = Some("m".into());
        st.last_frame_ts = Some(1_000);
        assert_eq!(relay_column(&st, 13_000), "relay→m frame 12s");
        st.relay_to = None;
        st.mirror = true;
        st.last_frame_ts = None;
        assert_eq!(relay_column(&st, 0), "mirror no frame yet");
    }
}

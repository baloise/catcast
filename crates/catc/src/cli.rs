use crate::cfg::{Aliases, CatcConfig, Stage};
use crate::net::{keys_for, Broker};
use crate::release;
use anyhow::{anyhow, bail, Context, Result};
use catcast_core::Config;
use catcast_proto::Message;
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
    /// Import or export the rotation config (YAML).
    Config {
        #[command(subcommand)]
        sub: ConfigCmd,
    },
    /// Import or export the rotation logic (Rhai).
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
    /// Cleanly stop the stage process.
    Shutdown(Targets),
    /// Print catc's version and the latest published release.
    Version,
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
    /// given, the command goes to every active stage.
    #[arg(long = "name")]
    pub names: Vec<String>,
    /// Target every registered stage, regardless of active flag.
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
    #[command(flatten)]
    pub targets: Targets,
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
        } => match r#for {
            Some(d) => {
                let dur = humantime::parse_duration(&d)
                    .with_context(|| format!("parse duration {d:?}"))?;
                cmd_send(
                    targets,
                    Message::NavTimed {
                        url,
                        duration_secs: dur.as_secs(),
                    },
                )
                .await
            }
            None => cmd_send(targets, Message::Nav { url }).await,
        },
        Cmd::Config { sub } => match sub {
            ConfigCmd::Import { file, targets } => cmd_config_import(file, targets).await,
            ConfigCmd::Export { targets } => {
                cmd_export_one(targets, ExportKind::Config, None).await
            }
        },
        Cmd::Logic { sub } => match sub {
            LogicCmd::Import { file, targets } => {
                const DEFAULT_LOGIC: &str = include_str!("../../../default-logic/default.rhai");
                let rhai = match file {
                    Some(path) => {
                        fs::read_to_string(&path).with_context(|| format!("read {path}"))?
                    }
                    None => DEFAULT_LOGIC.to_owned(),
                };
                cmd_send(targets, Message::SetLogic { rhai }).await
            }
            LogicCmd::Export { targets } => cmd_export_one(targets, ExportKind::Logic, None).await,
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
        Cmd::Shutdown(t) => cmd_send(t, Message::Shutdown).await,
        Cmd::Version => cmd_version().await,
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
    cfg.stages.push(Stage { name, active: true });
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

fn resolve_targets(cfg: &CatcConfig, t: &Targets) -> Result<Vec<String>> {
    if t.all {
        Ok(cfg.stages.iter().map(|s| s.name.clone()).collect())
    } else if !t.names.is_empty() {
        for n in &t.names {
            if cfg.stage(n).is_none() {
                bail!("stage {n:?} not registered (run `catc stage add {n}`)");
            }
        }
        Ok(t.names.clone())
    } else {
        let active = cfg.active_names();
        if active.is_empty() {
            bail!("no active stages — register some with `catc stage add` or pass --name/--all");
        }
        Ok(active)
    }
}

async fn cmd_send(t: Targets, msg: Message) -> Result<()> {
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

async fn cmd_export_one(t: Targets, kind: ExportKind, dir: Option<String>) -> Result<()> {
    let cfg = CatcConfig::load()?;
    let names = resolve_targets(&cfg, &t)?;
    if names.len() != 1 {
        bail!("export needs exactly one target (--name N)");
    }
    let name = names[0].clone();
    let keys = keys_for(&names)?;
    let key = keys.get(&name).expect("key just derived");

    let requests: Vec<Message> = match kind {
        ExportKind::Config => vec![Message::GetConfig],
        ExportKind::Logic => vec![Message::GetLogic],
        ExportKind::State => vec![Message::GetConfig, Message::GetLogic],
    };

    let mut br = Broker::connect(&cfg.socks).await?;
    for req in &requests {
        br.send(key, &name, req).await?;
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

async fn cmd_targets_list(probe: bool) -> Result<()> {
    let cfg = CatcConfig::load()?;
    if cfg.stages.is_empty() {
        println!("(no stages registered — run `catc stage add <name>`)");
        return Ok(());
    }
    if !probe {
        for s in &cfg.stages {
            println!("{}\t{}", if s.active { "active" } else { "off   " }, s.name);
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
            *slot = Some(s);
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
                    "up  \t{act}\t{}\tv{}\t{platform}\t{status}",
                    s.name, st.version
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
    let names = resolve_targets(&cfg, &o.targets)?;
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
    if selected.is_empty() {
        br.close().await;
        bail!("nothing to update");
    }

    // One asset URL + hash per platform among the selected stages.
    let mut per_platform: HashMap<String, (String, String)> = HashMap::new();
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
                (u.clone(), h.trim().to_ascii_lowercase())
            }
            _ => {
                let raw = release::asset_url(&cfg.update_url, &target, platform);
                let hash = release::fetch_sha256(&cfg.update_url, &target, platform)
                    .await
                    .with_context(|| format!("checksum for {platform}"))?;
                let url = match &cfg.update_proxy {
                    Some(px) => release::proxied_url(px, &raw),
                    None => raw,
                };
                (url, hash)
            }
        };
        per_platform.insert(platform.clone(), entry);
    }
    let encoding = if o.raw || (o.url.is_none() && cfg.update_proxy.is_none()) {
        UpdateEncoding::Raw
    } else {
        UpdateEncoding::Base64
    };

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
        let (url, sha256) = per_platform[platform].clone();
        br.send(
            keys.get(n).expect("key just derived"),
            n,
            &Message::Update {
                version: target.to_string(),
                url,
                sha256,
                encoding,
            },
        )
        .await?;
        println!("{n}: update to v{target} sent");
        phase.insert(n.clone(), Phase::Sent);
    }

    let started = std::time::Instant::now();
    let target_str = target.to_string();
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
    a.0.insert(name, command.join(" "));
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

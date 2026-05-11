use crate::cfg::{Aliases, CatcConfig, Stage};
use crate::net::{keys_for, Broker};
use anyhow::{anyhow, bail, Context, Result};
use catcast_core::Config;
use catcast_proto::Message;
use clap::{Args, Parser, Subcommand};
use std::fs;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "catc", version, about = "CatCast CLI")]
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
    /// Register a stage by name (read it off the stage's catcast://about page).
    AddStage {
        name: String,
    },
    /// Forget a stage.
    RemoveStage {
        name: String,
    },
    /// Mark stages active.
    Activate(StageList),
    /// Mark stages inactive.
    Deactivate(StageList),

    Pause(Targets),
    Play(Targets),
    Nav {
        url: String,
        /// Optional duration like `5m`, `30s`. If set, rotation resumes after.
        #[arg(long)]
        r#for: Option<String>,
        #[command(flatten)]
        targets: Targets,
    },
    Manual {
        on_off: OnOff,
        #[command(flatten)]
        targets: Targets,
    },

    Config {
        #[command(subcommand)]
        sub: ConfigCmd,
    },
    Logic {
        #[command(subcommand)]
        sub: LogicCmd,
    },
    State {
        #[command(subcommand)]
        sub: StateCmd,
    },

    Targets {
        #[command(subcommand)]
        sub: TargetsCmd,
    },
    Alias {
        #[command(subcommand)]
        sub: AliasCmd,
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
    Import {
        file: String,
        #[command(flatten)]
        targets: Targets,
    },
    Export {
        #[command(flatten)]
        targets: Targets,
    },
}

#[derive(Subcommand)]
pub enum LogicCmd {
    Import {
        file: String,
        #[command(flatten)]
        targets: Targets,
    },
    Export {
        #[command(flatten)]
        targets: Targets,
    },
}

#[derive(Subcommand)]
pub enum StateCmd {
    Import {
        file: String,
        #[command(flatten)]
        targets: Targets,
    },
    Export {
        #[command(flatten)]
        targets: Targets,
    },
}

#[derive(Subcommand)]
pub enum TargetsCmd {
    /// List configured stages.
    List {
        /// Send a GetState to each and report which respond.
        #[arg(long)]
        probe: bool,
    },
}

#[derive(Subcommand)]
pub enum AliasCmd {
    Add {
        name: String,
        /// The full `catc` command line, e.g. `nav https://x --for 5m`.
        command: Vec<String>,
    },
    List,
    Run {
        name: String,
    },
    Import {
        file: String,
    },
    Export {
        file: String,
    },
}

pub async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Init { socks } => cmd_init(socks),
        Cmd::AddStage { name } => cmd_add_stage(name),
        Cmd::RemoveStage { name } => cmd_remove_stage(name),
        Cmd::Activate(s) => cmd_set_active(s, true),
        Cmd::Deactivate(s) => cmd_set_active(s, false),

        Cmd::Pause(t) => cmd_send(t, Message::Pause).await,
        Cmd::Play(t) => cmd_send(t, Message::Play).await,
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
        Cmd::Manual { on_off, targets } => {
            let on = matches!(on_off, OnOff::On);
            cmd_send(targets, Message::Manual { on }).await
        }

        Cmd::Config { sub } => match sub {
            ConfigCmd::Import { file, targets } => cmd_config_import(file, targets).await,
            ConfigCmd::Export { targets } => cmd_export_one(targets, ExportKind::Config).await,
        },
        Cmd::Logic { sub } => match sub {
            LogicCmd::Import { file, targets } => {
                let rhai = fs::read_to_string(&file).with_context(|| format!("read {file}"))?;
                cmd_send(targets, Message::SetLogic { rhai }).await
            }
            LogicCmd::Export { targets } => cmd_export_one(targets, ExportKind::Logic).await,
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
            StateCmd::Export { targets } => cmd_export_one(targets, ExportKind::State).await,
        },

        Cmd::Targets { sub } => match sub {
            TargetsCmd::List { probe } => cmd_targets_list(probe).await,
        },
        Cmd::Alias { sub } => match sub {
            AliasCmd::Add { name, command } => cmd_alias_add(name, command),
            AliasCmd::List => cmd_alias_list(),
            AliasCmd::Run { name } => cmd_alias_run(name).await,
            AliasCmd::Import { file } => cmd_alias_import(file),
            AliasCmd::Export { file } => cmd_alias_export(file),
        },
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
                bail!("stage {n:?} not registered (run `catc add-stage {n}`)");
            }
        }
        Ok(t.names.clone())
    } else {
        let active = cfg.active_names();
        if active.is_empty() {
            bail!("no active stages — register some with `catc add-stage` or pass --name/--all");
        }
        Ok(active)
    }
}

async fn cmd_send(t: Targets, msg: Message) -> Result<()> {
    cmd_send_many(t, vec![msg]).await
}

async fn cmd_send_many(t: Targets, msgs: Vec<Message>) -> Result<()> {
    let cfg = CatcConfig::load()?;
    let names = resolve_targets(&cfg, &t)?;
    let keys = keys_for(&names)?;
    let mut br = Broker::connect(&cfg.socks).await?;
    for name in &names {
        let key = keys.get(name).expect("key just derived");
        for m in &msgs {
            br.send(key, name, m).await?;
        }
        println!("-> {name}: {}", msgs.len());
    }
    br.close().await;
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

async fn cmd_export_one(t: Targets, _kind: ExportKind) -> Result<()> {
    // Export currently requires exactly one target. We'd request a State
    // snapshot from that one stage and write the requested artifact.
    // TODO(catstage): wire SetConfig/SetLogic readbacks. For v0.0.1 the
    // GetState reply only carries metadata, not the full config/logic
    // payload — extend State to include them, or add dedicated GetConfig
    // / GetLogic messages.
    let cfg = CatcConfig::load()?;
    let names = resolve_targets(&cfg, &t)?;
    if names.len() != 1 {
        bail!("export needs exactly one target (--name N)");
    }
    bail!("export not implemented yet — see TODO in catc/src/cli.rs");
}

async fn cmd_targets_list(probe: bool) -> Result<()> {
    let cfg = CatcConfig::load()?;
    if cfg.stages.is_empty() {
        println!("(no stages registered — run `catc add-stage <name>`)");
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
    let replies = br.collect(&keys, Duration::from_secs(2)).await;
    br.close().await;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (name, _) in replies {
        seen.insert(name);
    }
    for s in &cfg.stages {
        let live = if seen.contains(&s.name) {
            "up  "
        } else {
            "down"
        };
        let act = if s.active { "active" } else { "off   " };
        println!("{live}\t{act}\t{}", s.name);
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

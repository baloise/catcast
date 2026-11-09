//! `catc.toml` and `aliases.yaml` on disk.

use anyhow::{anyhow, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;

const CONFIG_FILE: &str = "catc.toml";
const ALIASES_FILE: &str = "aliases.yaml";
const CONFIG_ENV: &str = "CATC_CONFIG";

/// Where releases are looked up when nothing else is configured.
pub const DEFAULT_UPDATE_URL: &str = "https://github.com/baloise/catcast";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatcConfig {
    pub socks: String,
    /// Bare repository URL (no trailing slash) whose `/releases/latest`
    /// redirect names the newest version. Used by `stage list --probe` and
    /// `catc version`. An empty string disables every release lookup, so
    /// catc never talks to anything but the broker.
    #[serde(default = "default_update_url")]
    pub update_url: String,
    /// catproxy origin (e.g. `https://catproxy.example.workers.dev`) that
    /// stages download updates through, via its `/b64/` route, so a
    /// content-sniffing corporate proxy sees text rather than an executable.
    /// Unset: stages fetch the raw release asset directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_proxy: Option<String>,
    #[serde(default)]
    pub stages: Vec<Stage>,
}

impl Default for CatcConfig {
    fn default() -> Self {
        Self {
            socks: String::new(),
            update_url: default_update_url(),
            update_proxy: None,
            stages: Vec::new(),
        }
    }
}

fn default_update_url() -> String {
    DEFAULT_UPDATE_URL.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stage {
    pub name: String,
    #[serde(default = "true_default")]
    pub active: bool,
    /// A mirror stage (`catstage --mirror`). Commands sent to all active
    /// stages skip it; `stage update` and `status` still include it. Set by
    /// `relay pair`, and by `status` when the stage reports itself a mirror.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mirror: bool,
}

fn true_default() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Aliases(pub BTreeMap<String, String>);

fn platform_config_dir() -> Result<PathBuf> {
    ProjectDirs::from("", "", "catc")
        .map(|d| d.config_dir().to_path_buf())
        .ok_or_else(|| anyhow!("could not resolve a config directory for catc"))
}

/// Path to `catc.toml`, resolved by precedence:
///   1. `$CATC_CONFIG` (used as-is, may not exist yet)
///   2. `./catc.toml` in cwd (only if it already exists)
///   3. platform default (e.g. `~/.config/catc/catc.toml` on Linux)
///
/// `load()` and `save()` both call this, so reads and writes target the
/// same file.
pub fn config_path() -> Result<PathBuf> {
    if let Some(p) = env::var_os(CONFIG_ENV) {
        return Ok(PathBuf::from(p));
    }
    if let Ok(cwd) = env::current_dir() {
        let candidate = cwd.join(CONFIG_FILE);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Ok(platform_config_dir()?.join(CONFIG_FILE))
}

/// Path to `aliases.yaml`, anchored to the same directory as the
/// resolved `config_path()` so the pair stays together.
pub fn aliases_path() -> Result<PathBuf> {
    let cfg = config_path()?;
    let dir = cfg
        .parent()
        .ok_or_else(|| anyhow!("config path {} has no parent", cfg.display()))?;
    Ok(dir.join(ALIASES_FILE))
}

/// Directory holding the working copies `catc config edit` / `catc logic
/// edit` keep for one stage: `stages/<name>/` beside the resolved
/// `catc.toml`, so a repo-local catc.toml keeps its copies in the repo and
/// the platform default keeps them under the user's home.
pub fn stage_dir(name: &str) -> Result<PathBuf> {
    let cfg = config_path()?;
    let dir = cfg
        .parent()
        .ok_or_else(|| anyhow!("config path {} has no parent", cfg.display()))?;
    Ok(dir.join("stages").join(safe_name(name)))
}

/// A stage name as a single path component: anything outside
/// `[A-Za-z0-9._-]` becomes `_`, and a name of only dots is prefixed so it
/// can never mean `.` or `..`.
pub fn safe_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.chars().all(|c| c == '.') {
        format!("_{s}")
    } else {
        s
    }
}

impl CatcConfig {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        let s = fs::read_to_string(&path)
            .with_context(|| format!("read {} (run `catc init` first?)", path.display()))?;
        let cfg: CatcConfig =
            toml::from_str(&s).with_context(|| format!("parse {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let s = toml::to_string_pretty(self)?;
        fs::write(&path, s).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    pub fn stage_mut(&mut self, name: &str) -> Option<&mut Stage> {
        self.stages.iter_mut().find(|s| s.name == name)
    }

    pub fn stage(&self, name: &str) -> Option<&Stage> {
        self.stages.iter().find(|s| s.name == name)
    }
}

impl Aliases {
    pub fn load() -> Result<Self> {
        let path = aliases_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let s = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let inner: BTreeMap<String, String> = serde_yaml::from_str(&s)?;
        Ok(Self(inner))
    }

    pub fn save(&self) -> Result<()> {
        let path = aliases_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let s = serde_yaml::to_string(&self.0)?;
        fs::write(&path, s)?;
        Ok(())
    }
}

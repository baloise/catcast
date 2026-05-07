//! `catc.toml` and `aliases.yaml` on disk.

use anyhow::{anyhow, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatcConfig {
    pub socks: String,
    #[serde(default)]
    pub stages: Vec<Stage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stage {
    pub name: String,
    #[serde(default = "true_default")]
    pub active: bool,
}

fn true_default() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Aliases(pub BTreeMap<String, String>);

fn dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "baloise", "catc")
        .ok_or_else(|| anyhow!("could not resolve a config directory for catc"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(dirs()?.config_dir().join("catc.toml"))
}

pub fn aliases_path() -> Result<PathBuf> {
    Ok(dirs()?.config_dir().join("aliases.yaml"))
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

    pub fn active_names(&self) -> Vec<String> {
        self.stages
            .iter()
            .filter(|s| s.active)
            .map(|s| s.name.clone())
            .collect()
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

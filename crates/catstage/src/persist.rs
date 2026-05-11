//! On-disk persistence for catstage.
//!
//! Three files live in the OS config dir (`%APPDATA%\catcast` on Windows,
//! `~/.config/catcast` on Linux/macOS — resolved via [`directories`]):
//!
//! - `config.yaml`   — current `Config` YAML source (as received).
//! - `logic.rhai`    — current Rhai logic source.
//! - `state.json`    — last persisted [`State`] snapshot.
//!
//! Persist writes are synchronous and best-effort: failures are surfaced as
//! `anyhow::Error` and the caller decides whether to log+continue.

use anyhow::{Context, Result};
use catcast_core::State;
use directories::ProjectDirs;
use std::fs;
use std::path::{Path, PathBuf};

/// Locate (and create on demand) the catstage config directory.
pub fn config_dir() -> Result<PathBuf> {
    let proj = ProjectDirs::from("", "", "catcast")
        .context("could not resolve a per-user config directory")?;
    let dir = proj.config_dir().to_path_buf();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.yaml"))
}
pub fn logic_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("logic.rhai"))
}
pub fn state_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("state.json"))
}

fn read_if_exists(p: &Path) -> Result<Option<String>> {
    if p.exists() {
        Ok(Some(
            fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
        ))
    } else {
        Ok(None)
    }
}

pub fn load_config_yaml() -> Result<Option<String>> {
    read_if_exists(&config_path()?)
}

pub fn load_logic_rhai() -> Result<Option<String>> {
    read_if_exists(&logic_path()?)
}

pub fn load_state() -> Result<Option<State>> {
    let Some(text) = read_if_exists(&state_path()?)? else {
        return Ok(None);
    };
    let state: State = serde_json::from_str(&text).context("parsing state.json")?;
    Ok(Some(state))
}

fn write_atomic(p: &Path, content: &str) -> Result<()> {
    // Write to <name>.tmp then rename — keeps existing file intact on crash.
    let tmp = p.with_extension(format!(
        "{}.tmp",
        p.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, p)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), p.display()))?;
    Ok(())
}

pub fn save_config_yaml(yaml: &str) -> Result<()> {
    write_atomic(&config_path()?, yaml)
}

pub fn save_logic_rhai(rhai: &str) -> Result<()> {
    write_atomic(&logic_path()?, rhai)
}

pub fn save_state(state: &State) -> Result<()> {
    let s = serde_json::to_string_pretty(state).context("encoding state.json")?;
    write_atomic(&state_path()?, &s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_state_roundtrip() {
        // Use a tempdir directly — we don't want to touch real $HOME.
        let dir = tempdir();
        let state = State::fresh("kitchen", "0.0.1");
        let path = dir.join("state.json");
        let s = serde_json::to_string(&state).unwrap();
        std::fs::write(&path, s).unwrap();
        let read: State = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read.name, "kitchen");
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("catstage-test-{}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

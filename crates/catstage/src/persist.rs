//! On-disk persistence for catstage.
//!
//! Three files live in the OS config dir (`%APPDATA%\catcast` on Windows,
//! `~/.config/catcast` on Linux/macOS — resolved via [`directories`]):
//!
//! - `config.yaml`   — current `Config` YAML source (as received).
//! - `logic.rhai`    — current Rhai logic source.
//! - `state.json`    — last persisted [`State`] snapshot.
//! - `socks.url`     — last `--socks` broker URL, used when started without one.
//! - `relay.json`    — relay pairing (mirror name, frame secret, timing).
//! - `mirror.json`   — mirror pairing (frame secret).
//! - `mirror.name`   — the generated name of a `--mirror` stage.
//! - `frame.json` + `tiles/` — the mirror's last frame and its tile store.
//!
//! Persist writes are synchronous and best-effort: failures are surfaced as
//! `anyhow::Error` and the caller decides whether to log+continue.

use anyhow::{Context, Result};
use catcast_core::{Mode, State};
use directories::ProjectDirs;
use std::fs;
use std::io::Write;
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
pub fn socks_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("socks.url"))
}

pub fn relay_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("relay.json"))
}
pub fn mirror_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("mirror.json"))
}
pub fn mirror_name_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("mirror.name"))
}
pub fn frame_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("frame.json"))
}
pub fn tiles_dir() -> Result<PathBuf> {
    let dir = config_dir()?.join("tiles");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Read a JSON file into `T`; `None` when it does not exist.
pub fn load_json<T: serde::de::DeserializeOwned>(p: &Path) -> Result<Option<T>> {
    let Some(text) = read_if_exists(p)? else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| format!("parsing {}", p.display()))
}

pub fn save_json<T: serde::Serialize>(p: &Path, value: &T) -> Result<()> {
    let s = serde_json::to_string_pretty(value).context("encoding JSON")?;
    write_atomic(p, &s)
}

/// Remove `p` if present.
pub fn remove(p: &Path) -> Result<()> {
    match fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", p.display())),
    }
}

/// The remembered name of a `--mirror` stage, created on first use: 20
/// characters from an unambiguous alphabet (~99 bits). The name is the key
/// under which the frame secret reaches the mirror, so it must not be
/// guessable like a hostname.
pub fn mirror_name() -> Result<String> {
    let p = mirror_name_path()?;
    if let Some(n) = read_if_exists(&p)?.map(|s| s.trim().to_owned()) {
        if !n.is_empty() {
            return Ok(n);
        }
    }
    let n = random_name();
    write_atomic(&p, &format!("{n}\n"))?;
    Ok(n)
}

pub fn random_name() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    (0..20)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
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

/// The remembered broker URL; `None` when never saved or the file is blank.
pub fn load_socks() -> Result<Option<String>> {
    Ok(read_if_exists(&socks_path()?)?
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty()))
}

pub fn load_state() -> Result<Option<State>> {
    let Some(text) = read_if_exists(&state_path()?)? else {
        return Ok(None);
    };
    // Parse loosely as serde_json::Value so we can detect the v1 shape
    // (paused/manual booleans) and migrate it to v2's `mode` enum before
    // strict-deserializing into State. Old stages that wrote `state.json`
    // before this commit produced schema=1; new writes are schema=2.
    let v: serde_json::Value = serde_json::from_str(&text).context("parsing state.json as JSON")?;
    let migrated = migrate_state_v1_to_v2(v);
    let state: State = serde_json::from_value(migrated).context("parsing state.json into State")?;
    Ok(Some(state))
}

/// If `v` looks like a v1 state.json (no `mode` key; has legacy `paused` /
/// `manual` booleans), rewrite it into the v2 shape. v2-and-newer pass
/// through untouched.
fn migrate_state_v1_to_v2(mut v: serde_json::Value) -> serde_json::Value {
    let obj = match v.as_object_mut() {
        Some(o) => o,
        None => return v,
    };
    if obj.contains_key("mode") {
        return v;
    }
    let manual = obj
        .remove("manual")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let paused = obj
        .remove("paused")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let mode = if manual || paused {
        Mode::Paused
    } else {
        Mode::Playing
    };
    obj.insert("mode".into(), serde_json::to_value(mode).unwrap());
    obj.insert(
        "schema".into(),
        serde_json::to_value(catcast_core::state::CURRENT_STATE_SCHEMA).unwrap(),
    );
    v
}

fn write_atomic(p: &Path, content: &str) -> Result<()> {
    // Write to <name>.<pid>.tmp, flush it to disk, then rename over the
    // target — keeps the existing file intact on crash.
    //
    // The flush matters on kiosks that lose power: without it the rename
    // can reach the disk before the data does, leaving a target full of
    // NUL bytes. The pid matters during a self-update, when the old and
    // the new stage run side by side and would share one temp file.
    let tmp = p.with_extension(format!(
        "{}.{}.tmp",
        p.extension().and_then(|s| s.to_str()).unwrap_or(""),
        std::process::id()
    ));
    let mut f = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(content.as_bytes())
        .and_then(|()| f.sync_all())
        .with_context(|| format!("writing {}", tmp.display()))?;
    drop(f);
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

pub fn save_socks(url: &str) -> Result<()> {
    write_atomic(&socks_path()?, &format!("{url}\n"))
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

    #[test]
    fn write_atomic_replaces_target_and_leaves_no_temp_file() {
        let dir = tempdir().join("write-atomic");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(&path, "old").unwrap();
        write_atomic(&path, "{\"new\":true}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"new\":true}");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("state.json")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrates_v1_paused_to_mode_paused() {
        let v1 = serde_json::json!({
            "schema": 1,
            "name": "kitchen",
            "version": "0.0.1",
            "current_url": "https://example.com",
            "paused": true,
            "manual": false,
            "has_logic": true,
            "has_config": true,
            "since": 0,
        });
        let migrated = migrate_state_v1_to_v2(v1);
        let s: State = serde_json::from_value(migrated).unwrap();
        assert_eq!(s.mode, Mode::Paused);
        assert_eq!(s.schema, catcast_core::state::CURRENT_STATE_SCHEMA);
    }

    #[test]
    fn migrates_v1_manual_to_mode_paused() {
        let v1 = serde_json::json!({
            "schema": 1,
            "name": "kitchen",
            "version": "0.0.1",
            "current_url": null,
            "paused": false,
            "manual": true,
            "has_logic": false,
            "has_config": false,
            "since": 0,
        });
        let migrated = migrate_state_v1_to_v2(v1);
        let s: State = serde_json::from_value(migrated).unwrap();
        assert_eq!(s.mode, Mode::Paused);
    }

    #[test]
    fn migrates_v1_neither_to_mode_playing() {
        let v1 = serde_json::json!({
            "schema": 1,
            "name": "kitchen",
            "version": "0.0.1",
            "current_url": null,
            "paused": false,
            "manual": false,
            "has_logic": false,
            "has_config": false,
            "since": 0,
        });
        let migrated = migrate_state_v1_to_v2(v1);
        let s: State = serde_json::from_value(migrated).unwrap();
        assert_eq!(s.mode, Mode::Playing);
    }

    #[test]
    fn random_names_are_long_distinct_and_unambiguous() {
        let a = random_name();
        let b = random_name();
        assert_eq!(a.len(), 20);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| !"01ilo".contains(c)), "{a}");
    }

    #[test]
    fn json_helpers_round_trip_and_remove() {
        let dir = tempdir().join("json-helpers");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.json");
        assert_eq!(load_json::<Vec<u32>>(&p).unwrap(), None);
        save_json(&p, &vec![1u32, 2]).unwrap();
        assert_eq!(load_json::<Vec<u32>>(&p).unwrap(), Some(vec![1, 2]));
        remove(&p).unwrap();
        remove(&p).unwrap();
        assert!(!p.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("catstage-test-{}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

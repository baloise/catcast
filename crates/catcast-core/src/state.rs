use serde::{Deserialize, Serialize};

pub const CURRENT_STATE_SCHEMA: u32 = 2;

/// What the kiosk is currently doing.
///
/// - `Playing` — rotation runs, scheduler advances at slot boundaries.
/// - `Paused` — rotation frozen; advances suppressed until the operator
///   hits `play` (or sends a Nav/back/forward). Used both when the operator
///   freezes the current content URL and when the kiosk is on the about page
///   (`catc about` / F1). `current_url` in `State` tells you which.
///
/// Wire form is snake_case (`"playing"`, `"paused"`) so a hand-edited
/// `state.json` is greppable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Playing,
    Paused,
}

/// Stage runtime state, persisted on disk and emitted to the CLI on `GetState`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct State {
    pub schema: u32,
    pub name: String,
    pub version: String,
    /// Release-asset suffix of the running build (see [`platform_tag`]).
    /// Added after schema 2 without a bump: `schema` tracks *shape* changes
    /// that need a migration, and a defaulted additive field is readable by
    /// both older and newer binaries. Stages predating the field report `""`.
    #[serde(default)]
    pub platform: String,
    pub current_url: Option<String>,
    pub mode: Mode,
    pub has_logic: bool,
    pub has_config: bool,
    /// Unix-ms when the stage entered the current state.
    pub since: i64,
}

impl State {
    pub fn fresh(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            schema: CURRENT_STATE_SCHEMA,
            name: name.into(),
            version: version.into(),
            platform: platform_tag(),
            current_url: None,
            mode: Mode::Playing,
            has_logic: false,
            has_config: false,
            since: chrono::Utc::now().timestamp_millis(),
        }
    }
}

/// Release-asset suffix of the running build: `windows-x64`, `linux-x64`,
/// `macos-arm64`, ... Must match the `suffix` values of the matrix in
/// `.github/workflows/release.yml`; `catc stage update` uses it to pick the
/// asset for a stage.
pub fn platform_tag() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{}-{arch}", std::env::consts::OS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_tag_matches_release_matrix() {
        let tag = platform_tag();
        if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            assert_eq!(tag, "windows-x64");
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(tag, "linux-x64");
        } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            assert_eq!(tag, "macos-arm64");
        }
        assert!(tag.contains('-'));
    }

    #[test]
    fn state_without_platform_field_deserialises() {
        let json = r#"{"schema":2,"name":"k","version":"0.1.1","current_url":null,
            "mode":"playing","has_logic":false,"has_config":false,"since":0}"#;
        let s: State = serde_json::from_str(json).unwrap();
        assert_eq!(s.platform, "");
        assert_eq!(s.version, "0.1.1");
    }
}

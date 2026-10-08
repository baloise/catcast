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
    /// Where the webview actually is: the URL of the last finished top-level
    /// load. Differs from `current_url` while a corporate proxy or SSO
    /// redirect holds the kiosk elsewhere (see [`State::stuck`]). Live data:
    /// additive like `platform`, never meaningful from disk.
    #[serde(default)]
    pub actual_url: Option<String>,
    pub mode: Mode,
    pub has_logic: bool,
    pub has_config: bool,
    /// Unix-ms when the stage entered the current state.
    pub since: i64,
}

impl State {
    /// True when the webview sits on a different host than the one the
    /// scheduler sent it to, and that host is not the bundled about page.
    /// A proxy login (`gateway.zscaler.net`, `login.microsoftonline.com`,
    /// ...) that never returns to the content URL looks exactly like this.
    pub fn stuck(&self) -> bool {
        stuck(self.current_url.as_deref(), self.actual_url.as_deref())
    }

    pub fn fresh(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            schema: CURRENT_STATE_SCHEMA,
            name: name.into(),
            version: version.into(),
            platform: platform_tag(),
            current_url: None,
            actual_url: None,
            mode: Mode::Playing,
            has_logic: false,
            has_config: false,
            since: chrono::Utc::now().timestamp_millis(),
        }
    }
}

/// The catstage about page, as the webview reports it: `tauri://localhost`
/// on Linux/macOS, `http://tauri.localhost` on Windows.
pub fn is_about_url(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(u) => {
            (u.scheme() == "tauri" && u.host_str() == Some("localhost"))
                || (u.scheme() == "http" && u.host_str() == Some("tauri.localhost"))
        }
        Err(_) => false,
    }
}

/// Host of `url`, lowercased; `None` for unparsable or host-less URLs.
pub fn url_host(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .host_str()
        .map(|h| h.to_ascii_lowercase())
}

/// See [`State::stuck`]. Both URLs known, hosts differ, actual is not the
/// about page.
pub fn stuck(intended: Option<&str>, actual: Option<&str>) -> bool {
    let (Some(intended), Some(actual)) = (intended, actual) else {
        return false;
    };
    if is_about_url(actual) {
        return false;
    }
    match (url_host(intended), url_host(actual)) {
        (Some(a), Some(b)) => a != b,
        _ => false,
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
        assert_eq!(s.actual_url, None);
        assert_eq!(s.version, "0.1.1");
    }

    #[test]
    fn stuck_rule() {
        let menu = "https://catproxy.example.workers.dev/https://pages.example.com/menu.html";
        assert!(stuck(
            Some(menu),
            Some("https://login.microsoftonline.com/x/saml2/?whr=example.com")
        ));
        // Same host, different path: a normal page load.
        assert!(!stuck(
            Some(menu),
            Some("https://catproxy.example.workers.dev/https://pages.example.com/other.html")
        ));
        // Host comparison is case-insensitive.
        assert!(!stuck(
            Some(menu),
            Some("https://CATPROXY.example.workers.dev/")
        ));
        // Parked on the about page is not stuck.
        assert!(!stuck(Some(menu), Some("http://tauri.localhost/")));
        assert!(!stuck(Some(menu), Some("tauri://localhost/#idle")));
        // Unknown either side: no verdict.
        assert!(!stuck(None, Some(menu)));
        assert!(!stuck(Some(menu), None));
        assert!(!stuck(Some(menu), Some("about:blank")));
    }
}

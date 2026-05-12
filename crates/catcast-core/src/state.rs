use serde::{Deserialize, Serialize};

pub const CURRENT_STATE_SCHEMA: u32 = 2;

/// What the kiosk is currently doing.
///
/// Three explicit modes replace the older `paused` + `manual` boolean pair:
///
/// - `Playing` — rotation runs, scheduler advances at slot boundaries.
/// - `Paused` — frozen on the currently-shown rotation URL; advances
///   suppressed until the operator hits `play` (or sends a Nav/back/forward).
/// - `Idle` — kiosk parked on the bundled about page; "this screen is being
///   attended to" signal. Set by `NavAbout`.
///
/// Wire form is snake_case (`"playing"`, `"paused"`, `"idle"`) so a hand-
/// edited `state.json` is greppable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Playing,
    Paused,
    Idle,
}

/// Stage runtime state, persisted on disk and emitted to the CLI on `GetState`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct State {
    pub schema: u32,
    pub name: String,
    pub version: String,
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
            current_url: None,
            mode: Mode::Playing,
            has_logic: false,
            has_config: false,
            since: chrono::Utc::now().timestamp_millis(),
        }
    }
}

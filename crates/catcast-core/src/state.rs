use serde::{Deserialize, Serialize};

pub const CURRENT_STATE_SCHEMA: u32 = 1;

/// Stage runtime state, persisted on disk and emitted to the CLI on `GetState`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct State {
    pub schema: u32,
    pub name: String,
    pub version: String,
    pub current_url: Option<String>,
    pub paused: bool,
    pub manual: bool,
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
            paused: false,
            manual: false,
            has_logic: false,
            has_config: false,
            since: chrono::Utc::now().timestamp_millis(),
        }
    }
}

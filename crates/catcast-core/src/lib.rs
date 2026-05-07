//! Shared CatCast types: config schema, state, logic source.

pub mod config;
pub mod state;

pub use config::{Config, ConfigError, FixedEntry, RotationEntry, Warning};
pub use state::State;

//! The default Rhai logic `catc logic import` pushes, and recognition of the
//! defaults earlier releases shipped.
//!
//! A binary update never touches a stage's `logic.rhai`: the logic may be
//! custom, so overwriting it is the operator's call. What catc can do is tell
//! a stale *default* apart from a custom script and suggest the re-import.
//! Recognition is by hash of the normalised source, so only verbatim copies
//! of a shipped default count — any edit makes the script custom.

use sha2::{Digest, Sha256};

pub const DEFAULT_LOGIC: &str = include_str!("../../../default-logic/default.rhai");

/// Hash of [`DEFAULT_LOGIC`]. A test pins it, so editing `default.rhai`
/// fails the build until the old value moves to [`PREVIOUS`].
const CURRENT_SHA256: &str = "0ae069e6dac303c1548e2e365918c4079ad25ec6b859c2d7177deb7e7ac62f93";

/// Hashes of earlier shipped defaults, with the releases that shipped them.
const PREVIOUS: &[(&str, &str)] = &[(
    "c70f6f53e5aeb8d0390ff7177e0300e6afa363b0f9afc6ad6df91e92e770fad8",
    "v0.0.2–v0.1.0",
)];

#[derive(Debug, PartialEq, Eq)]
pub enum LogicStatus {
    /// Matches catc's built-in default.
    Current,
    /// A verbatim earlier default; carries the releases that shipped it.
    OutdatedDefault(&'static str),
    /// Anything else — left alone.
    Custom,
    /// The stage has no logic loaded.
    Missing,
}

/// Line endings and trailing whitespace are ignored: a default pushed from a
/// CRLF checkout is still the default.
fn digest(rhai: &str) -> String {
    let normalised = rhai.replace("\r\n", "\n");
    Sha256::digest(normalised.trim_end().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn classify(rhai: Option<&str>) -> LogicStatus {
    let Some(rhai) = rhai else {
        return LogicStatus::Missing;
    };
    let h = digest(rhai);
    if h == CURRENT_SHA256 {
        return LogicStatus::Current;
    }
    match PREVIOUS.iter().find(|(p, _)| *p == h) {
        Some((_, releases)) => LogicStatus::OutdatedDefault(releases),
        None => LogicStatus::Custom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_hash_is_pinned() {
        assert_eq!(
            digest(DEFAULT_LOGIC),
            CURRENT_SHA256,
            "default.rhai changed: move CURRENT_SHA256 into PREVIOUS (with the \
             releases that shipped it) and pin the new hash"
        );
    }

    #[test]
    fn current_is_not_listed_as_previous() {
        assert!(PREVIOUS.iter().all(|(h, _)| *h != CURRENT_SHA256));
    }

    #[test]
    fn classify_ignores_line_endings_and_trailing_space() {
        let crlf = DEFAULT_LOGIC.replace('\n', "\r\n") + "\r\n\r\n";
        assert_eq!(classify(Some(&crlf)), LogicStatus::Current);
    }

    #[test]
    fn classify_flags_edits_as_custom() {
        let edited = DEFAULT_LOGIC.replacen("fn ", "fn  ", 1);
        assert_eq!(classify(Some(&edited)), LogicStatus::Custom);
        assert_eq!(classify(None), LogicStatus::Missing);
    }

    #[test]
    fn classify_recognises_the_v0_1_0_default() {
        // The one line that changed in 4ad5dfb.
        let old = DEFAULT_LOGIC.replace(
            "    unit.trim(); // trims in place; returns ()\n    switch unit {",
            "    switch unit.trim() {",
        );
        assert_eq!(
            classify(Some(&old)),
            LogicStatus::OutdatedDefault("v0.0.2–v0.1.0")
        );
    }
}

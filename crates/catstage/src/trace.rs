//! Ring buffer of the kiosk webview's top-level navigations.
//!
//! Three kinds of entry (see [`TraceKind`]): what the scheduler asked for,
//! every navigation the webview started (server redirects included, so a
//! corporate-proxy login or an SSO hop is visible), and what finished
//! loading. `catc trace` pulls it over the broker; `State::actual_url` is
//! the one-line summary. Bounded so a stage that runs for months stays
//! small; the newest entry wins.

use catcast_proto::{TraceEntry, TraceKind};
use std::collections::VecDeque;
use std::sync::Mutex;
use tauri::Manager;

pub const CAPACITY: usize = 200;

pub struct NavTrace {
    entries: Mutex<VecDeque<TraceEntry>>,
}

impl Default for NavTrace {
    fn default() -> Self {
        Self::new()
    }
}

impl NavTrace {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(CAPACITY)),
        }
    }

    pub fn record(&self, kind: TraceKind, url: impl Into<String>) {
        let mut entries = self.entries.lock().expect("NavTrace poisoned");
        if entries.len() == CAPACITY {
            entries.pop_front();
        }
        entries.push_back(TraceEntry {
            ts: chrono::Utc::now().timestamp_millis(),
            kind,
            url: url.into(),
        });
    }

    /// Oldest first.
    pub fn snapshot(&self) -> Vec<TraceEntry> {
        self.entries
            .lock()
            .expect("NavTrace poisoned")
            .iter()
            .cloned()
            .collect()
    }
}

/// Record against the app-managed trace; a no-op before `setup` manages it.
pub fn record(app: &tauri::AppHandle, kind: TraceKind, url: &str) {
    if let Some(t) = app.try_state::<NavTrace>() {
        t.record(kind, url);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_newest_entries() {
        let t = NavTrace::new();
        for i in 0..(CAPACITY + 5) {
            t.record(TraceKind::Started, format!("https://h/{i}"));
        }
        let snap = t.snapshot();
        assert_eq!(snap.len(), CAPACITY);
        assert_eq!(snap.first().unwrap().url, "https://h/5");
        assert_eq!(
            snap.last().unwrap().url,
            format!("https://h/{}", CAPACITY + 4)
        );
    }

    #[test]
    fn snapshot_is_oldest_first() {
        let t = NavTrace::new();
        t.record(TraceKind::Intended, "https://a/");
        t.record(TraceKind::Started, "https://a/");
        t.record(TraceKind::Loaded, "https://b/");
        let kinds: Vec<TraceKind> = t.snapshot().iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![TraceKind::Intended, TraceKind::Started, TraceKind::Loaded]
        );
    }
}

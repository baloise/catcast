//! `catc status`: broker, stages and their mirrors as one aligned table.
//!
//! What it can show is bounded by the design: the broker keeps no list of
//! who is in a room, and a stage's name is its key, so only stages catc
//! knows by name can be probed. A relay reports its mirror's full name,
//! which is enough to probe the mirror even when it is not registered. A
//! mirror never learns its relay's name, so the tree is built from the
//! relay side.

use crate::cfg::{config_path, CatcConfig};
use crate::net::{keys_for, Broker};
use crate::release;
use anyhow::{anyhow, Result};
use catcast_core::State;
use catcast_proto::Message;
use semver::Version;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long to wait for each probe round.
const PROBE_WAIT: Duration = Duration::from_secs(3);
const SEEN_FILE: &str = "seen.json";

/// What one probe round learned about a stage. `Some(None)`: something
/// decrypted under its key but no `State` arrived (answers, but too old to
/// say more); `None` in the map: no answer at all.
pub type Replies = HashMap<String, Option<State>>;

/// Last time each stage answered, unix-ms, kept beside `catc.toml`.
pub type Seen = BTreeMap<String, i64>;

pub async fn cmd_status() -> Result<()> {
    let cfg = CatcConfig::load()?;
    let cfg_path = config_path()?;
    let seen_path = seen_path(&cfg_path);
    let mut seen = load_seen(&seen_path);
    let now = chrono::Utc::now().timestamp_millis();

    let t0 = Instant::now();
    let conn = Broker::connect(&cfg.socks).await;
    let connect_ms = t0.elapsed().as_millis();

    let (replies, latest, broker_err) = match conn {
        Ok(mut br) => {
            let latest = release::lookup(&cfg.update_url);
            let probing = async {
                let names: Vec<String> = cfg.stages.iter().map(|s| s.name.clone()).collect();
                let mut replies = probe(&mut br, &names).await?;
                // Second round: mirrors a relay named that catc does not know.
                let extra: Vec<String> = mirrors_of(&replies)
                    .into_iter()
                    .filter(|m| !replies.contains_key(m) && cfg.stage(m).is_none())
                    .collect();
                if !extra.is_empty() {
                    replies.extend(probe(&mut br, &extra).await?);
                }
                anyhow::Ok(replies)
            };
            let (replies, latest) = tokio::join!(probing, latest);
            br.close().await;
            (replies?, latest, None)
        }
        Err(e) => (HashMap::new(), None, Some(e)),
    };
    match &broker_err {
        None => println!("broker   {}  (connected in {connect_ms} ms)", cfg.socks),
        Some(e) => println!("broker   {}  UNREACHABLE: {e:#}", cfg.socks),
    }
    println!("config   {}", cfg_path.display());
    let latest = match latest {
        Some(Ok(v)) => {
            println!("release  v{v} (catc v{})", env!("CARGO_PKG_VERSION"));
            Some(v)
        }
        Some(Err(e)) => {
            println!("release  unavailable ({e:#})");
            None
        }
        None => None,
    };
    println!();

    for name in replies.keys() {
        seen.insert(name.clone(), now);
    }
    if !replies.is_empty() {
        if let Err(e) = save_seen(&seen_path, &seen) {
            eprintln!("catc: could not save {}: {e:#}", seen_path.display());
        }
    }

    // Learn roles: a registered stage that says it is a mirror gets marked,
    // so commands for all active stages stop reaching it.
    for s in &cfg.stages {
        let is_mirror = matches!(replies.get(&s.name), Some(Some(st)) if st.mirror);
        if is_mirror && !s.mirror {
            if let Err(e) = crate::cli::mark_mirror(&s.name) {
                eprintln!("catc: could not mark {} as a mirror: {e:#}", s.name);
            }
        }
    }

    let stages: Vec<(String, bool)> = cfg
        .stages
        .iter()
        .map(|s| (s.name.clone(), s.active))
        .collect();
    if stages.is_empty() {
        println!("(no stages registered — run `catc stage add <name>`)");
    } else {
        let probed = broker_err.is_none();
        let rows = build_rows(&stages, &replies, &seen, probed, latest.as_ref(), now);
        print!("{}", render(&rows));
    }
    match broker_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

async fn probe(br: &mut Broker, names: &[String]) -> Result<Replies> {
    let keys = keys_for(names)?;
    for n in names {
        br.send(&keys[n], n, &Message::GetState).await?;
    }
    let mut out: Replies = HashMap::new();
    let want = names.len();
    br.collect_until(&keys, PROBE_WAIT, |name, pt| {
        let slot = out.entry(name.to_string()).or_default();
        if let Message::State(s) = pt.msg {
            keep_newest(slot, s);
        }
        out.values().filter(|v| v.is_some()).count() == want
    })
    .await;
    Ok(out)
}

/// Right after an update the old process can still answer next to the new
/// one. Keep the State of the newer process: the later start time when both
/// report one, else the one that does (only newer builds do), else the
/// higher version.
pub fn keep_newest(slot: &mut Option<State>, s: State) {
    let replace = match slot.as_ref() {
        None => true,
        Some(old) => match (old.started_ts, s.started_ts) {
            (Some(a), Some(b)) => b > a,
            (None, Some(_)) => true,
            (Some(_), None) => false,
            (None, None) => {
                let v = |st: &State| semver::Version::parse(&st.version).ok();
                v(&s) > v(old)
            }
        },
    };
    if replace {
        *slot = Some(s);
    }
}

/// Mirror names the responding relays point at, deduplicated, in order.
fn mirrors_of(replies: &Replies) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut names: Vec<&String> = replies.keys().collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|n| replies[n].as_ref()?.relay_to.clone())
        .filter(|m| seen.insert(m.clone()))
        .collect()
}

fn seen_path(cfg_path: &std::path::Path) -> PathBuf {
    cfg_path
        .parent()
        .map(|d| d.join(SEEN_FILE))
        .unwrap_or_else(|| PathBuf::from(SEEN_FILE))
}

fn load_seen(path: &std::path::Path) -> Seen {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_seen(path: &std::path::Path, seen: &Seen) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(seen)?)?;
    std::fs::rename(&tmp, path).map_err(|e| anyhow!("{e}"))
}

/// One table line. `depth` 1 is a mirror drawn under its relay.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub depth: usize,
    pub cells: [String; COLS],
}

const COLS: usize = 11;
const HEADER: [&str; COLS] = [
    "STAGE", "STATUS", "SEEN", "VERSION", "UPDATE", "PLATFORM", "UPTIME", "LINK", "FRAME", "CACHE",
    "ON",
];

/// Registered stages in `catc.toml` order; a mirror that a responding relay
/// points at is drawn under that relay instead of on its own line, and a
/// mirror that is not registered is added there too.
pub fn build_rows(
    stages: &[(String, bool)],
    replies: &Replies,
    seen: &Seen,
    probed: bool,
    latest: Option<&Version>,
    now: i64,
) -> Vec<Row> {
    let registered: HashSet<&str> = stages.iter().map(|(n, _)| n.as_str()).collect();
    let nested: HashSet<String> = stages
        .iter()
        .filter_map(|(n, _)| replies.get(n)?.as_ref()?.relay_to.clone())
        .collect();
    let mut rows = Vec::new();
    for (name, active) in stages {
        if nested.contains(name) {
            continue;
        }
        let note = if *active { "" } else { " (off)" };
        rows.push(row(0, name, note, replies, seen, probed, latest, now));
        let mirror = replies
            .get(name)
            .and_then(|s| s.as_ref())
            .and_then(|s| s.relay_to.as_ref());
        if let Some(m) = mirror {
            let active = stages.iter().find(|(n, _)| n == m).map(|(_, a)| *a);
            let note = match active {
                None if !registered.contains(m.as_str()) => " (not registered)",
                Some(false) => " (off)",
                _ => "",
            };
            rows.push(row(1, m, note, replies, seen, probed, latest, now));
        }
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn row(
    depth: usize,
    name: &str,
    note: &str,
    replies: &Replies,
    seen: &Seen,
    probed: bool,
    latest: Option<&Version>,
    now: i64,
) -> Row {
    let dash = || "-".to_string();
    let label = format!("{name}{note}");
    let last_seen = |prefix: &str| match seen.get(name) {
        Some(ts) => format!("{prefix}{} ago", compact((now - ts).max(0) / 1000)),
        None => format!("{prefix}never"),
    };
    let cells = match replies.get(name) {
        None => {
            let status = if probed { "down" } else { "?" };
            let mut c: [String; COLS] = Default::default();
            c[0] = label;
            c[1] = status.into();
            c[2] = last_seen("");
            for cell in c.iter_mut().skip(3) {
                *cell = dash();
            }
            // Role unknown: leave the relay columns blank.
            c[8].clear();
            c[9].clear();
            c
        }
        Some(None) => {
            let mut c: [String; COLS] = Default::default();
            c[0] = label;
            c[1] = "up".into();
            c[2] = "now".into();
            for cell in c.iter_mut().skip(3) {
                *cell = "?".into();
            }
            c[8].clear();
            c[9].clear();
            c
        }
        Some(Some(st)) => {
            let age = |ts: Option<i64>| match ts {
                Some(t) => compact((now - t).max(0) / 1000),
                None => dash(),
            };
            let update = match latest {
                Some(v) => match release::freshness(&st.version, v) {
                    release::Freshness::Outdated => format!("→ v{v}"),
                    release::Freshness::Current => "current".into(),
                    release::Freshness::Ahead => "ahead".into(),
                    release::Freshness::Unknown => "?".into(),
                },
                None => dash(),
            };
            let link = match (st.connected_ts, st.started_ts) {
                (Some(c), _) => match st.reconnects {
                    0 => compact((now - c).max(0) / 1000),
                    1 => format!("{}, 1 drop", compact((now - c).max(0) / 1000)),
                    n => format!("{}, {n} drops", compact((now - c).max(0) / 1000)),
                },
                // Answered, so it is connected; an old stage just can't say
                // since when.
                (None, None) => dash(),
                (None, Some(_)) => "?".into(),
            };
            let frame = if st.relay_to.is_some() || st.mirror {
                match st.last_frame_ts {
                    Some(t) => compact((now - t).max(0) / 1000),
                    None => "none yet".into(),
                }
            } else {
                String::new()
            };
            let cache = match (st.cache_bytes, st.cache_cap) {
                (Some(b), Some(c)) => format!("{}/{} MB", mb(b), c >> 20),
                _ if st.mirror => dash(),
                _ => String::new(),
            };
            let on = match st
                .actual_url
                .as_deref()
                .and_then(catcast_core::state::url_host)
            {
                Some(h) if st.stuck() => format!("STUCK on {h}"),
                Some(_) if st.mirror => "mirror page".into(),
                Some(h) => h,
                None => dash(),
            };
            [
                label,
                "up".into(),
                "now".into(),
                format!("v{}", st.version),
                update,
                if st.platform.is_empty() {
                    dash()
                } else {
                    st.platform.clone()
                },
                age(st.started_ts),
                link,
                frame,
                cache,
                on,
            ]
        }
    };
    Row { depth, cells }
}

/// Megabytes with one decimal below 10, whole above.
fn mb(bytes: u64) -> String {
    let m = bytes as f64 / (1u64 << 20) as f64;
    if m < 10.0 {
        format!("{m:.1}")
    } else {
        format!("{m:.0}")
    }
}

/// `45s`, `12m`, `3h 4m`, `2d 5h`: two units at most.
pub fn compact(secs: i64) -> String {
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m"),
        (0, _, 0) => format!("{h}h"),
        (0, _, _) => format!("{h}h {m}m"),
        (_, 0, _) => format!("{d}d"),
        _ => format!("{d}d {h}h"),
    }
}

/// Fixed-width table. Widths count characters, so the tree glyphs and `→`
/// line up like ASCII; columns that are empty on every row are dropped.
pub fn render(rows: &[Row]) -> String {
    let lines: Vec<Vec<String>> = std::iter::once(HEADER.map(String::from).to_vec())
        .chain(rows.iter().enumerate().map(|(i, r)| {
            let mut cells = r.cells.to_vec();
            if r.depth > 0 {
                let last = rows.get(i + 1).is_none_or(|n| n.depth < r.depth);
                let glyph = if last { "└─ " } else { "├─ " };
                cells[0] = format!("{glyph}{}", cells[0]);
            }
            cells
        }))
        .collect();
    let keep: Vec<usize> = (0..COLS)
        .filter(|&c| lines.iter().skip(1).any(|l| !l[c].is_empty()))
        .collect();
    let width = |c: usize| {
        lines
            .iter()
            .map(|l| l[c].chars().count())
            .max()
            .unwrap_or(0)
    };
    let widths: HashMap<usize, usize> = keep.iter().map(|&c| (c, width(c))).collect();
    let mut out = String::new();
    for l in &lines {
        let mut line = String::new();
        for (k, &c) in keep.iter().enumerate() {
            let cell = &l[c];
            line.push_str(cell);
            if k + 1 < keep.len() {
                let pad = widths[&c] - cell.chars().count() + 2;
                line.extend(std::iter::repeat_n(' ', pad));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000_000_000;

    fn st(name: &str) -> State {
        let mut s = State::fresh(name, "0.5.0");
        s.platform = "windows-x64".into();
        s.started_ts = Some(NOW - 3 * 86_400_000 - 4 * 3_600_000);
        s.connected_ts = Some(NOW - 2 * 3_600_000);
        s.reconnects = 2;
        s.actual_url = Some("https://dash.example.com/x".into());
        s.current_url = s.actual_url.clone();
        s
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn newest_process_wins_over_the_one_being_replaced() {
        let mut old = State::fresh("k", "0.4.0");
        old.started_ts = None;
        let mut new = State::fresh("k", "0.6.0");
        new.started_ts = Some(NOW);
        // Either arrival order ends on the new process.
        let mut slot = None;
        keep_newest(&mut slot, new.clone());
        keep_newest(&mut slot, old.clone());
        assert_eq!(slot.as_ref().unwrap().version, "0.6.0");
        let mut slot = None;
        keep_newest(&mut slot, old.clone());
        keep_newest(&mut slot, new.clone());
        assert_eq!(slot.as_ref().unwrap().version, "0.6.0");
        // Both pre-health builds: the higher version.
        let mut slot = None;
        let mut newer = State::fresh("k", "0.5.0");
        newer.started_ts = None;
        keep_newest(&mut slot, newer);
        keep_newest(&mut slot, old);
        assert_eq!(slot.unwrap().version, "0.5.0");
    }

    #[test]
    fn compact_keeps_two_units() {
        assert_eq!(compact(45), "45s");
        assert_eq!(compact(12 * 60 + 5), "12m");
        assert_eq!(compact(3 * 3600), "3h");
        assert_eq!(compact(3 * 3600 + 4 * 60), "3h 4m");
        assert_eq!(compact(2 * 86400 + 5 * 3600 + 7), "2d 5h");
        assert_eq!(compact(86400 + 59), "1d");
    }

    #[test]
    fn mirror_nests_under_its_relay_and_unregistered_mirror_is_probed() {
        let mirror = "k7pqx3mabcdefghjkmnp";
        let mut relay = st("lobby");
        relay.relay_to = Some(mirror.into());
        relay.last_frame_ts = Some(NOW - 12_000);
        let mut m = st(mirror);
        m.platform = "linux-x64".into();
        m.mirror = true;
        m.last_frame_ts = Some(NOW - 12_000);
        m.cache_bytes = Some(11 << 20);
        m.cache_cap = Some(64 << 20);
        let replies: Replies = [
            ("lobby".to_string(), Some(relay)),
            (mirror.to_string(), Some(m)),
        ]
        .into();
        let stages = vec![("lobby".to_string(), true), ("kitchen".to_string(), false)];
        let seen: Seen = [("kitchen".to_string(), NOW - 90 * 60_000)].into();
        let rows = build_rows(&stages, &replies, &seen, true, Some(&v("0.5.0")), NOW);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].cells[0], "lobby");
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].cells[0], format!("{mirror} (not registered)"));
        assert_eq!(rows[1].cells[9], "11/64 MB");
        assert_eq!(rows[1].cells[8], "12s");
        assert_eq!(rows[2].cells[0], "kitchen (off)");
        assert_eq!(rows[2].cells[1], "down");
        assert_eq!(rows[2].cells[2], "1h 30m ago");
        assert_eq!(rows[0].cells[6], "3d 4h");
        assert_eq!(rows[0].cells[7], "2h, 2 drops");
    }

    #[test]
    fn registered_mirror_is_not_listed_twice() {
        let mut relay = st("lobby");
        relay.relay_to = Some("mirror-name-1234567".into());
        let mut m = st("mirror-name-1234567");
        m.mirror = true;
        let replies: Replies = [
            ("lobby".to_string(), Some(relay)),
            ("mirror-name-1234567".to_string(), Some(m)),
        ]
        .into();
        let stages = vec![
            ("mirror-name-1234567".to_string(), true),
            ("lobby".to_string(), true),
        ];
        let rows = build_rows(&stages, &replies, &Seen::new(), true, None, NOW);
        let names: Vec<&str> = rows.iter().map(|r| r.cells[0].as_str()).collect();
        assert_eq!(names, ["lobby", "mirror-name-1234567"]);
        assert_eq!(rows[1].depth, 1);
    }

    #[test]
    fn columns_align_with_tree_glyphs_and_long_names() {
        let mut relay = st("CH-WD-K000ljay7");
        relay.relay_to = Some("a-really-long-mirror-name-xyz".into());
        let mut m = st("a-really-long-mirror-name-xyz");
        m.mirror = true;
        m.version = "0.4.0".into();
        let replies: Replies = [
            ("CH-WD-K000ljay7".to_string(), Some(relay)),
            ("a-really-long-mirror-name-xyz".to_string(), Some(m)),
        ]
        .into();
        let stages = vec![
            ("CH-WD-K000ljay7".to_string(), true),
            ("tv".to_string(), true),
        ];
        let rows = build_rows(
            &stages,
            &replies,
            &Seen::new(),
            true,
            Some(&v("0.5.0")),
            NOW,
        );
        let out = render(&rows);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[2].starts_with("└─ a-really-long-mirror-name-xyz"));
        // Every column starts at the same character offset on every line.
        for col in ["STATUS", "VERSION", "UPDATE", "LINK", "ON"] {
            let at = lines[0].find(&format!("  {col}")).unwrap() + 2;
            let at = lines[0][..at].chars().count();
            for l in &lines[1..] {
                let before: String = l.chars().take(at).collect();
                let rest: String = l.chars().skip(at).collect();
                assert!(before.ends_with("  "), "{col} misaligned in {l:?}");
                assert!(!rest.starts_with(' '), "{col} misaligned in {l:?}");
            }
        }
        assert!(out.contains("→ v0.5.0"));
    }

    #[test]
    fn plain_stages_drop_relay_columns() {
        let replies: Replies = [("tv".to_string(), Some(st("tv")))].into();
        let rows = build_rows(
            &[("tv".to_string(), true)],
            &replies,
            &Seen::new(),
            true,
            None,
            NOW,
        );
        let out = render(&rows);
        assert!(!out.contains("FRAME"));
        assert!(!out.contains("CACHE"));
    }
}

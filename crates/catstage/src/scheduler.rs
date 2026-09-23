//! Rotation + fixed-schedule timer for catstage.
//!
//! A single tokio task drives:
//!
//! - round-robin rotation through `Plan::rotation`,
//! - cron-driven "fixed" entries (when the cron expression hits, swap to the
//!   entry's URL for its duration, then resume the round-robin),
//! - one-shot timed navigation (`NavTimed`) that overrides rotation+cron
//!   until its timer elapses, after which the previous rotation step
//!   continues.
//!
//! Control messages come in on an mpsc channel, so dispatch (from the socks
//! client) can wake the scheduler without owning any of its internal state.
//!
//! The scheduler emits each new "current URL" via a callback so the rest of
//! the binary can update [`State::current_url`] and notify the CLI.

use crate::logic::{CronJob, LogicHandle, Plan};
use catcast_core::config::parse_cron;
use catcast_core::Mode;
use chrono::{DateTime, Local};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{sleep_until, Instant};

/// Commands sent to the scheduler task.
#[derive(Debug)]
pub enum Cmd {
    /// Replace the plan and restart rotation from index 0.
    Rebuild(Plan),
    /// Override the current rotation entry for `secs`, then resume.
    TimedNav { url: String, secs: u64 },
    /// Navigate to a URL and stay there until something else changes.
    Nav(String),
    /// Freeze rotation on the currently-shown URL.
    Pause,
    /// Resume rotation. Equivalent to `SetMode(Playing)`.
    Play,
    /// Step the rotation by `delta` (typically -1 / +1). Bypasses pause /
    /// one-shot. The oneshot reply carries the URL of the new slot,
    /// or `None` if the rotation is empty.
    Step {
        delta: i32,
        reply: tokio::sync::oneshot::Sender<Option<String>>,
    },
    /// Stop the task. Used in tests.
    #[allow(dead_code)]
    Shutdown,
}

/// Callbacks fired by the scheduler when state changes. Kept as a trait so
/// tests can swap in a vector-collector instead of the real "update State,
/// broadcast" path.
pub trait Events: Send + Sync + 'static {
    fn on_url_change(&self, url: &str);
    fn on_mode_change(&self, mode: Mode);
}

/// Minimal no-op events sink. Useful for tests; the real binary wires this
/// to the persistence + broadcast paths in `main`.
#[allow(dead_code)]
pub struct NoopEvents;
impl Events for NoopEvents {
    fn on_url_change(&self, _url: &str) {}
    fn on_mode_change(&self, _mode: Mode) {}
}

/// Spawn the scheduler task. Returns the command sender; drop it to stop.
/// Kept as a convenience for tests / future single-step callers; the binary
/// uses [`spawn_with_rx`] so the tx half can be in `app.manage()`-d state
/// before the rx half is consumed.
#[allow(dead_code)]
pub fn spawn(
    plan: Plan,
    logic: Option<Arc<Mutex<Option<LogicHandle>>>>,
    events: Arc<dyn Events>,
) -> mpsc::Sender<Cmd> {
    let (tx, rx) = mpsc::channel(32);
    spawn_with_rx(rx, plan, logic, events);
    tx
}

/// Same as [`spawn`] but uses an externally-owned receiver. Used by main()
/// so it can hand the matching sender to Tauri's managed state before the
/// scheduler is wired up — otherwise the about page may invoke commands
/// before `app.manage()` has been called.
///
/// The scheduler always boots in `Mode::Playing`; the operator's
/// persisted mode is ignored at startup (a stage that was left in `Paused`
/// recovers cleanly on next launch).
pub fn spawn_with_rx(
    rx: mpsc::Receiver<Cmd>,
    plan: Plan,
    logic: Option<Arc<Mutex<Option<LogicHandle>>>>,
    events: Arc<dyn Events>,
) {
    tokio::spawn(run(rx, plan, logic, events));
}

/// Internal state held by the scheduler task.
struct Runtime {
    plan: Plan,
    /// Index into `plan.rotation`. Always within bounds when rotation
    /// non-empty; ignored when empty.
    idx: usize,
    mode: Mode,
    /// If `Some`, we're currently displaying a one-shot URL until this
    /// instant; afterwards we return to rotation.
    one_shot_until: Option<Instant>,
    /// URL last announced via `on_url_change`. Used so we don't spam the
    /// event sink when nothing actually changed.
    last_url: Option<String>,
    /// Cron occurrences up to this wall-clock time have been handled.
    cron_checked: DateTime<Local>,
}

impl Runtime {
    fn new(plan: Plan) -> Self {
        Self {
            plan,
            idx: 0,
            mode: Mode::Playing,
            one_shot_until: None,
            last_url: None,
            cron_checked: Local::now(),
        }
    }
}

async fn run(
    mut rx: mpsc::Receiver<Cmd>,
    plan: Plan,
    logic: Option<Arc<Mutex<Option<LogicHandle>>>>,
    events: Arc<dyn Events>,
) {
    let mut rt = Runtime::new(plan);

    // The current rotation "slot" started here. Used to compute remaining
    // time when an interrupt (NavTimed, cron) lands mid-slot.
    let mut slot_started = Instant::now();
    let mut slot_duration = current_slot_duration(&rt);

    announce_current(&rt, &events);

    loop {
        // Compute the next wake instant: min(slot end, one_shot end, cron tick).
        let now = Instant::now();
        let slot_deadline = slot_started + slot_duration;
        let one_shot_deadline = rt.one_shot_until;
        let cron_deadline = next_cron_fire(&rt.plan.cron, rt.cron_checked).map(|dt| {
            let ms = (dt - Local::now()).num_milliseconds().max(0) as u64;
            now + Duration::from_millis(ms)
        });

        let tokio_deadline = [Some(slot_deadline), one_shot_deadline, cron_deadline]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or_else(|| now + Duration::from_secs(60));

        tokio::select! {
            Some(cmd) = rx.recv() => {
                match cmd {
                    Cmd::Rebuild(new_plan) => {
                        // Drain any one-shot actions the script declared at
                        // top level (e.g. an immediate `nav` in run(cfg)).
                        let one_shots = new_plan.one_shot.clone();
                        rt.plan = new_plan;
                        rt.idx = 0;
                        rt.one_shot_until = None;
                        rt.cron_checked = Local::now();
                        slot_started = Instant::now();
                        slot_duration = current_slot_duration(&rt);
                        announce_current(&rt, &events);
                        for action in one_shots {
                            apply_nav_action(&mut rt, action, &events, &mut slot_started, &mut slot_duration);
                        }
                    }
                    Cmd::TimedNav { url, secs } => {
                        rt.one_shot_until = Some(Instant::now() + Duration::from_secs(secs.max(1)));
                        // Timed nav implies Playing — when the timer expires
                        // rotation resumes; staying Paused would be a UX trap.
                        set_mode(&mut rt, Mode::Playing, &events, &mut slot_started, &mut slot_duration);
                        emit_url(&mut rt, &url, &events);
                    }
                    Cmd::Nav(url) => {
                        rt.one_shot_until = None;
                        set_mode(&mut rt, Mode::Playing, &events, &mut slot_started, &mut slot_duration);
                        emit_url(&mut rt, &url, &events);
                    }
                    Cmd::Pause => {
                        set_mode(&mut rt, Mode::Paused, &events, &mut slot_started, &mut slot_duration);
                    }
                    Cmd::Play => {
                        // Play also ends a timed nav early (e.g. a coffee
                        // break), so rotation advances again right away.
                        rt.one_shot_until = None;
                        slot_started = Instant::now();
                        slot_duration = current_slot_duration(&rt);
                        set_mode(&mut rt, Mode::Playing, &events, &mut slot_started, &mut slot_duration);
                        // Re-announce the current rotation URL so the kiosk
                        // navigates away from whatever it was held on.
                        announce_current(&rt, &events);
                    }
                    Cmd::Step { delta, reply } => {
                        let new_url = if rt.plan.rotation.is_empty() {
                            None
                        } else {
                            // rem_euclid keeps the result non-negative even
                            // for negative `delta`, so -1 wraps to the last
                            // entry instead of underflowing.
                            let n = rt.plan.rotation.len() as i32;
                            rt.idx = (rt.idx as i32 + delta).rem_euclid(n) as usize;
                            rt.one_shot_until = None;
                            slot_started = Instant::now();
                            slot_duration = current_slot_duration(&rt);
                            let u = rt.plan.rotation[rt.idx].url.clone();
                            emit_url(&mut rt, &u, &events);
                            Some(u)
                        };
                        let _ = reply.send(new_url);
                    }
                    Cmd::Shutdown => break,
                }
            }
            _ = sleep_until(tokio_deadline) => {
                // Decide which deadline expired.
                let now = Instant::now();
                if let Some(end) = rt.one_shot_until {
                    if now >= end {
                        rt.one_shot_until = None;
                        // Resume rotation from current slot — reannounce so
                        // listeners see the rotation URL again.
                        slot_started = Instant::now();
                        slot_duration = current_slot_duration(&rt);
                        announce_current(&rt, &events);
                        continue;
                    }
                }
                // Cron tick? Fire every job with an occurrence since the last
                // check, then feed its nav/pause/play side-effects in.
                let wall_now = Local::now();
                let due = due_jobs(&rt.plan.cron, rt.cron_checked, wall_now);
                rt.cron_checked = wall_now;
                if !due.is_empty() {
                    let actions = fire_cron(&rt.plan.cron, &due, &logic);
                    slot_started = Instant::now();
                    slot_duration = current_slot_duration(&rt);
                    for action in actions {
                        apply_nav_action(&mut rt, action, &events, &mut slot_started, &mut slot_duration);
                    }
                    continue;
                }
                // Otherwise: advance rotation (unless not playing / one-shot / empty).
                if rt.mode == Mode::Playing
                    && rt.one_shot_until.is_none()
                    && !rt.plan.rotation.is_empty()
                {
                    rt.idx = (rt.idx + 1) % rt.plan.rotation.len();
                    slot_started = Instant::now();
                    slot_duration = current_slot_duration(&rt);
                    announce_current(&rt, &events);
                } else {
                    // Reset slot timer so we don't tight-loop.
                    slot_started = Instant::now();
                    slot_duration = current_slot_duration(&rt);
                }
            }
            else => break,
        }
    }
}

/// Transition the runtime's mode and emit the event iff it actually changed.
/// Resets slot timing on transitions into `Playing` so we don't immediately
/// fall through to the next entry.
fn set_mode(
    rt: &mut Runtime,
    new: Mode,
    events: &Arc<dyn Events>,
    slot_started: &mut Instant,
    slot_duration: &mut Duration,
) {
    if rt.mode == new {
        return;
    }
    rt.mode = new;
    if new == Mode::Playing {
        *slot_started = Instant::now();
        *slot_duration = current_slot_duration(rt);
    }
    events.on_mode_change(new);
}

fn apply_nav_action(
    rt: &mut Runtime,
    action: crate::logic::NavAction,
    events: &Arc<dyn Events>,
    slot_started: &mut Instant,
    slot_duration: &mut Duration,
) {
    use crate::logic::NavAction;
    match action {
        NavAction::Nav { url } => {
            rt.one_shot_until = None;
            emit_url(rt, &url, events);
        }
        NavAction::NavTimed { url, secs } => {
            rt.one_shot_until = Some(Instant::now() + Duration::from_secs(secs.max(1)));
            emit_url(rt, &url, events);
        }
        NavAction::Pause => set_mode(rt, Mode::Paused, events, slot_started, slot_duration),
        NavAction::Play => set_mode(rt, Mode::Playing, events, slot_started, slot_duration),
    }
}

fn current_slot_duration(rt: &Runtime) -> Duration {
    if rt.plan.rotation.is_empty() {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(rt.plan.rotation[rt.idx].secs.max(1))
    }
}

fn current_url(rt: &Runtime) -> Option<&str> {
    if rt.plan.rotation.is_empty() {
        None
    } else {
        Some(&rt.plan.rotation[rt.idx].url)
    }
}

fn announce_current(rt: &Runtime, events: &Arc<dyn Events>) {
    if let Some(url) = current_url(rt) {
        events.on_url_change(url);
    }
}

fn emit_url(rt: &mut Runtime, url: &str, events: &Arc<dyn Events>) {
    if rt.last_url.as_deref() != Some(url) {
        rt.last_url = Some(url.to_string());
    }
    events.on_url_change(url);
}

/// Earliest cron occurrence strictly after `after`, across all jobs.
fn next_cron_fire(jobs: &[CronJob], after: DateTime<Local>) -> Option<DateTime<Local>> {
    jobs.iter()
        .filter_map(|job| parse_cron(&job.cron).ok()?.after(&after).next())
        .min()
}

/// Indices of jobs with an occurrence in `(after, until]`.
fn due_jobs(jobs: &[CronJob], after: DateTime<Local>, until: DateTime<Local>) -> Vec<usize> {
    jobs.iter()
        .enumerate()
        .filter(|(_, job)| {
            parse_cron(&job.cron)
                .ok()
                .and_then(|s| s.after(&after).next())
                .is_some_and(|next| next <= until)
        })
        .map(|(i, _)| i)
        .collect()
}

/// Run the due jobs' callbacks and collect their navigation side-effects.
fn fire_cron(
    jobs: &[CronJob],
    due: &[usize],
    logic: &Option<Arc<Mutex<Option<LogicHandle>>>>,
) -> Vec<crate::logic::NavAction> {
    let Some(handle_arc) = logic else {
        return Vec::new();
    };
    let guard = handle_arc.lock().expect("logic mutex poisoned");
    let Some(handle) = guard.as_ref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for &i in due {
        match handle.fire(&jobs[i].callback) {
            Ok(actions) => out.extend(actions),
            Err(e) => eprintln!("catstage: cron job {:?} failed: {e:#}", jobs[i].cron),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::RotationItem;
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct Collector {
        urls: StdMutex<Vec<String>>,
        modes: StdMutex<Vec<Mode>>,
    }
    impl Events for Collector {
        fn on_url_change(&self, url: &str) {
            self.urls.lock().unwrap().push(url.to_string());
        }
        fn on_mode_change(&self, mode: Mode) {
            self.modes.lock().unwrap().push(mode);
        }
    }

    fn plan_two_urls() -> Plan {
        Plan {
            default_secs: 1,
            rotation: vec![
                RotationItem {
                    url: "https://a/".into(),
                    secs: 1,
                },
                RotationItem {
                    url: "https://b/".into(),
                    secs: 1,
                },
            ],
            cron: vec![],
            one_shot: vec![],
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn rotation_advances_round_robin() {
        let coll = Arc::new(Collector::default());
        let tx = spawn(plan_two_urls(), None, coll.clone());
        // First URL is announced immediately on spawn.
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        // Give the task a moment to drain.
        tokio::task::yield_now().await;
        let _ = tx.send(Cmd::Shutdown).await;
        // We expect at least the first URL plus one advance.
        let urls = coll.urls.lock().unwrap().clone();
        assert!(urls.first().map(|s| s.as_str()) == Some("https://a/"));
        assert!(
            urls.iter().any(|u| u == "https://b/"),
            "rotation never advanced past index 0: {urls:?}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn timed_nav_overrides_then_resumes() {
        let coll = Arc::new(Collector::default());
        let tx = spawn(plan_two_urls(), None, coll.clone());
        tokio::time::advance(Duration::from_millis(50)).await;
        tx.send(Cmd::TimedNav {
            url: "https://override/".into(),
            secs: 2,
        })
        .await
        .unwrap();
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        let urls = coll.urls.lock().unwrap().clone();
        // Sequence: initial "https://a/", then "https://override/",
        // then back to a rotation URL (either a or b).
        assert!(urls.contains(&"https://override/".to_string()));
        let last = urls.last().unwrap();
        assert!(last == "https://a/" || last == "https://b/", "{urls:?}");
        let _ = tx.send(Cmd::Shutdown).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn rebuild_swaps_plan_mid_rotation() {
        let coll = Arc::new(Collector::default());
        let tx = spawn(plan_two_urls(), None, coll.clone());
        tokio::time::advance(Duration::from_millis(50)).await;
        let new_plan = Plan {
            default_secs: 1,
            rotation: vec![RotationItem {
                url: "https://c/".into(),
                secs: 1,
            }],
            cron: vec![],
            one_shot: vec![],
        };
        tx.send(Cmd::Rebuild(new_plan)).await.unwrap();
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        let urls = coll.urls.lock().unwrap().clone();
        assert!(
            urls.contains(&"https://c/".to_string()),
            "rebuild didn't take effect: {urls:?}"
        );
        let _ = tx.send(Cmd::Shutdown).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn pause_play_emit_mode_events() {
        let coll = Arc::new(Collector::default());
        let tx = spawn(plan_two_urls(), None, coll.clone());
        tokio::time::advance(Duration::from_millis(50)).await;
        tx.send(Cmd::Pause).await.unwrap();
        tokio::time::advance(Duration::from_millis(50)).await;
        tx.send(Cmd::Play).await.unwrap();
        tokio::time::advance(Duration::from_millis(50)).await;
        tokio::task::yield_now().await;
        let modes = coll.modes.lock().unwrap().clone();
        assert_eq!(modes, vec![Mode::Paused, Mode::Playing]);
        let _ = tx.send(Cmd::Shutdown).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn paused_then_play_re_announces_url() {
        let coll = Arc::new(Collector::default());
        let tx = spawn(plan_two_urls(), None, coll.clone());
        tokio::time::advance(Duration::from_millis(50)).await;
        tx.send(Cmd::Pause).await.unwrap();
        tokio::time::advance(Duration::from_millis(50)).await;
        tx.send(Cmd::Play).await.unwrap();
        tokio::time::advance(Duration::from_millis(50)).await;
        tokio::task::yield_now().await;
        let modes = coll.modes.lock().unwrap().clone();
        assert_eq!(modes, vec![Mode::Paused, Mode::Playing]);
        // Play after Paused should re-emit the current rotation URL so the
        // kiosk navigates back from wherever it was held.
        let urls = coll.urls.lock().unwrap().clone();
        let after_play_count = urls.iter().filter(|u| u.as_str() == "https://a/").count();
        assert!(
            after_play_count >= 2,
            "expected re-announce on Play: {urls:?}"
        );
        let _ = tx.send(Cmd::Shutdown).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn play_ends_timed_nav_early() {
        let coll = Arc::new(Collector::default());
        let tx = spawn(plan_two_urls(), None, coll.clone());
        tokio::time::advance(Duration::from_millis(50)).await;
        tx.send(Cmd::TimedNav {
            url: "https://coffee/".into(),
            secs: 1800,
        })
        .await
        .unwrap();
        tokio::time::advance(Duration::from_millis(100)).await;
        tx.send(Cmd::Play).await.unwrap();
        tokio::time::advance(Duration::from_millis(100)).await;
        // Rotation must advance again within a couple of 1s slots, long
        // before the 30-minute timer would have run out.
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        let urls = coll.urls.lock().unwrap().clone();
        let after_coffee: Vec<_> = urls
            .iter()
            .skip_while(|u| *u != "https://coffee/")
            .skip(1)
            .collect();
        assert!(
            after_coffee.len() >= 2,
            "rotation stayed frozen after Play: {urls:?}"
        );
        let _ = tx.send(Cmd::Shutdown).await;
    }

    fn job(cron: &str) -> CronJob {
        CronJob {
            cron: cron.into(),
            callback: crate::logic::CronCallback::Named("f".into()),
        }
    }

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32, ms: u32) -> DateTime<Local> {
        use chrono::TimeZone;
        Local.with_ymd_and_hms(y, m, d, h, min, s).unwrap()
            + chrono::Duration::milliseconds(ms.into())
    }

    #[test]
    fn due_jobs_matches_local_weekday_window() {
        let jobs = [job("0 11 * * MON-FRI")];
        // 2026-09-21 is a Monday, 2026-09-26 a Saturday.
        let mon = |h, min, s, ms| local(2026, 9, 21, h, min, s, ms);
        let sat = |h, min, s, ms| local(2026, 9, 26, h, min, s, ms);
        assert_eq!(
            due_jobs(&jobs, mon(10, 59, 59, 0), mon(11, 0, 0, 500)),
            vec![0]
        );
        assert!(due_jobs(&jobs, sat(10, 59, 59, 0), sat(11, 0, 0, 500)).is_empty());
        assert!(due_jobs(&jobs, mon(11, 0, 1, 0), mon(11, 5, 0, 0)).is_empty());
        assert_eq!(
            next_cron_fire(&jobs, sat(12, 0, 0, 0)),
            Some(local(2026, 9, 28, 11, 0, 0, 0))
        );
    }
}

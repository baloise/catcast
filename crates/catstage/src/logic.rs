//! Rhai logic engine for catstage.
//!
//! The Rhai script's job is to take a parsed [`Config`] (as a Rhai Map) and
//! call back into host-registered functions to declare the desired schedule:
//!
//! - `set_rotation(urls_with_secs, default_secs)` — populates the rotation
//!   ring. `urls_with_secs` is an array of `#{ url, secs }` maps. The default
//!   `set_rotation` call lives in `default-logic/default.rhai` which the CLI
//!   pushes; the stage binary doesn't bake it in.
//! - `nav(url)` / `nav(url, secs)` — immediate navigation, optionally with
//!   auto-resume after `secs`.
//! - `pause()` / `play()` — toggle the `paused` flag.
//! - `schedule(cron_expr, fn_name)` / `schedule(cron_expr, closure)` — run
//!   the named Rhai function (or the closure) on every cron tick. Cron is
//!   evaluated in the stage machine's local time zone.
//!
//! The host functions write into a [`Plan`] that the scheduler consumes. The
//! engine is sticky: once `eval` finishes, the registered closures keep
//! pointing at a *fresh* `Plan` so the script's top-level call to `run(cfg)`
//! fully populates it before `eval` returns.

use anyhow::{Context, Result};
use catcast_core::Config;
use rhai::{Array, Dynamic, Engine, FnPtr, Map, Scope, AST};
use std::sync::{Arc, Mutex};

/// What a logic eval produces: a rotation ring + a list of cron-driven calls.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub rotation: Vec<RotationItem>,
    /// Fallback rotation duration if a [`RotationItem`] has none. The host
    /// expects every item to carry its own resolved duration, but this is
    /// still useful when manual `nav` is followed by "resume" logic.
    pub default_secs: u64,
    pub cron: Vec<CronJob>,
    /// One-shot `nav` calls the script made (immediate URL or timed nav).
    pub one_shot: Vec<NavAction>,
}

#[derive(Debug, Clone)]
pub struct RotationItem {
    pub url: String,
    pub secs: u64,
}

#[derive(Debug, Clone)]
pub struct CronJob {
    pub cron: String,
    pub callback: CronCallback,
}

/// What a cron tick invokes.
#[derive(Debug, Clone)]
pub enum CronCallback {
    /// A top-level script function, looked up by name.
    Named(String),
    /// A function pointer / closure. Captured variables travel as curried
    /// arguments, so `|| nav(url, secs)` keeps its `url` and `secs`.
    Closure(FnPtr),
}

#[derive(Debug, Clone)]
pub enum NavAction {
    /// Set the current URL and stay there (no resume).
    Nav {
        url: String,
    },
    /// Set the current URL for `secs` seconds, then auto-resume.
    NavTimed {
        url: String,
        secs: u64,
    },
    Pause,
    Play,
}

/// A handle the scheduler holds onto so it can fire cron-scheduled Rhai
/// callbacks later. Created by [`evaluate`]. We keep the AST instead of the
/// engine — Rhai's `Engine` isn't `Clone`, and building a fresh one per cron
/// tick is microsecond-cheap.
pub struct LogicHandle {
    ast: AST,
}

impl LogicHandle {
    /// Invoke a `schedule()`-registered callback and return the navigation
    /// side-effects (nav, pause, play) it produced, in call order.
    pub fn fire(&self, callback: &CronCallback) -> Result<Vec<NavAction>> {
        // Fresh host fns bound to a fresh collector, so only this tick's
        // side-effects come back.
        let collector = Arc::new(Mutex::new(Plan::default()));
        let mut engine = make_engine();
        register_host_fns(&mut engine, Arc::clone(&collector));
        match callback {
            CronCallback::Named(name) => engine
                .call_fn::<Dynamic>(&mut Scope::new(), &self.ast, name, ())
                .map(|_| ())
                .with_context(|| format!("calling Rhai fn {name}"))?,
            CronCallback::Closure(fnptr) => fnptr
                .call::<Dynamic>(&engine, &self.ast, ())
                .map(|_| ())
                .with_context(|| format!("calling Rhai closure {}", fnptr.fn_name()))?,
        }
        let actions = std::mem::take(&mut collector.lock().expect("plan mutex poisoned").one_shot);
        Ok(actions)
    }
}

/// Construct a Rhai [`Engine`] with our depth limits applied. Rhai's stock
/// defaults are 32/16 in debug builds (64/32 release), which the shipped
/// `default-logic/default.rhai` brushes past on a `#{ url: entry["url"], … }`
/// inside a `for` body — the resulting "Expression exceeds maximum
/// complexity" was a non-obvious foot-gun for anyone authoring logic. The
/// bumped 128/64 ceiling is still well under runaway recursion territory but
/// well above realistic dashboard-config scripts. Centralised so [`evaluate`]
/// and [`LogicHandle::call`] (which both spin up an Engine) share the same
/// settings.
fn make_engine() -> Engine {
    let mut e = Engine::new();
    e.set_max_expr_depths(128, 64);
    e
}

/// Syntax-check a Rhai source without running it. Used by the SetLogic path
/// to surface parse / depth-limit errors to the operator *before* we persist
/// a broken script and flip `has_logic = true`. Cheaper than [`evaluate`]
/// because it skips the `run(cfg)` call.
pub fn compile_check(rhai_src: &str) -> Result<()> {
    make_engine()
        .compile(rhai_src)
        .context("compiling Rhai logic source")?;
    Ok(())
}

/// Parse `rhai_src`, register host fns bound to a fresh [`Plan`], and run the
/// script's top-level `run(cfg)` entry point with the current `Config` as a
/// Rhai map. Returns both the populated plan and a handle for later cron
/// callbacks.
pub fn evaluate(rhai_src: &str, cfg: &Config) -> Result<(Plan, LogicHandle)> {
    let plan = Arc::new(Mutex::new(Plan {
        default_secs: 30,
        ..Plan::default()
    }));
    let mut engine = make_engine();
    register_host_fns(&mut engine, Arc::clone(&plan));

    let ast = engine
        .compile(rhai_src)
        .context("compiling Rhai logic source")?;

    // Pass the config to the script as a Rhai map. We go via JSON which
    // gives us a structure Rhai's map accessors are happy with (`cfg["..."]`).
    let cfg_json = serde_json::to_string(cfg).context("serialising config for Rhai")?;
    let cfg_map: Map = engine
        .parse_json(&cfg_json, true)
        .context("parsing config JSON as Rhai map")?;

    let mut scope = Scope::new();
    engine
        .call_fn::<()>(&mut scope, &ast, "run", (cfg_map,))
        .context("calling Rhai run(cfg)")?;

    let final_plan = plan.lock().expect("plan mutex poisoned").clone();
    Ok((final_plan, LogicHandle { ast }))
}

fn register_host_fns(engine: &mut Engine, collector: Arc<Mutex<Plan>>) {
    // set_rotation(urls: Array, default_secs: i64)
    let c = Arc::clone(&collector);
    engine.register_fn("set_rotation", move |urls: Array, default_secs: i64| {
        let mut plan = c.lock().expect("plan mutex poisoned");
        plan.default_secs = default_secs.max(1) as u64;
        plan.rotation.clear();
        for item in urls {
            // Each item should be a map {url, secs} — silently skip malformed.
            if let Some(m) = item.try_cast::<Map>() {
                let url = m
                    .get("url")
                    .and_then(|v| v.clone().try_cast::<String>())
                    .unwrap_or_default();
                let secs = m
                    .get("secs")
                    .and_then(|v| v.as_int().ok())
                    .unwrap_or(default_secs)
                    .max(1) as u64;
                if !url.is_empty() {
                    plan.rotation.push(RotationItem { url, secs });
                }
            }
        }
    });

    let c = Arc::clone(&collector);
    engine.register_fn("nav", move |url: &str| {
        c.lock()
            .expect("plan mutex poisoned")
            .one_shot
            .push(NavAction::Nav {
                url: url.to_string(),
            });
    });
    let c = Arc::clone(&collector);
    engine.register_fn("nav", move |url: &str, secs: i64| {
        c.lock()
            .expect("plan mutex poisoned")
            .one_shot
            .push(NavAction::NavTimed {
                url: url.to_string(),
                secs: secs.max(1) as u64,
            });
    });

    let c = Arc::clone(&collector);
    engine.register_fn("pause", move || {
        c.lock()
            .expect("plan mutex poisoned")
            .one_shot
            .push(NavAction::Pause);
    });
    let c = Arc::clone(&collector);
    engine.register_fn("play", move || {
        c.lock()
            .expect("plan mutex poisoned")
            .one_shot
            .push(NavAction::Play);
    });

    // schedule(cron_expr, fn_name): the scheduler invokes the named script
    // function on each cron tick.
    let c = Arc::clone(&collector);
    engine.register_fn("schedule", move |cron: &str, fn_name: &str| {
        c.lock().expect("plan mutex poisoned").cron.push(CronJob {
            cron: cron.to_string(),
            callback: CronCallback::Named(fn_name.to_string()),
        });
    });
    // schedule(cron_expr, closure): `default.rhai` passes `|| nav(url, secs)`.
    // Store the function pointer only — it runs on the cron tick, never at
    // eval time (otherwise every `fixed` entry would show on config load).
    let c = Arc::clone(&collector);
    engine.register_fn("schedule", move |cron: &str, fnptr: FnPtr| {
        c.lock().expect("plan mutex poisoned").cron.push(CronJob {
            cron: cron.to_string(),
            callback: CronCallback::Closure(fnptr),
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_basic() -> Config {
        Config::from_yaml(
            r#"
version: 1
rotation:
  default_duration: 30s
  urls:
    - https://a/
    - url: https://b/
      duration: 60s
"#,
        )
        .unwrap()
    }

    #[test]
    fn evaluates_default_logic_skeleton() {
        let logic = r#"
            fn run(cfg) {
                let urls = [
                    #{ url: "https://a/", secs: 30 },
                    #{ url: "https://b/", secs: 60 },
                ];
                set_rotation(urls, 30);
            }
        "#;
        let (plan, _h) = evaluate(logic, &cfg_basic()).unwrap();
        assert_eq!(plan.rotation.len(), 2);
        assert_eq!(plan.rotation[0].url, "https://a/");
        assert_eq!(plan.rotation[0].secs, 30);
        assert_eq!(plan.rotation[1].secs, 60);
    }

    #[test]
    fn nav_one_shot_recorded() {
        let logic = r#"
            fn run(cfg) {
                nav("https://x/", 10);
            }
        "#;
        let (plan, _h) = evaluate(logic, &cfg_basic()).unwrap();
        assert_eq!(plan.one_shot.len(), 1);
        match &plan.one_shot[0] {
            NavAction::NavTimed { url, secs } => {
                assert_eq!(url, "https://x/");
                assert_eq!(*secs, 10);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn default_logic_fixed_entry_fires_only_on_tick() {
        let cfg = Config::from_yaml(
            r#"
version: 1
rotation:
  default_duration: 30s
  urls:
    - https://a/
fixed:
  - cron: "0 11 * * MON-FRI"
    url: https://menu/
    duration: 60m
"#,
        )
        .unwrap();
        let src = include_str!("../../../default-logic/default.rhai");
        let (plan, handle) = evaluate(src, &cfg).unwrap();
        // Nothing may navigate at load time...
        assert!(plan.one_shot.is_empty(), "{:?}", plan.one_shot);
        assert_eq!(plan.cron.len(), 1);
        assert_eq!(plan.cron[0].cron, "0 11 * * MON-FRI");
        // ...only when the tick fires, with the captured url + secs.
        let actions = handle.fire(&plan.cron[0].callback).unwrap();
        match actions.as_slice() {
            [NavAction::NavTimed { url, secs }] => {
                assert_eq!(url, "https://menu/");
                assert_eq!(*secs, 3600);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn named_schedule_callback_fires() {
        let logic = r#"
            fn menu() { nav("https://menu/"); }
            fn run(cfg) { schedule("0 11 * * *", "menu"); }
        "#;
        let (plan, handle) = evaluate(logic, &cfg_basic()).unwrap();
        assert!(plan.one_shot.is_empty());
        let actions = handle.fire(&plan.cron[0].callback).unwrap();
        assert!(
            matches!(actions.as_slice(), [NavAction::Nav { url }] if url == "https://menu/"),
            "{actions:?}"
        );
    }
}

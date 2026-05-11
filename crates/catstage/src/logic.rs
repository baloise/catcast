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
//! - `schedule(cron_expr, fn_name)` — schedule the named Rhai function (no
//!   args) to run on every cron tick.
//!
//! The host functions write into a [`Plan`] that the scheduler consumes. The
//! engine is sticky: once `eval` finishes, the registered closures keep
//! pointing at a *fresh* `Plan` so the script's top-level call to `run(cfg)`
//! fully populates it before `eval` returns.

use anyhow::{Context, Result};
use catcast_core::Config;
use rhai::{Array, Dynamic, Engine, Map, Scope, AST};
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
    pub fn_name: String,
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
/// callbacks later. Created by [`evaluate`]. We keep the source instead of
/// the engine — Rhai's `Engine` isn't `Clone`, and building a fresh one per
/// cron tick is microsecond-cheap.
pub struct LogicHandle {
    src: String,
    ast: AST,
}

impl LogicHandle {
    /// Invoke a previously-`schedule()`-registered Rhai function. The plan
    /// captures `fn_name`s the script declared; cron ticks call them by name.
    /// Side-effects (nav, pause, play) populate the supplied collector.
    pub fn call(&self, fn_name: &str, collector: Arc<Mutex<Plan>>) -> Result<()> {
        // Re-register host fns bound to the supplied collector so the script's
        // body can call `nav`, `pause`, `play` etc. and the scheduler sees them.
        let mut engine = Engine::new();
        register_host_fns(&mut engine, collector);
        // Recompile so the AST is bound to the fresh engine.
        let ast = engine
            .compile(&self.src)
            .with_context(|| "recompiling Rhai source for cron call")?;
        let mut scope = Scope::new();
        engine
            .call_fn::<()>(&mut scope, &ast, fn_name, ())
            .with_context(|| format!("calling Rhai fn {fn_name}"))?;
        // ast field exists to keep the original handle small; reused only for
        // the type's lifetime guarantees, not directly here.
        let _ = &self.ast;
        Ok(())
    }
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
    let mut engine = Engine::new();
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
    Ok((
        final_plan,
        LogicHandle {
            src: rhai_src.to_string(),
            ast,
        },
    ))
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

    // schedule(cron_expr, fn_name): we don't try to capture Rhai closures —
    // the script names the callback and the scheduler invokes it by name on
    // each cron tick. `default.rhai` already passes a closure for simplicity;
    // we support both by detecting a string vs. callable. For closures we
    // synthesise a stable name and store the closure in a side table — but
    // that's more machinery than we need; we instead document that the
    // default-logic should use string fn names.
    let c = Arc::clone(&collector);
    engine.register_fn("schedule", move |cron: &str, fn_name: &str| {
        c.lock().expect("plan mutex poisoned").cron.push(CronJob {
            cron: cron.to_string(),
            fn_name: fn_name.to_string(),
        });
    });
    // schedule(cron_expr, anonymous_fn): accept a Rhai function-pointer
    // (`FnPtr`) so existing default-logic that passes `|| nav(url, secs)`
    // keeps working. We name the slot synthetically so a later cron tick can
    // re-eval the closure via `LogicHandle::call`. Since FnPtr doesn't carry
    // captured environment outside the eval, we materialise the side-effects
    // *right now* by calling it once with no args — adequate for the v1
    // default-logic which immediately calls `nav(...)`.
    let c = Arc::clone(&collector);
    engine.register_fn(
        "schedule",
        move |context: rhai::NativeCallContext, cron: &str, fnptr: rhai::FnPtr| {
            // Snapshot whatever side-effects the closure produces.
            let _ = fnptr.call_within_context::<Dynamic>(&context, ());
            // Still record the cron entry so the scheduler will *also* fire
            // it on the next match. We use the fn-pointer's fn_name as the
            // dispatch key; for anonymous closures Rhai picks a stable name.
            c.lock().expect("plan mutex poisoned").cron.push(CronJob {
                cron: cron.to_string(),
                fn_name: fnptr.fn_name().to_string(),
            });
        },
    );
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
}

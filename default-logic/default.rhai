// CatCast default logic.
//
// Shipped as a separate file so it can be hot-replaced with `catc logic
// import <FILE>` without rebuilding the stage. Keep the surface small:
// `set_rotation`, `nav`, `pause`, `play`, `schedule`, `on_event`. The
// host (catstage) registers these as Rust-backed functions; this file
// just wires them up from the parsed config.
//
// Conventions:
//   - `cfg` is a Rhai map matching the YAML schema in catcast-core.
//   - All durations passed to host fns are seconds (u64).

fn run(cfg) {
    let rotation = cfg["rotation"];
    let default_secs = parse_duration_secs(rotation["default_duration"]);

    // Round-robin rotation. Each entry is either a string URL (short form)
    // or `{ url, duration }` (long form).
    let urls = [];
    for entry in rotation["urls"] {
        if type_of(entry) == "string" {
            urls.push(#{ url: entry, secs: default_secs });
        } else {
            urls.push(#{
                url: entry["url"],
                secs: parse_duration_secs(entry["duration"]),
            });
        }
    }
    set_rotation(urls, default_secs);

    // Fixed-time overrides via cron + duration.
    if cfg.contains("fixed") {
        for f in cfg["fixed"] {
            let secs = parse_duration_secs(f["duration"]);
            let url = f["url"];
            schedule(f["cron"], || nav(url, secs));
        }
    }
}

// Helper kept here (rather than in the host) so contributors can tweak it
// without touching Rust. Accepts strings like "30s", "5m", "1h".
fn parse_duration_secs(s) {
    let n = "";
    let unit = "";
    for ch in s {
        if ch >= '0' && ch <= '9' { n += ch; } else { unit += ch; }
    }
    let v = parse_int(n);
    switch unit.trim() {
        "s" => v,
        "m" => v * 60,
        "h" => v * 3600,
        "" => v,
        _ => v,
    }
}

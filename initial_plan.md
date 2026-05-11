# CatCast — Implementation Plan

## Context

CatCast is a tiny, opinionated digital-signage stack for corporate environments
where you can't install software as admin. Three components, monorepo, public on
GitHub under **0BSD**, in the spirit of CopyHeart.

- **catstage** — Tauri (Rust + WebView2) fullscreen viewer.
- **catsocks** — dumb WebSocket relay on Cloudflare Workers (Rust via the
  `worker` crate, compiled to WASM, Durable Object with Hibernatable
  WebSockets).
- **catc** — CLI (Rust, single EXE) that drives stages through the relay.

**Scale assumed:** 1–2 screens, a handful of updates per month. Lost messages OK,
no SLA. Threat model: keep the cloud broker out; trust anyone already inside
the corp network.

**Decisions locked in this session:**
- **Threat model.** Adversaries we defend against:
    - The **public cloud broker** (Cloudflare itself, its logs, anyone with
      the URL). Honest-but-curious, must only see opaque ciphertext.
    - **Anyone on the open internet** who guesses or scrapes the broker URL.
  Inside the room, participants who already know a stage's name trust each
  other (matches the "inside-corp takeover is not a concern" stance).
- **PSK = stage name** (default: hostname; override with `--name`). The PSK
  is never put on the wire — see "Wire protocol" below: the envelope is just
  `{ v, nonce, ct }`. The stage name lives inside the ciphertext as the
  routing field. So the broker, and any random URL-scraper, see only opaque
  blobs. To learn a stage's name an attacker needs **physical access to the
  stage screen** (the `catcast://about` page displays the name) — at which
  point they own the device by other means anyway.
- **Operator setup flow** (matches the way the CLI already needs a config
  file for the broker URL):
    1. Deploy CatSocks. Get a URL: `wss://catsocks.<acct>.workers.dev/r/<room>`.
    2. On each stage machine: run `catstage --install-autostart --socks <URL>
       [--name <name>]` once. Stage boots, shows its name on `catcast://about`.
    3. On the laptop: `catc init --socks <URL>` writes `catc.toml`. Then
       `catc add-stage <name>` for each stage (operator reads the name off
       the screen). The CLI now caches an Argon2id-derived key per stage.
    4. `catc targets list --probe` confirms which configured stages reply.
- **Broker URL is not a security boundary** in this design. It can be a
  friendly path (`/r/baloise-floor-3`). Anyone who knows it sees only
  ciphertext.
- **Argon2id KDF** still earns its keep: hostnames are low-entropy and
  guessable; the slow KDF turns "guess hostnames offline against a captured
  ciphertext" into a real cost. Derive once at boot per known PSK, cache.
- **Per-message decryption cost.** Stages run AEAD verify on every envelope
  with their single PSK (one trial). The CLI runs AEAD verify with each
  PSK in its config until one succeeds (small N, microseconds each). For
  1–2 stages and a handful of commands per month this is invisible.
- Broker is **Rust on CF Workers** via the `worker` crate (not TypeScript),
  to honour the "Rust as much as possible" goal. Cloudflare's Rust SDK
  supports Durable Objects with Hibernatable WebSockets. Expect ~2× the
  lines of TS, but absolute amount is tiny (single file). Fits the free
  tier.
- Logic engine is Rhai (Rust-native, sandboxed, small).
- "Manual" mode (chrome+URL bar overlay) toggleable both by hotkey on the stage
  and by `catc manual on|off`. Everything doable on the stage must also be
  doable from the CLI.
- Platforms v1: **Windows x64 only**.
- "Install" = copy `catstage.exe` to a folder of choice, run it once with
  `--install-autostart --socks <wss://...> [--name <name>]` to drop a Startup
  folder shortcut. SmartScreen warning accepted on first run; auto-updates
  thereafter handle themselves (Tauri updater verifies its own Ed25519
  signature on the update bundle, independent of OS code-signing).
- Fixed-timing schedule uses standard cron via the `cron` crate.
- Fixed-timing URLs ship in v1.

> License: **0BSD** ("BSD Zero Clause"). Same "do whatever, no attribution
> required" spirit as WTFPL, but OSI-approved and accepted by virtually every
> corporate legal team — useful when adopters are also corporates.

## Repository layout

```
catcast/
├── Cargo.toml                       # workspace
├── LICENSE                          # 0BSD
├── README.md                        # CopyHeart-style: copy, fork, share, remix
├── CONTRIBUTING.md                  # no CLA, DCO optional, contribution-friendly tone
├── .github/
│   └── workflows/
│       ├── ci.yml                   # fmt + clippy + test (windows-latest)
│       ├── release.yml              # tag-driven build + GH Release + updater manifest
│       └── socks-deploy.yml         # wrangler deploy on socks/ changes
├── crates/
│   ├── catcast-core/                # serde types: Config, LogicSource, Command, State
│   ├── catcast-proto/               # crypto envelope + JSON commands + cron parsing
│   ├── catstage/                    # Tauri app -> catstage.exe
│   │   ├── tauri.conf.json
│   │   ├── src/                     # Rust: socks client, scheduler, Rhai host, IPC
│   │   └── ui/                      # tiny HTML+JS shell (rotator + manual overlay)
│   ├── catc/                        # CLI -> catc.exe
│   └── catsocks/                    # Cloudflare Worker (Rust, compiled to WASM)
│       ├── Cargo.toml               # crate-type = ["cdylib"]
│       ├── wrangler.toml            # DO binding + migrations
│       └── src/lib.rs               # Durable Object: hibernatable WS, broadcast
├── default-logic/
│   └── default.rhai                 # shipped as a separately-pushable file
└── examples/
    ├── config.yaml
    └── aliases.yaml
```

## Wire protocol (`crates/catcast-proto`)

Envelope (the only thing the broker sees):

```json
{ "v": 1, "nonce": "<base64-24>", "ct": "<base64-aead>" }
```

- **No name on the wire.** Addressing lives inside the ciphertext.
- AEAD: XChaCha20-Poly1305 (`chacha20poly1305` crate).
- Symmetric key per PSK, derived via Argon2id with a fixed, documented salt
  (stable across runs); computed once at boot and cached.
- Stages run AEAD verify with their single key. The CLI runs AEAD verify
  with each cached key in its config until one succeeds (small N).
- Wrong key → AEAD verify fails → silently dropped.

Cleartext payload after decrypt:
```json
{ "target": "<stage-name>", "ts": <unix-ms>, "msg": <Message> }
```

- `target` is the routing field. Stages discard messages whose `target`
  doesn't match their own name (after successful decrypt). The CLI uses
  `target` to know which configured stage a reply came from.
- `ts` is included inside the AEAD; replays past a small skew window are
  ignored.

`Message` variants — CLI → stage:
```
Pause
Play
Nav        { url }
NavTimed   { url, duration_secs }
Manual     { on: bool }
SetConfig  { yaml }
SetLogic   { rhai }
GetState                             # request a State snapshot in reply
```

`Message` — stage → CLI, one type used everywhere:
```
State { name, version, current_url, paused, manual,
        has_logic, has_config, since, ts }
```
- Sent unsolicited on connect (any listening CLI that has this stage
  configured will catch up).
- Sent in reply to `GetState`.
- Doubles as a liveness signal — no separate `Ping`/`Pong`.

### Target discovery (now strictly opt-in)

There is **no broker-level enumeration**. The CLI only ever interacts with
stages it has been told about explicitly:

- `catc add-stage <name>` registers a stage in the local CLI config (operator
  typically reads the name off the stage's `catcast://about` page).
- `catc targets list` prints the stages from `catc.toml`.
- `catc targets list --probe` sends a `GetState` to each configured stage
  and reports who replied within a few seconds.

This means a fresh CLI cannot discover stages by snooping the broker — it
has to be told. That's deliberate: the encryption only protects payloads;
gating the *list of names* keeps drive-by takeover attempts from working
even if someone scrapes the URL.

## CatStage runtime (`crates/catstage`)

- Tauri 2.x window: `decorations: false`, `fullscreen: true`, `always_on_top:
  true`, `skip_taskbar: true`. WebView2 hosts a tiny bundled `ui/index.html`.
- The HTML shell is a minimal frame: it shows either (a) the
  `catcast://about` info page (see below) when no config is loaded, (b) full
  navigation of the webview to the configured URL for content, or (c) a
  manual-mode overlay with chrome.
- **`catcast://about`** is registered as a custom Tauri URI scheme handler
  (`tauri.conf.json` → `protocol`). Navigating to it (either as the no-config
  splash, via `catc nav catcast://about`, or by typing it in the manual-mode URL
  bar) renders a Rust-served HTML page showing: stage name, version, broker
  URL, connection state, manual on/off, config + logic presence, current
  rotation entry, last-error. Easy to extend later (logs link, "force
  reconnect" button, etc.).
- Rust owns: socks client (`tokio-tungstenite`), scheduler (driven by a Rhai
  call back into Rust), Rhai engine, persistence, autostart installer,
  hotkey listener (`global-hotkey` crate; **Ctrl+Alt+M** toggles manual).
- `tauri::Manager::emit` for Rust → webview commands (navigate, show
  manual overlay, etc.); `#[tauri::command]` for webview → Rust (e.g. manual
  overlay's "exit" button).
- Cookies / localStorage / SSO: WebView2 persists per-user in a stable data
  folder under `%LOCALAPPDATA%\catcast\webview2`. We DO NOT clear it on update.
- Persistence dir: `%APPDATA%\catcast\` (resolved via `directories` crate).
  Files: `config.yaml`, `logic.rhai`, `state.json` (versioned with `schema: 1`).
- On boot: load disk state → connect to socks (retry forever with backoff) →
  meanwhile run rotation against last-known config. No connection ≠ blank screen.
- Autostart: `--install-autostart` creates `%APPDATA%\Microsoft\Windows\Start
  Menu\Programs\Startup\catstage.lnk` pointing at the EXE with persisted args.
  No admin required. Uses `mslnk` crate or PowerShell COM via `Command`.
- Auto-update: Tauri's built-in updater, manifest at
  `https://github.com/baloise/catcast/releases/latest/download/latest.json`,
  signed with project Ed25519 key (public key baked into stage at build).
- ⚠️ **Spike required before implementing the updater**: the corp proxy is
  known to block executable downloads. Plan in order:
    1. Try the default Tauri update payload (`.exe.zip` from GH Releases) from
       a stage on the corp network. If it passes through, we're done.
    2. If blocked, try serving the bundle from a CF Worker route that proxies
       `releases.githubusercontent.com` (different hostname, may bypass URL
       reputation rules).
    3. If still blocked, change the bundle format to a base64-encoded `.txt`
       (or `.bin`) and write a small Rust unwrap-and-replace stage in
       `--apply-update`. Tauri's updater is configurable enough to fetch
       arbitrary URLs and we hand off post-download to our own code.
    4. Last resort: ship updates as a CF-hosted SSE stream of base64 chunks.
  Don't write the updater wiring until the spike picks one of these. Until
  then the binary is build-and-drop with manual replacement.

## CatCLI (`crates/catc`) — binary name `catc`

### Targeting model

Each registered stage has an `active: bool` flag (default `true` on
`add-stage`). Command targeting:

- No `--name` and no `--all` → command goes to **every active stage**.
- `--name <N>` (repeatable) → only those stages, regardless of active flag.
- `--all` → every registered stage, regardless of active flag.

This lets the operator stage a session: `catc deactivate lobby` then
work freely on the rest of the floor without `--name` plumbing.

### Subcommands

```
catc init --socks <URL>                  # write catc.toml; idempotent
catc add-stage <name>                    # register a stage by its name
                                         #   (read off the stage's catcast://about)
catc remove-stage <name>
catc activate   <name>... | --all        # mark stages active
catc deactivate <name>... | --all        # mark stages inactive
catc pause [--name N]... [--all]
catc play  [--name N]... [--all]
catc nav <URL> [--for <DURATION>] [--name N]... [--all]
catc manual on|off [--name N]... [--all]
catc config  import <FILE> | export [--name N]... [--all]
catc logic   import <FILE> | export [--name N]... [--all]
catc state   import <FILE> | export [--name N]... [--all]
catc targets list [--probe]              # shows active flag and (with --probe) liveness
catc alias   add <name> <command...> | list | run <name> | import <FILE> | export <FILE>
```

Files:
- `%APPDATA%\catc\catc.toml` — broker URL and the list of registered stages,
  each entry: `{ name, active }`. The Argon2id-derived AEAD key per stage is
  cached at runtime; the TOML only holds the names and active flag.
- `%APPDATA%\catc\aliases.yaml` — `name: command-string`.

`alias run coffee` simply expands and re-invokes the command-line; aliases
inherit the active-set targeting.

### Export semantics with multiple targets

`config export` / `logic export` / `state export` require exactly one target
(else the CLI errors out — exporting "the" config from N stages is
ambiguous). The targeting flags above are about *sending*; export pulls
from a single source.

## CatSocks broker (`crates/catsocks/`)

- One Rust crate, `crate-type = ["cdylib"]`, built to WASM by `worker-build`
  and deployed via `wrangler deploy`.
- Entry point uses the `worker` crate's `#[event(fetch)]` to upgrade
  `/r/:room` to a WebSocket and forward to a Durable Object stub named after
  `:room`.
- Durable Object struct `Room` with **Hibernatable WebSockets** (the `worker`
  crate's `state.accept_web_socket(...)` plus the `web_socket_message`
  lifecycle hook): zero CPU billed while idle. Free tier fits.
- Behaviour: on a message from any socket, broadcast verbatim to every other
  socket in the room. No persistence, no auth at the broker level
  (encryption is end-to-end). Idle DO hibernates, wakes on next message.
- `wrangler.toml` sets the DO binding and migrations and points at
  `crates/catsocks` as the build root. CI deploy command:
  `wrangler deploy --cwd crates/catsocks`.

## Default logic (`default-logic/default.rhai`)

Capability-bound API exposed to Rhai (registered in Rust):

```
fn set_rotation(urls, default_duration_secs)   // round-robin
fn nav(url)                                    // immediate, no resume
fn nav(url, duration_secs)                     // pauses rotation, resumes after
fn pause()                                     // freeze rotation
fn play()                                      // resume rotation
fn schedule(cron_expr, command_fn)             // cron via `cron` crate
fn on_event(name, fn)                          // future hook surface
```

`default.rhai` reads the current `config.yaml` (passed in as a Rhai `Map`) and
wires `set_rotation` + a `schedule` call per `fixed[]` entry. Shipped as a
separate file the CLI can push, **not** baked into the stage binary.

Example `examples/config.yaml`:
```yaml
version: 1                                    # required; used for migrations
rotation:
  default_duration: 30s
  urls:
    - https://intranet.example.com/dashboard  # short form — uses default
    - url: https://intranet.example.com/news  # long form — per-URL override
      duration: 60s
fixed:
  - cron: "0 9 * * MON-FRI"
    url: https://intranet.example.com/standup
    duration: 30m
```

Schema rules (validated in `catcast-core::Config::validate`):

- `version` is required. Unknown future versions are a hard error; missing
  field is a hard error. Migrations (when we have v2) read the old version
  and rewrite to current.
- `rotation.urls` accepts each entry as either a bare string (URL using
  `default_duration`) or an object `{ url, duration }`. serde's untagged
  enum handles both.
- **Overlap detection** for `fixed[]`: at config-load time we expand each
  cron expression for the next 7 days using the `cron` crate, build
  `(start, end = start + duration)` intervals per entry, and emit a
  warning per overlapping pair: e.g.
  `Warning: fixed entry "0 9 * * MON-FRI" (30m) overlaps "30 9 * * MON" (15m)`.
  Warnings are returned by `validate()` and shown by the CLI on
  `catc config import`, and logged by the stage on load.
- Validation never refuses on overlap (operator may want it); only warns.

## GitHub Actions

- **ci.yml** (push, PR): `cargo fmt --check`, `cargo clippy -D warnings`,
  `cargo test`. Runs on `windows-latest`.
- **release.yml** (`v*` tag): build `catstage.exe` and `catc.exe`, generate
  Tauri updater bundle + signature, upload to GH Release, write `latest.json`
  manifest. Updater signing key in repo secrets.
- **socks-deploy.yml** (push to `main` touching `crates/catsocks/**`): install
  `worker-build`, then `wrangler deploy --cwd crates/catsocks`. CF API token
  in repo secrets.

## Critical files to create

- `Cargo.toml` (workspace)
- `crates/catcast-core/src/lib.rs` (types, schema versioning)
- `crates/catcast-proto/src/lib.rs` (envelope, KDF, AEAD, message types)
- `crates/catstage/tauri.conf.json` + `src/main.rs` + `src/socks.rs` +
  `src/scheduler.rs` + `src/logic.rs` + `src/persist.rs` + `src/autostart.rs` +
  `src/hotkey.rs` + `ui/index.html` + `ui/manual.html`
- `crates/catc/src/main.rs` + one module per subcommand group
- `crates/catsocks/src/lib.rs`, `crates/catsocks/Cargo.toml`,
  `crates/catsocks/wrangler.toml`
- `default-logic/default.rhai`
- `examples/config.yaml`, `examples/aliases.yaml`
- `LICENSE`, `README.md`, `CONTRIBUTING.md`, `.github/workflows/*.yml`

## Crates / libs to lean on (don't reinvent)

- `tauri` 2.x, `tauri-plugin-updater`
- `wry` (transitively via Tauri)
- `rhai` for scripting
- `cron` for cron parsing, `chrono` for time
- `chacha20poly1305`, `argon2`, `rand` for crypto
- `tokio`, `tokio-tungstenite` for async + WS client
- `serde`, `serde_json`, `serde_yaml` for config
- `clap` v4 for CLI
- `rusqlite` *or* `serde_json` for state — start with JSON; SQLite only if a
  persistence pain emerges
- `directories` for cross-platform paths (Windows-only today, but free
  insurance against future macOS/Linux ports)
- `global-hotkey` for Ctrl+Alt+M
- `mslnk` for Startup folder shortcut
- Broker side: `worker` crate (Cloudflare's Rust SDK), `worker-build` (build
  tool, dev-only), `wrangler` (deploy tool, dev-only)

## Verification plan

Local end-to-end on a Windows 11 box:

1. `cd crates/catsocks && wrangler dev` → broker on
   `ws://localhost:8787/r/default` (wrangler invokes `worker-build` to compile
   the Rust crate to WASM).
2. `cargo run -p catstage -- --socks ws://localhost:8787/r/default --name kitchen`
   → fullscreen window shows the `catcast://about` page with the name "kitchen".
3. `cargo run -p catc -- init --socks ws://localhost:8787/r/default` then
   `cargo run -p catc -- add-stage kitchen` (operator reads "kitchen" off the
   stage's catcast://about page in step 2).
4. `cargo run -p catc -- targets list --probe` → reports `kitchen: up`.
5. `cargo run -p catc -- logic import default-logic/default.rhai --name kitchen`
   `cargo run -p catc -- config import examples/config.yaml --name kitchen`
   → CLI prints any overlap warnings. Rotation begins.
6. `catc nav https://example.com --for 1m --name kitchen` → interrupts rotation;
   resumes after 1 minute.
7. `catc manual on --name kitchen` → manual overlay appears with URL bar.
   Type `catcast://about` in the URL bar → info page renders with stage
   metadata. Press Ctrl+Alt+M on the stage → toggles back to rotation.
   Verify cookies persist after logging into a test SSO page and toggling to
   manual again.
8. `catc pause / play` → rotation halts/resumes.
9. Edit `examples/config.yaml` to introduce two overlapping `fixed` entries.
   `catc config import …` → CLI prints warnings; stage logs them; rotation
   still starts.
10. Long form vs short form: replace one rotation URL with the
    `{ url, duration }` form and confirm its custom duration is honoured.
11. **Active-set targeting.** Run a second stage `cargo run -p catstage --
    --socks ws://localhost:8787/r/default --name lobby`, then `catc add-stage
    lobby`. `catc deactivate lobby`, then `catc pause` (no `--name`) →
    `kitchen` pauses, `lobby` keeps rotating. `catc pause --all` → both
    pause. `catc activate lobby`, then `catc play` → both resume.
12. **Negative test for discovery.** From a *second* CLI install with a
    fresh `catc.toml` (broker URL only, no `add-stage` yet), run
    `catc targets list --probe` → reports nothing. Confirms a CLI without
    pre-registered names cannot enumerate stages on the room.
13. Kill `wrangler dev`, reboot the stage VM with the Startup shortcut →
    stage comes up, shows last config from disk, retries broker silently.
14. **Run the auto-update spike (see Auto-update section).** Only after the
    spike picks a delivery path, implement the updater wiring; until then,
    bumping a version is a manual EXE swap.
15. Deploy `crates/catsocks/` to Cloudflare via `wrangler deploy`; repeat
    steps 2–13 against the public
    `wss://catsocks.<account>.workers.dev/r/<room>`.

Acceptance: all ten steps work without admin rights and without code-signing
beyond the Tauri updater's own Ed25519 signature.

## Out of scope (deliberately deferred)

- macOS, Linux, ARM builds (defer).
- Master-data management of state across many stages (per user request).
- Asymmetric command auth / multi-tenant ACLs (PSK suffices for stated threat
  model).
- Detailed observability / metrics on the broker (free tier won't carry it
  anyway).
- Migration tooling for state schema changes (handle when v2 actually lands).

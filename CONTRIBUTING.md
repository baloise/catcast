# Contributing to CatCast

CatCast is released under [0BSD](LICENSE), in the spirit of CopyHeart: copy,
fork, share, remix. No attribution required. No CLA. No DCO.

## How to help

- **Open an issue** if something's broken, confusing, or missing.
- **Send a pull request** if you fixed it. Small PRs are easier to merge
  than big ones; one logical change per PR.
- **Don't ask permission.** If you're unsure whether a change fits, open
  it as a draft and we'll talk in the PR.
- **Documentation patches** count. Typo fixes count. A good README
  example is worth a feature.

## Commit style

[Conventional Commits](https://www.conventionalcommits.org), loose form:

    feat:     a user-visible feature
    fix:      a bug fix
    refactor: behaviour unchanged
    docs:     prose / README / comments
    chore:    deps, tooling, CI
    test:     tests only
    perf:     measurable performance win

Append `!` (e.g. `feat!:`) or a `BREAKING CHANGE:` footer for anything
that breaks the wire protocol, on-disk format, or CLI surface — that's
how the maintainer knows to cut a `major`.

We don't lint this. Best-effort is enough; the goal is readable release
notes and an obvious bump level, not bureaucracy.

## What we'll merge

Roughly in order of preference:

1. Bug fixes with a regression test.
2. Features that fit the existing scope and don't break the "drop EXE,
   walk away" promise.
3. Platform ports (macOS / Linux / ARM) — see the plan; these are
   wanted but not blocking v1.
4. Refactors that visibly simplify something.

## What we probably won't merge

- Things that add accounts, telemetry, or a "master server".
- Adding a heavy framework where a 50-line module would do.
- Speculative abstractions for hypothetical use cases.

If in doubt, open an issue first.

## Local development

CatCast is a Rust workspace; `cargo build --workspace` from the repo root
should be the only thing you need. Tests run with `cargo test --workspace`.

**Linux / WSL is the recommended dev path** even though v1 ships
Windows-only binaries. Inner-loop on Linux for fast iteration, then push a
tag — GitHub Actions builds and publishes `catstage.exe` and `catc.exe` from
`windows-latest` (see `.github/workflows/release.yml`). CI also runs on
`ubuntu-latest` and `windows-latest` in parallel so you'll catch
platform-specific breakage before it lands.

Linux dev needs the WebKitGTK toolchain so catstage's Tauri shell can
build:

```bash
sudo apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev \
    libjavascriptcoregtk-4.1-dev libxdo-dev
```

(Equivalents exist on Fedora / Arch — check the Tauri 2 prerequisites
page if you're on a non-Debian distro.)

### Running the whole stack locally

Three components, three terminals. Run the native `catsocks` broker (a
tiny axum/tokio relay with the same wire shape as the Cloudflare Worker)
so you don't need Node, wrangler, or a CF account just to iterate.

**Terminal 1 — the broker:**

```bash
cargo run -p catsocks
# listening on ws://127.0.0.1:8787/r/<room>
```

**Terminal 2 — a stage:**

```bash
cargo run -p catstage -- --socks ws://127.0.0.1:8787/r/dev --name kitchen --screen 2
```

`--screen` is optional and 1-based (`--screen 2` means second monitor).
If the selected monitor is unavailable, catstage falls back to the primary
monitor.

Tauri opens a fullscreen window pointed at `catcast://about` — a
**read-only** info page showing the stage's name, version, connection
state, on-disk files, and autostart status. There are no buttons. Every
operator action is a `catc` command.

**Terminal 3 — the CLI:**

```bash
# Tell catc where the broker is; writes ~/.config/catc/catc.toml.
cargo run -p catc -- init --socks ws://127.0.0.1:8787/r/dev

# Register the stage by name (read the name off the about page in
# terminal 2). The CLI derives an Argon2id key for this PSK.
cargo run -p catc -- stage add kitchen

# Round-trip a probe: catc sends an encrypted GetState, catstage
# replies with State, catc decrypts and reports.
cargo run -p catc -- stage list --probe
#   up    active  kitchen

# Push a default config + logic so the stage starts rotating.
cargo run -p catc -- logic  import default-logic/default.rhai
cargo run -p catc -- config import examples/config.yaml

# Drive the stage. Three modes: Playing / Paused / Idle.
cargo run -p catc -- pause                                   # freeze on current URL (Paused)
cargo run -p catc -- play                                    # resume rotation (Playing)
cargo run -p catc -- about                                   # park on about page (Idle)
cargo run -p catc -- forward                                 # step one slot forward
cargo run -p catc -- back                                    # step one slot backward
cargo run -p catc -- nav https://example.com --for 1m        # timed interrupt, auto-resume
cargo run -p catc -- fullscreen on                           # window state
cargo run -p catc -- devtools on                             # remote DevTools

# Manage autostart (Startup-folder .lnk on Windows; no-op elsewhere):
cargo run -p catc -- autostart install                       # uses the stage's current --socks/--name
cargo run -p catc -- autostart uninstall

# Clean shutdown:
cargo run -p catc -- shutdown
```

The about page has a single client-side shortcut: **F1** brings the
kiosk back to the admin page from any URL (pure JS `location.href` —
works on rotation URLs where the Tauri IPC bridge isn't injected) and
flips the stage to `Idle` so rotation doesn't yank the operator off
again at the next slot. It's equivalent to `catc about`. All other
operator actions are CLI commands that travel over the broker.

`catc` discovers `catc.toml` (and its sibling `aliases.yaml`) by
precedence: `$CATC_CONFIG` if set, else `./catc.toml` in the cwd if it
exists, else the platform default (`~/.config/catc/` on Linux). To keep
your real config untouched while hacking, use one of:

```bash
# Throwaway file via env var:
CATC_CONFIG=/tmp/catc-dev.toml cargo run -p catc -- init --socks ws://127.0.0.1:8787/r/dev
# …keep CATC_CONFIG set (or `export` it) for subsequent invocations.

# Or just drop a catc.toml in a scratch dir and cd into it:
mkdir /tmp/catc-dev && cd /tmp/catc-dev && touch catc.toml
cargo run -p catc -- init --socks ws://127.0.0.1:8787/r/dev
```

To wipe an existing dev state on the stage side, delete
`~/.config/catcast/{config.yaml,logic.rhai,state.json}`.

### Cloudflare Worker deploy

The production broker (`crates/catsocks`) is a Rust crate compiled to
`wasm32-unknown-unknown` and deployed to Cloudflare Workers:

```bash
rustup target add wasm32-unknown-unknown
cargo install -q worker-build
cd crates/catsocks
wrangler dev      # local CF Workers runtime
wrangler deploy   # publish
```

Both `wrangler` and `worker-build` are dev-only — they're not in
`Cargo.toml`. CI's `socks-deploy.yml` job invokes them on pushes to
`main` that touch `crates/catsocks/**`.

## Cutting a release

Releases are tag-driven. Tag a commit `vX.Y.Z` on `main` and GitHub
Actions builds + publishes binaries for Windows, Linux, and macOS-ARM
(see [release.yml](.github/workflows/release.yml)). No crates.io step.

The local flow uses [`cargo-release`](https://github.com/crate-ci/cargo-release)
to bump all five crates in lockstep, commit, tag, and push:

```bash
cargo install cargo-release   # first time only

# Dry-run: print what would happen, change nothing.
cargo release patch

# Guided: one confirm before each phase (commit, tag, push).
cargo release patch -x

# Autopilot.
cargo release patch -x --no-confirm
```

Use `minor` / `major` instead of `patch` for non-patch bumps; `rc` /
`beta` for pre-releases. cargo-release reads config from
`[workspace.metadata.release]` in the root `Cargo.toml`.

After the tag lands on origin, watch the **release** workflow in
Actions — the GitHub Release appears once all three OS matrix jobs
finish.

## Vibe

If you've ever wanted to add something to a piece of software but the
project's contribution process made it not worth it — please don't let
that be CatCast.

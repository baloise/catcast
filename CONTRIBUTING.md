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

Three components, three terminals. Use `catsocks-dev` (a tiny native
broker, same wire shape as the Cloudflare Worker) so you don't need
Node, wrangler, or a CF account just to iterate.

**Terminal 1 — the broker:**

```bash
cargo run -p catsocks --bin catsocks-dev
# listening on ws://127.0.0.1:8787/r/<room>
```

**Terminal 2 — a stage:**

```bash
cargo run -p catstage -- --socks ws://127.0.0.1:8787/r/dev --name kitchen
```

Tauri opens a fullscreen window pointed at `catcast://about` — that's
the info-and-control page. The header shows the stage name (`kitchen`).
The Autostart panel shows the onboarding wizard since no Startup-folder
shortcut is installed. The window stays on the about page until catc
pushes a config + logic.

**Terminal 3 — the CLI:**

```bash
# Tell catc where the broker is; writes ~/.config/catc/catc.toml.
cargo run -p catc -- init --socks ws://127.0.0.1:8787/r/dev

# Register the stage by name (read the name off the about page in
# terminal 2). The CLI derives an Argon2id key for this PSK.
cargo run -p catc -- add-stage kitchen

# Round-trip a probe: catc sends an encrypted GetState, catstage
# replies with State, catc decrypts and reports.
cargo run -p catc -- targets list --probe
#   up    active  kitchen

# Push a default config + logic so the stage starts rotating.
cargo run -p catc -- logic  import default-logic/default.rhai
cargo run -p catc -- config import examples/config.yaml

# Interrupt rotation with a timed nav.
cargo run -p catc -- nav https://example.com --for 1m
```

If you want to keep your real `~/.config/catc/` untouched while
hacking, point catc at a throwaway dir:

```bash
HOME=$(mktemp -d) cargo run -p catc -- init --socks ws://127.0.0.1:8787/r/dev
# …subsequent `catc` invocations need the same HOME override.
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

## Vibe

If you've ever wanted to add something to a piece of software but the
project's contribution process made it not worth it — please don't let
that be CatCast.

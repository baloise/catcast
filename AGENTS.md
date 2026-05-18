# AGENTS.md

Catcast is a Rust workspace (5 crates: `catcast-core`, `catcast-proto`,
`catc`, `catstage`, `catsocks`). See [CONTRIBUTING.md](CONTRIBUTING.md)
for the full dev guide; this file is the short version for AI tooling.

## Build / check / test

    cargo build --workspace
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

CI runs these on Ubuntu, Windows, and macOS — match all four locally
before opening a PR.

## When you finish a task

Run `cargo fmt --all` before handing off (or committing). The
`fmt --check` step above is the CI gate; running `--all` once at the end
keeps subsequent diffs noise-free.

## Commit style

Conventional Commits (`feat:`, `fix:`, `refactor:`, `docs:`, `chore:`,
`test:`, `perf:`). Append `!` or a `BREAKING CHANGE:` footer for
wire-protocol / CLI-surface breaks. Not CI-enforced; do your best.

## Scope discipline

One logical change per PR. No speculative abstractions, no telemetry,
no "master server" — see [CONTRIBUTING.md](CONTRIBUTING.md#what-well-merge)
for the merge bar.

## What lives where

- `crates/catcast-core` — shared types: config schema + state
- `crates/catcast-proto` — wire protocol + crypto (Argon2id, XChaCha20-Poly1305)
- `crates/catc` — operator CLI
- `crates/catstage` — Tauri kiosk shell (the EXE that ships)
- `crates/catsocks` — relay broker (native binary + Cloudflare Worker build)

## Releases

Tag-driven via `cargo-release`. Don't bump versions by hand. See the
"Cutting a release" section in CONTRIBUTING.

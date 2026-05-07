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

When `catstage` gains its Tauri/WebView2 shell, Linux dev will additionally
need the WebKitGTK toolchain (`libwebkit2gtk-4.1-dev`, `libsoup-3.0-dev`,
and friends). Until then, plain `cargo` is enough.

The CF Worker (`crates/catsocks`) needs `wrangler` for deploy and
`worker-build` for compilation; both are dev-only.

## Vibe

If you've ever wanted to add something to a piece of software but the
project's contribution process made it not worth it — please don't let
that be CatCast.

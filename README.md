# CatCast

<img src="CastCast_logo_plain.svg" alt="CatCast logo" width="107" align="right" hspace="17">

A tiny digital signage stack for places where you can't install software as
admin. Three pieces:

- **catstage** — fullscreen viewer (Tauri / WebView2). Drop the EXE on a
  machine, point it at a broker, walk away.
- **catsocks** — dumb WebSocket relay. Runs on Cloudflare Workers' free
  tier. Sees only opaque ciphertext.
- **catc** — the CLI. Drives one or many stages from your laptop.
- **catproxy** — optional same-origin rewriting proxy (Cloudflare Worker) for
  pages whose sub-resources get lost behind per-domain authenticating corporate
  proxies. See [workers/catproxy](workers/catproxy/README.md), including
  what to do when a kiosk lands on the corporate proxy's login page and
  how `catc trace` shows where a stage really is.

End-to-end encrypted. No accounts, no master server, no telemetry. Stage
state survives reboots and broker outages.

## Status

Early. v1 targets Windows x64 only — macOS / Linux / ARM distribution
may follow. The first install of a stage is a manual EXE drop; after
that `catc stage update` pushes a release to running stages: each one
downloads the asset (through catproxy's `/b64/` route as text, so a
content-sniffing corporate proxy never sees an executable), verifies the
SHA-256 published with the release, swaps the binary in beside itself,
relaunches with its own arguments and keeps the previous binary as
`.old` for one generation. Nothing updates on its own — there is no
periodic check, by design. WSL/Linux is the recommended dev path.

## Quick start

```bash
# 1. Deploy the broker (once per deployment)
cd crates/catsocks && wrangler deploy
# -> wss://catsocks.<your-acct>.workers.dev/r/<your-room>

# 2. On each stage machine
catstage.exe --install-autostart --socks wss://.../r/<room> --name kitchen --screen 2
# Optional: --screen is 1-based. If unavailable, catstage falls back to primary monitor.
# --socks is remembered: later starts can be plain `catstage.exe`.

# 3. On your laptop
catc init --socks wss://.../r/<room>
catc stage add kitchen        # the name shows on the stage's catcast://about page
catc config import config.yaml --name kitchen
catc nav https://example.com --for 5m
```

See `examples/` for `config.yaml`, `aliases.yaml`, and a default Rhai logic.

## License

[0BSD](LICENSE). Use it. Fork it. Strip the credits. Sell it. Whatever.

## Contributing

Yes please. See [CONTRIBUTING.md](CONTRIBUTING.md). No CLA, no DCO, no
gatekeeping. Send a patch, an issue, or a sketch on a napkin.

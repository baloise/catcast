<table>
<tr>
<td>

# CatCast

A tiny digital signage stack for places where you can't install software as
admin. Three pieces:

</td>
<td valign="top" align="right">
<img src="CastCast_logo_plain.svg" alt="CatCast logo" width="107">
</td>
</tr>
</table>

- **catstage** — fullscreen viewer (Tauri / WebView2). Drop the EXE on a
  machine, point it at a broker, walk away.
- **catsocks** — dumb WebSocket relay. Runs on Cloudflare Workers' free
  tier. Sees only opaque ciphertext.
- **catc** — the CLI. Drives one or many stages from your laptop.

End-to-end encrypted. No accounts, no master server, no telemetry. Stage
state survives reboots and broker outages.

## Status

Early. v1 targets Windows x64 only — macOS / Linux / ARM distribution
may follow. Auto-update is **not** in v1 (corp proxies block executable
downloads in our test environment); updating a stage is a manual EXE
swap. WSL/Linux is the recommended dev path.

## Quick start

```bash
# 1. Deploy the broker (once per deployment)
cd crates/catsocks && wrangler deploy
# -> wss://catsocks.<your-acct>.workers.dev/r/<your-room>

# 2. On each stage machine
catstage.exe --install-autostart --socks wss://.../r/<room> --name kitchen

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

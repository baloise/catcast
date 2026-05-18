# Corp-proxy auto-update spike

We need to find out which download paths a corporate content-inspection
proxy actually lets through *before* writing the catstage auto-update
wiring. The default Tauri updater pulls `.exe.zip` from GitHub Releases
— that may or may not survive the proxy. This spike probes several
plausible delivery paths against a real corp-network machine and reports
which ones return data vs. get blocked / quarantined / replaced with a
proxy interstitial.

## How to run

On the **actual target machine** (the Windows box that will host
catstage), open PowerShell and run:

```powershell
cd \path\to\catcast\spike\corp-proxy
.\probe.ps1 | Tee-Object -FilePath probe-results.txt
```

Or from WSL / a Linux dev box (only useful if WSL is subject to the
same proxy, which depends on your `WSL_*_PROXY` setup):

```bash
cd spike/corp-proxy
./probe.sh | tee probe-results.txt
```

The script makes ~6 HTTP requests against known-good URLs that
represent each candidate delivery channel:

1. `https://github.com/.../releases/download/...` — the default Tauri
   updater path. If this works, we're done; ship as-is.
2. `https://api.github.com/...` — the JSON API. If this works but #1
   doesn't, the proxy is blocking based on path/file-extension, not
   the github.com hostname.
3. `https://raw.githubusercontent.com/...` — the raw-file CDN. Some
   proxies block this specifically.
4. `https://objects.githubusercontent.com/...` — the actual binary
   blob store. If #1 redirects here and gets blocked at this step,
   the workaround is to proxy through our own CF Worker.
5. `https://*.workers.dev/...` — a Cloudflare Worker URL. If this is
   allowed, the workaround is: stand up a CF Worker that re-streams
   the binary with a friendly content-type.
6. A small ASCII / text-mimetype URL — sanity check that *anything*
   gets through.

For each, the script reports: HTTP status, final URL after redirects,
content-length, content-type, and (where supported) whether the body
looks like the expected bytes or has been replaced with HTML.

## How to interpret the results

Pick the **lowest-friction path that worked**. The decision tree is in
the [plan](../../initial_plan.md) under "Auto-update":

1. If #1 works → use Tauri's default updater. Done.
2. Else if #5 works → wire a CF Worker that re-streams the release
   binary; point the Tauri updater at the Worker URL.
3. Else → repackage updates as base64 `.txt` (and write a tiny
   apply-update unwrap stage). Re-spike to confirm `.txt` passes.
4. Else (extreme) → stream as Server-Sent Events of base64 chunks.

Don't implement the updater wiring until this spike has picked one.

## What to capture and send back

Just `probe-results.txt`. The script prints redactable details (no
authentication tokens, no internal hostnames) so it's safe to share.

If you see a proxy interstitial / HTML where a binary should be, save
the body as `interstitial.html` too — it sometimes tells us which
rule blocked the request.

## Findings so far (2026-05-11)

Run from a corp-network WSL host (proxy `HTTP_PROXY` set to a generic
gateway) **and** from native Windows 11 via PowerShell over WSL
interop. Results disagree, which is itself informative:

- **WSL side:** GH Release `.zip` downloads pass through unmodified
  (ZIP magic verified in the first bytes). `raw.githubusercontent.com`
  is blocked by a Smoothwall/Forcepoint filter (`_sm_nck=1`
  fingerprint), but Tauri doesn't need it.
- **Windows side (corp content-inspection proxy via PAC):**
  GH Release `.zip` downloads **fail with "connection forcibly closed"
  by the remote host** — classic content-inspection block. The GH API
  (JSON) and `workers.dev` still pass.

Conclusion: **the default Tauri updater path is not viable on the
real stage target.** Pick one of these:

1. **CF Worker update proxy.** Build a small Worker that fetches the
   release binary server-side from GitHub (no content inspection in
   that hop) and streams it back to the stage. The stage downloads
   from `https://<account>.workers.dev/...`. Open risk: the corp
   proxy may still sniff the *response* stream and cut it when it
   sees binary bytes. Needs a follow-up probe — see
   `spike/cf-worker-proxy/` (TBD).
2. **Base64-text repackaging.** Encode the update payload as a `.txt`
   and have the stage decode-and-apply. Content inspection is more
   permissive on text MIME but still inspects bodies; needs its own
   probe.
3. **Manual updates.** Ship by hand. For 1–2 screens with rare
   updates this is honestly fine and matches the "install is tedious"
   tolerance baked into v1.

Until one of these is committed to, **the Tauri updater is not
wired in catstage**. Treat catstage as build-and-drop.

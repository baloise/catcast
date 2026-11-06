# Mirror a stage outside the network

For a screen in a building without access to the network your pages live
in. A stage **inside** (the relay) sends screenshots of what it shows to a
stage **outside** (the mirror), end-to-end encrypted through the same
broker. Pixels only: the mirror never loads the pages, so intranet logins,
JavaScript dashboards and canvas charts all come out as they look on the
relay. For static pages that change every few minutes; not for video or
animation.

```bash
# Outside: install and start a mirror. It picks a random 20-character name,
# prints it, and shows it on its about page (F1) as text and QR code. The
# name is also in ~/.config/catcast/mirror.name (%APPDATA%\catcast on
# Windows). --mirror is remembered: a plain `catstage` stays a mirror until
# `--no-mirror`.
catstage.exe --install-autostart --start --socks wss://.../r/<room>-relay --mirror

# Laptop: register both stages, then pair them.
catc stage add lobby-relay
catc stage add <mirror-name-from-the-qr>
catc relay pair --relay lobby-relay --mirror <mirror-name> [--settle 3s] [--every 60s]
catc status                 # the mirror shows under its relay: frame age, cache
catc relay cache --mirror <mirror-name> 256MB   # tile cache size, default 64MB
catc relay off --relay lobby-relay --mirror <mirror-name>
```

- **Mirrors are left out of "all stages".** `relay pair` marks the mirror
  in `catc.toml` (`catc status` does too when it finds one), and commands
  without `--name` (nav, pause, config import and export) skip it: a mirror
  shows its relay's screen. `catc stage update` still includes it, so one
  command updates relay and mirror. A mirror refuses nav and config itself.
- **The relay must be a visible screen.** It captures what its window
  shows; Windows stops painting a minimised or hidden WebView2. A real
  kiosk or a second monitor, not a minimised helper.
- **Only changed parts travel.** Screenshots are cut into 128-pixel tiles
  and the mirror keeps tiles by content. A page that comes back in the
  rotation costs a list of hashes; a clock ticking in a corner costs the
  one or two tiles under it. The mirror's tile cache holds 64 MB by default,
  dropping the least recently shown tiles first; raise it with
  `catc relay cache` for long rotations. An unchanged screen sends nothing but a
  heartbeat every 10 minutes. The mirror marks its picture **STALE** after
  25 minutes without word from the relay, and shows the last picture after
  a reboot.
- **Secrets.** `relay pair` generates a random frame key and hands it to
  both stages inside the encrypted channel, so screen content is never
  encrypted under a guessable name. The pairing is sent under the mirror's
  name, which is why that name is random and why `relay pair` refuses
  names shorter than 16 characters. Frame traffic cannot carry commands:
  whoever holds the mirror box cannot steer the relay.
- **Not captured:** the relay's about page (it shows the relay's name,
  which is its key) and any page the relay is stuck on, like a proxy
  login.
- **Give the pair its own broker room** (`/r/<room>-relay`, with its own
  `catc.toml` via `CATC_CONFIG`). Every stage in a room receives every
  frame; it cannot read them, but it pays for them.
- **Traffic.** A full 1080p dashboard is roughly 0.3–1.5 MB the first time
  it is seen and a few KB per change after that, well inside the
  Cloudflare free tier. Sustained encrypted upload from inside the network
  may still catch a data-loss-prevention policy's eye; try it for a day
  first. And these pages leave the perimeter by design: check that their
  owner agrees.

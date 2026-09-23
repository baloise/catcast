# catproxy

A generic **same-origin rewriting proxy** on Cloudflare Workers.

```
https://<worker>/<absolute upstream URL>
https://catproxy.example.workers.dev/https://example.org/board.html
```

It fetches the upstream URL and streams it back. For HTML and CSS it rewrites
every URL (`src`, `href`, `srcset`, `poster`, `action`, `url()`, `@import`,
meta refresh, redirects) so that they point back at the worker, and injects a
small runtime shim that does the same for URLs the page builds in JavaScript
(`fetch`, `XMLHttpRequest`, `img.src = …`, `innerHTML` templates, …). The
browser ends up talking to **one origin only**.

## Why

Kiosk / signage browsers often sit behind corporate web proxies that
authenticate the browser **per destination domain** (Zscaler cookie auth and
similar). A top-level page load can follow the login redirect; `<img>` and
`fetch()` sub-requests to another domain cannot, so once that domain's cookie
expires the page quietly loses its images or data until someone opens that
domain by hand. Serving everything through a single origin removes the
problem: the page's own domain is authenticated when the page loads, and
nothing else is ever contacted.

## Configuration

Set as Worker variables — nothing deployment-specific lives in this repo.

| Variable      | Required | Meaning |
|---------------|----------|---------|
| `ALLOW_HOSTS` | yes      | Comma-separated host globs the worker may fetch, e.g. `pages.example.com,*.cdn.example`. Everything else is refused with `403`. An empty list refuses everything, so the worker is never an open proxy. |
| `DEFAULT_URL` | no       | `GET /` redirects to `/<DEFAULT_URL>`. Handy as the single URL to put on a kiosk. |

Only `https://` upstreams are proxied.

## Local development

```bash
cd workers/catproxy && bun install   # or npm install
printf 'ALLOW_HOSTS=pages.example.com,*.cdn.example\nDEFAULT_URL=https://pages.example.com/board.html\n' > .dev.vars
npm run dev                 # wrangler dev, http://localhost:8787
curl -sI localhost:8787/https://pages.example.com/board.html
```

Behind a corporate HTTP proxy `wrangler dev`'s local runtime can't reach
upstreams (or, with `NODE_USE_ENV_PROXY=1`, stalls on gzip-encoded responses).
`scripts/serve-node.mjs` runs the same handler under plain Node with an
`HTMLRewriter` polyfill instead:

```bash
ALLOW_HOSTS='pages.example.com,*.cdn.example' NODE_USE_ENV_PROXY=1 npm run dev:node
```

## Deploy

`.github/workflows/proxy-deploy.yml` deploys on every push to `main` touching
`workers/catproxy/**`. It needs the repository secrets `CLOUDFLARE_API_TOKEN`
and `CLOUDFLARE_ACCOUNT_ID` (shared with catsocks) and the repository
**variables** `CATPROXY_ALLOW_HOSTS` and `CATPROXY_DEFAULT_URL`, which are
passed through as `--var`. Or by hand:

```bash
wrangler deploy --var 'ALLOW_HOSTS:pages.example.com,*.cdn.example' \
                --var 'DEFAULT_URL:https://pages.example.com/board.html'
```

## What it does and doesn't do

- Forwards method, body and headers (minus hop-by-hop, `Cookie`, `Origin`,
  `Referer`; `Referer` is set to the upstream URL). `Authorization` and custom
  headers pass through, so APIs behind the page keep working.
- Drops upstream `Content-Security-Policy`, `X-Frame-Options`, `Set-Cookie`
  and cross-origin isolation headers; rewrites `Location` on redirects.
- Caches static-looking assets (images, fonts, css, js, json, …) at the
  Cloudflare edge for an hour.
- Relative URLs work without rewriting, because the worker path mirrors the
  upstream path; `<base href>` is honoured then removed.
- Not handled: `WebSocket`, service workers, `EventSource`, URLs assembled in
  CSS via JavaScript (`el.style.backgroundImage = …`), and upstreams that
  need cookies. Add them when a real page needs them.

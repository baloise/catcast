// catproxy — a generic same-origin rewriting proxy.
//
//   https://<worker>/<absolute upstream URL>
//   e.g. https://catproxy.example.workers.dev/https://example.org/board.html
//
// Fetches the upstream URL, streams it back, and — for HTML/CSS — rewrites
// every URL so that all follow-up requests (images, scripts, fetch(), XHR)
// also come back to this worker. The browser therefore only ever talks to ONE
// origin. That is the point: kiosk browsers behind corporate proxies that
// authenticate per destination domain (Zscaler & co.) can keep one session
// alive but silently lose sub-resources on other domains.
//
// Configuration is via Worker variables only (nothing deployment-specific is
// committed):
//   ALLOW_HOSTS  required. Comma-separated host globs, e.g.
//                "pages.example.com,*.cdn.example,static.example.org".
//                Anything not matching is refused with 403. An empty list
//                refuses everything, so a misconfigured deploy is never an
//                open proxy.
//   DEFAULT_URL  optional. `GET /` redirects to `/<DEFAULT_URL>`.
//
// A second route, `/b64/<absolute upstream URL>`, streams the upstream body
// back base64-encoded as text/plain. Corporate proxies that block executable
// downloads by sniffing content see only text; the client decodes. Used by
// `catc stage update` to fetch release binaries onto kiosks.
import { catproxyShim } from './shim.js';

const SHIM_SRC = `(${catproxyShim.toString()})();`;

const URL_ATTRS = ['src', 'href', 'srcset', 'poster', 'action', 'formaction', 'data-src'];

// Request headers that must not be forwarded (hop-by-hop, or describing the
// client<->worker leg rather than the worker<->upstream leg).
const DROP_REQ_HEADERS = new Set([
  'host', 'cookie', 'origin', 'referer', 'connection', 'keep-alive', 'upgrade',
  'proxy-authenticate', 'proxy-authorization', 'te', 'trailer', 'transfer-encoding',
  'x-forwarded-for', 'x-forwarded-host', 'x-forwarded-proto', 'x-real-ip',
]);

// Response headers that would break same-origin delivery or leak upstream state.
// `content-encoding` / `content-length` go too: the runtime hands us a decoded
// body and re-encodes on the way out, so the upstream values no longer apply.
const DROP_RES_HEADERS = new Set([
  'content-security-policy', 'content-security-policy-report-only',
  'x-frame-options', 'set-cookie', 'strict-transport-security',
  'cross-origin-opener-policy', 'cross-origin-embedder-policy', 'cross-origin-resource-policy',
  'report-to', 'nel', 'alt-svc', 'connection', 'keep-alive', 'transfer-encoding',
  'content-encoding', 'content-length',
]);

const CACHEABLE_EXT = /\.(png|jpe?g|gif|webp|avif|svg|ico|woff2?|ttf|otf|css|js|mjs|json|mp4|webm)$/i;

export default {
  async fetch(request, env, ctx) {
    const self = new URL(request.url);
    if (self.pathname.startsWith(B64_PREFIX)) return handleB64(request, self, env, ctx);
    const target = parseTarget(self.pathname.slice(1) + self.search);

    if (!target) return handleRoot(self, env);
    if (target.protocol !== 'https:') return text('only https:// upstreams are proxied', 400);
    if (!hostAllowed(target.hostname, env.ALLOW_HOSTS)) {
      return text(`upstream host not allowed: ${target.hostname}`, 403);
    }

    const upstreamReq = buildUpstreamRequest(request, target);
    const cacheable = (request.method === 'GET' || request.method === 'HEAD') && CACHEABLE_EXT.test(target.pathname);
    const upstreamRes = await fetch(upstreamReq, cacheable
      ? { redirect: 'manual', cf: { cacheEverything: true, cacheTtl: 3600 } }
      : { redirect: 'manual' });

    const headers = filterResponseHeaders(upstreamRes.headers, self, target);
    const ctype = (upstreamRes.headers.get('content-type') || '').toLowerCase();

    if (ctype.startsWith('text/html') || ctype.startsWith('application/xhtml+xml')) {
      const res = new Response(upstreamRes.body, { status: upstreamRes.status, headers });
      return rewriteHtml(res, self, target);
    }
    if (ctype.startsWith('text/css')) {
      const css = await upstreamRes.text();
      return new Response(rewriteCss(css, self, target), { status: upstreamRes.status, headers });
    }
    return new Response(upstreamRes.body, { status: upstreamRes.status, headers });
  },
};

// `https://host/path?q` (the request path without its leading `/`) -> URL.
// Tolerates `https:/host` (some clients fold `//`).
function parseTarget(raw) {
  const m = raw.match(/^(https?):\/+(.+)$/i);
  if (!m) return null;
  try {
    return new URL(`${m[1].toLowerCase()}://${m[2]}`);
  } catch {
    return null;
  }
}

function handleRoot(self, env) {
  if (self.pathname === '/robots.txt') return text('User-agent: *\nDisallow: /\n');
  if (self.pathname === '/' && env.DEFAULT_URL) {
    return Response.redirect(`${self.origin}/${env.DEFAULT_URL}`, 302);
  }
  if (self.pathname === '/') {
    return text(
      'catproxy — same-origin rewriting proxy\n\n' +
      `usage: ${self.origin}/https://<allowed-host>/<path>\n` +
      `       ${self.origin}/b64/https://<allowed-host>/<file>   (body as base64 text)\n` +
      `allowed hosts: ${env.ALLOW_HOSTS || '(none configured)'}\n`,
    );
  }
  return text('not found', 404);
}

function hostAllowed(hostname, allowHosts) {
  const globs = String(allowHosts || '').split(',').map((s) => s.trim().toLowerCase()).filter(Boolean);
  const h = hostname.toLowerCase();
  return globs.some((g) => {
    const re = new RegExp(`^${g.split('*').map(escapeRe).join('.*')}$`);
    return re.test(h);
  });
}

function escapeRe(s) {
  return s.replace(/[.+?^${}()|[\]\\]/g, '\\$&');
}

function buildUpstreamRequest(request, target) {
  const headers = new Headers();
  for (const [k, v] of request.headers) {
    const key = k.toLowerCase();
    if (DROP_REQ_HEADERS.has(key) || key.startsWith('cf-')) continue;
    headers.set(k, v);
  }
  headers.set('referer', target.href);
  const hasBody = !['GET', 'HEAD'].includes(request.method);
  return new Request(target.href, {
    method: request.method,
    headers,
    body: hasBody ? request.body : undefined,
    duplex: 'half', // required by the Fetch spec when streaming a body; no-op where not
  });
}

function filterResponseHeaders(upstream, self, target) {
  const headers = new Headers();
  for (const [k, v] of upstream) {
    const key = k.toLowerCase();
    if (DROP_RES_HEADERS.has(key)) continue;
    if (key === 'location') {
      headers.set(k, proxifyUrl(v, self, target));
      continue;
    }
    headers.set(k, v);
  }
  return headers;
}

// Absolute-ise `u` against the upstream document and prefix it with our origin.
function proxifyUrl(u, self, target) {
  if (u == null) return u;
  u = String(u).trim();
  if (!u || /^(data:|blob:|about:|javascript:|mailto:|tel:|#)/i.test(u)) return u;
  if (u.startsWith(`${self.origin}/http`)) return u;
  let abs;
  try {
    abs = new URL(u, target).href;
  } catch {
    return u;
  }
  if (!/^https?:/i.test(abs)) return u;
  return `${self.origin}/${abs}`;
}

function proxifySrcset(v, self, target) {
  return String(v).split(',').map((part) => {
    const m = part.trim().match(/^(\S+)(\s+.*)?$/);
    return m ? proxifyUrl(m[1], self, target) + (m[2] || '') : part;
  }).join(', ');
}

function rewriteCss(css, self, target) {
  return css
    .replace(/url\(\s*(['"]?)([^'")]+)\1\s*\)/gi, (_, q, u) => `url(${q}${proxifyUrl(u, self, target)}${q})`)
    .replace(/@import\s+(['"])([^'"]+)\1/gi, (_, q, u) => `@import ${q}${proxifyUrl(u, self, target)}${q}`);
}

function rewriteHtml(res, self, target) {
  // `<base href>` may change the resolution base after <head> opened; when we
  // see one, tell the shim and drop the element (relative URLs must keep
  // resolving against our path, which mirrors the upstream path).
  const attrHandler = (name) => ({
    element(el) {
      const v = el.getAttribute(name);
      if (v == null) return;
      el.setAttribute(name, name === 'srcset' ? proxifySrcset(v, self, target) : proxifyUrl(v, self, target));
    },
  });

  let rw = new HTMLRewriter()
    .on('head', {
      element(el) {
        el.prepend(
          `<script>window.__CATPROXY__={base:${JSON.stringify(target.href)}};${SHIM_SRC}</script>`,
          { html: true },
        );
      },
    })
    .on('base[href]', {
      element(el) {
        let base;
        try { base = new URL(el.getAttribute('href'), target).href; } catch { return; }
        el.replace(`<script>window.__CATPROXY__.base=${JSON.stringify(base)};</script>`, { html: true });
      },
    })
    .on('meta[http-equiv]', {
      element(el) {
        if ((el.getAttribute('http-equiv') || '').toLowerCase() !== 'refresh') return;
        const c = el.getAttribute('content') || '';
        el.setAttribute('content', c.replace(/(url\s*=\s*)(['"]?)([^'"]+)\2/i,
          (_, lead, q, u) => `${lead}${q}${proxifyUrl(u, self, target)}${q}`));
      },
    })
    .on('[style]', {
      element(el) {
        el.setAttribute('style', rewriteCss(el.getAttribute('style') || '', self, target));
      },
    })
    .on('style', cssTextHandler(self, target));

  for (const a of URL_ATTRS) rw = rw.on(`[${a}]`, attrHandler(a));
  return rw.transform(res);
}

// <style> text arrives in chunks; buffer until the last one, then rewrite.
function cssTextHandler(self, target) {
  let buf = '';
  return {
    text(chunk) {
      buf += chunk.text;
      if (chunk.lastInTextNode) {
        chunk.replace(rewriteCss(buf, self, target), { html: false });
        buf = '';
      } else {
        chunk.remove();
      }
    },
  };
}

function text(body, status = 200) {
  return new Response(body, { status, headers: { 'content-type': 'text/plain; charset=utf-8' } });
}

// ---------------------------------------------------------------------------
// /b64/ — stream an upstream file as base64 text.

const B64_PREFIX = '/b64/';
const B64_MAX_HOPS = 5;
const REDIRECT_STATUS = new Set([301, 302, 303, 307, 308]);

async function handleB64(request, self, env, ctx) {
  if (request.method !== 'GET') return text('b64 route is GET only', 405);
  let target = parseTarget(self.pathname.slice(B64_PREFIX.length) + self.search);
  if (!target) return text(`usage: ${self.origin}/b64/https://<allowed-host>/<file>`, 400);
  if (target.protocol !== 'https:') return text('only https:// upstreams are proxied', 400);

  // The transcoded body is cached at the edge keyed by our own URL, so each
  // PoP encodes a given file once. (Caching the upstream fetch would not
  // help: release CDNs redirect to signed, rotating URLs.)
  const cache = globalThis.caches?.default;
  const cacheKey = new Request(self.href, { method: 'GET' });
  if (cache) {
    const hit = await cache.match(cacheKey);
    if (hit) return hit;
  }

  // Follow redirects by hand so every hop is checked against ALLOW_HOSTS —
  // GitHub release downloads bounce from github.com to a CDN host. Nothing
  // from the client request is forwarded: a `Range` would break alignment,
  // and cookies have no business at a file host.
  let upstream;
  for (let hop = 0; ; hop++) {
    if (!hostAllowed(target.hostname, env.ALLOW_HOSTS)) {
      return text(`upstream host not allowed: ${target.hostname}`, 403);
    }
    upstream = await fetch(target.href, {
      method: 'GET',
      headers: { accept: 'application/octet-stream', 'user-agent': 'catproxy-b64' },
      redirect: 'manual',
    });
    if (!REDIRECT_STATUS.has(upstream.status)) break;
    const loc = upstream.headers.get('location');
    await upstream.body?.cancel();
    if (!loc) return text(`upstream ${target.hostname} redirected without a Location`, 502);
    if (hop >= B64_MAX_HOPS) return text(`too many redirects from ${target.hostname}`, 502);
    target = new URL(loc, target);
  }
  if (upstream.status !== 200) {
    await upstream.body?.cancel();
    return text(`upstream returned HTTP ${upstream.status}`, 502);
  }

  const headers = new Headers({
    'content-type': 'text/plain; charset=us-ascii',
    'cache-control': 'public, max-age=3600',
    'x-catproxy-upstream': target.origin,
  });
  const len = upstream.headers.get('content-length');
  if (len) headers.set('x-catproxy-length', len); // decoded size, for progress / sanity checks

  const body = upstream.body ? upstream.body.pipeThrough(base64Stream()) : '';
  const out = new Response(body, { status: 200, headers });
  if (cache && ctx?.waitUntil) ctx.waitUntil(cache.put(cacheKey, out.clone()));
  return out;
}

// Base64 as a TransformStream. Fetch bodies arrive in arbitrary chunk sizes,
// so up to two bytes are carried over between chunks: every emitted piece
// encodes a multiple of three input bytes, and only flush() may pad.
function base64Stream() {
  let carry = new Uint8Array(0);
  return new TransformStream({
    transform(chunk, controller) {
      let buf = chunk;
      if (carry.length) {
        buf = new Uint8Array(carry.length + chunk.length);
        buf.set(carry);
        buf.set(chunk, carry.length);
      }
      const whole = buf.length - (buf.length % 3);
      carry = buf.slice(whole); // copy: `chunk` may be recycled by the runtime
      if (whole) controller.enqueue(encodeBase64(buf.subarray(0, whole)));
    },
    flush(controller) {
      if (carry.length) controller.enqueue(encodeBase64(carry));
    },
  });
}

const B64_ALPHABET = new TextEncoder().encode(
  'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/',
);
const B64_PAD = 61; // '='

// Standard base64 with padding, returned as ASCII bytes. Uses the native
// encoder where the runtime has one; the table loop keeps older runtimes
// (and `node --test`) on identical output.
function encodeBase64(bytes) {
  if (typeof bytes.toBase64 === 'function') return new TextEncoder().encode(bytes.toBase64());
  const n = bytes.length;
  const out = new Uint8Array(Math.ceil(n / 3) * 4);
  let i = 0;
  let o = 0;
  for (; i + 2 < n; i += 3) {
    const v = (bytes[i] << 16) | (bytes[i + 1] << 8) | bytes[i + 2];
    out[o++] = B64_ALPHABET[(v >> 18) & 63];
    out[o++] = B64_ALPHABET[(v >> 12) & 63];
    out[o++] = B64_ALPHABET[(v >> 6) & 63];
    out[o++] = B64_ALPHABET[v & 63];
  }
  const rem = n - i;
  if (rem === 1) {
    const v = bytes[i] << 16;
    out[o++] = B64_ALPHABET[(v >> 18) & 63];
    out[o++] = B64_ALPHABET[(v >> 12) & 63];
    out[o++] = B64_PAD;
    out[o++] = B64_PAD;
  } else if (rem === 2) {
    const v = (bytes[i] << 16) | (bytes[i + 1] << 8);
    out[o++] = B64_ALPHABET[(v >> 18) & 63];
    out[o++] = B64_ALPHABET[(v >> 12) & 63];
    out[o++] = B64_ALPHABET[(v >> 6) & 63];
    out[o++] = B64_PAD;
  }
  return out;
}

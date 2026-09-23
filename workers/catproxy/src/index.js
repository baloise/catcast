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
  async fetch(request, env) {
    const self = new URL(request.url);
    const target = parseTarget(self);

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

// `/https://host/path?q` -> URL. Tolerates `https:/host` (some clients fold `//`).
function parseTarget(self) {
  const raw = self.pathname.slice(1) + self.search;
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

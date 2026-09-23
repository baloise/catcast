// Run the worker under plain Node instead of `wrangler dev`.
//
// Useful behind corporate proxies: wrangler's local runtime either can't reach
// the internet at all or (with NODE_USE_ENV_PROXY=1) stalls on gzip-encoded
// upstreams. Node's fetch honours HTTPS_PROXY when NODE_USE_ENV_PROXY=1 is set.
//
//   npm install
//   ALLOW_HOSTS='a.example,*.b.example' DEFAULT_URL=https://a.example/x.html \
//   NODE_USE_ENV_PROXY=1 node scripts/serve-node.mjs [port]
//
// Dev-only extras:
//   UPSTREAM_OVERRIDES  comma-separated `prefix=replacement` pairs applied to
//                       upstream URLs, e.g. test an unpublished page by mapping
//                       `https://pages.example.com/site/=http://127.0.0.1:8000/`.
//   The upstream User-Agent is replaced by a neutral one: corporate proxies
//   start browser login flows based on browser UAs, which would otherwise be
//   relayed back to your browser as a redirect to the proxy's login gateway.
//
// Not for production — Cloudflare-only fetch options (`cf`) are ignored here.
import http from 'node:http';
import { HTMLRewriter } from '@miniflare/html-rewriter';
import worker from '../src/index.js';

globalThis.HTMLRewriter ??= HTMLRewriter;

const port = Number(process.argv[2] || process.env.PORT || 8787);
const env = { ALLOW_HOSTS: process.env.ALLOW_HOSTS || '', DEFAULT_URL: process.env.DEFAULT_URL || '' };

const overrides = (process.env.UPSTREAM_OVERRIDES || '').split(',').filter(Boolean)
  .map((pair) => pair.split('='));
const realFetch = globalThis.fetch;
globalThis.fetch = (input, init) => {
  const req = input instanceof Request ? input : new Request(input, init);
  let url = req.url;
  for (const [prefix, replacement] of overrides) {
    if (url.startsWith(prefix)) { url = replacement + url.slice(prefix.length); break; }
  }
  const headers = new Headers(req.headers);
  headers.set('user-agent', 'catproxy-dev/1.0');
  return realFetch(new Request(url, { method: req.method, headers, body: req.body, duplex: 'half', redirect: 'manual' }), init);
};

http.createServer(async (req, res) => {
  const url = `http://${req.headers.host || `localhost:${port}`}${req.url}`;
  const hasBody = !['GET', 'HEAD'].includes(req.method);
  const request = new Request(url, {
    method: req.method,
    headers: req.headers,
    body: hasBody ? req : undefined,
    duplex: 'half',
  });
  try {
    const out = await worker.fetch(request, env);
    res.writeHead(out.status, Object.fromEntries(out.headers));
    if (out.body) for await (const chunk of out.body) res.write(chunk);
    res.end();
  } catch (e) {
    console.error(e);
    res.writeHead(502, { 'content-type': 'text/plain' }).end(`catproxy error: ${e.message}`);
  }
  console.log(req.method, req.url, res.statusCode);
}).listen(port, '127.0.0.1', () => console.log(`catproxy (node) on http://127.0.0.1:${port}`));

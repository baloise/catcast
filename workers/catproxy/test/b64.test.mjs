// `node --test` — no network, no wrangler: `fetch` is stubbed per test.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import worker from '../src/index.js';

const ENV = { ALLOW_HOSTS: 'github.com,objects.githubusercontent.com' };
const ctx = { waitUntil() {} };

function seeded(n, seed = 0x9e3779b9) {
  const out = new Uint8Array(n);
  let x = seed >>> 0;
  for (let i = 0; i < n; i++) {
    x ^= x << 13; x >>>= 0;
    x ^= x >>> 17;
    x ^= x << 5; x >>>= 0;
    out[i] = x & 255;
  }
  return out;
}

// Deliver `bytes` as a ReadableStream cut into the given chunk sizes (cycled).
function chunked(bytes, sizes) {
  let pos = 0;
  let k = 0;
  return new ReadableStream({
    pull(controller) {
      if (pos >= bytes.length) return controller.close();
      const size = sizes[k++ % sizes.length];
      controller.enqueue(bytes.slice(pos, pos + size));
      pos += size;
    },
  });
}

// routes: url -> () => Response
function stubFetch(routes) {
  const calls = [];
  globalThis.fetch = async (input, init) => {
    const url = typeof input === 'string' ? input : input.url;
    calls.push({ url, init });
    const handler = routes[url];
    if (!handler) return new Response('not found', { status: 404 });
    return handler();
  };
  return calls;
}

const get = (path, init) => worker.fetch(new Request(`https://proxy.test${path}`, init), ENV, ctx);

test('round-trips a 10 MB body delivered in awkward chunk sizes', async () => {
  const input = seeded(10 * 1024 * 1024 + 1);
  stubFetch({
    'https://objects.githubusercontent.com/blob': () =>
      new Response(chunked(input, [1, 2, 3, 4, 5, 7, 4093, 65537, 3, 1]), {
        status: 200,
        headers: { 'content-length': String(input.length) },
      }),
  });
  const res = await get('/b64/https://objects.githubusercontent.com/blob');
  assert.equal(res.status, 200);
  assert.equal(res.headers.get('content-type'), 'text/plain; charset=us-ascii');
  assert.equal(res.headers.get('x-catproxy-length'), String(input.length));
  const text = await res.text();
  assert.match(text, /^[A-Za-z0-9+/]+=?=?$/, 'body is one clean base64 run: padding only at the end');
  assert.equal(Buffer.compare(Buffer.from(text, 'base64'), Buffer.from(input)), 0);
});

test('pads correctly for every remainder', async () => {
  for (const n of [0, 1, 2, 3, 4, 5, 6]) {
    const input = seeded(n, 7 + n);
    stubFetch({ 'https://github.com/f': () => new Response(chunked(input, [1]), { status: 200 }) });
    const text = await (await get('/b64/https://github.com/f')).text();
    assert.equal(text, Buffer.from(input).toString('base64'), `n=${n}`);
  }
});

test('follows the github → CDN redirect and reports the final origin', async () => {
  const calls = stubFetch({
    'https://github.com/baloise/catcast/releases/download/v1/x.exe': () =>
      new Response(null, { status: 302, headers: { location: 'https://objects.githubusercontent.com/x?sig=1' } }),
    'https://objects.githubusercontent.com/x?sig=1': () => new Response('MZ', { status: 200 }),
  });
  const res = await get('/b64/https://github.com/baloise/catcast/releases/download/v1/x.exe');
  assert.equal(res.status, 200);
  assert.equal(await res.text(), Buffer.from('MZ').toString('base64'));
  assert.equal(res.headers.get('x-catproxy-upstream'), 'https://objects.githubusercontent.com');
  assert.equal(calls.length, 2);
  for (const c of calls) {
    assert.equal(c.init.redirect, 'manual');
    assert.equal(c.init.headers.cookie, undefined);
  }
});

test('refuses a redirect to a host outside ALLOW_HOSTS', async () => {
  stubFetch({
    'https://github.com/r': () => new Response(null, { status: 302, headers: { location: 'https://evil.example/x' } }),
  });
  const res = await get('/b64/https://github.com/r');
  assert.equal(res.status, 403);
});

test('gives up after too many redirects', async () => {
  stubFetch({
    'https://github.com/loop': () => new Response(null, { status: 302, headers: { location: '/loop' } }),
  });
  const res = await get('/b64/https://github.com/loop');
  assert.equal(res.status, 502);
  assert.match(await res.text(), /too many redirects/);
});

test('maps upstream errors to 502 without transcoding them', async () => {
  stubFetch({ 'https://github.com/missing': () => new Response('nope', { status: 404 }) });
  const res = await get('/b64/https://github.com/missing');
  assert.equal(res.status, 502);
  assert.match(await res.text(), /HTTP 404/);
});

test('rejects non-GET, http:// and malformed targets, and hosts not allowed', async () => {
  stubFetch({});
  assert.equal((await get('/b64/https://github.com/x', { method: 'POST' })).status, 405);
  assert.equal((await get('/b64/http://github.com/x')).status, 400);
  assert.equal((await get('/b64/')).status, 400);
  assert.equal((await get('/b64/https://evil.example/x')).status, 403);
});

test('the plain route still works after the refactor', async () => {
  stubFetch({
    'https://github.com/page.txt': () => new Response('hi', { status: 200, headers: { 'content-type': 'text/plain' } }),
  });
  const res = await get('/https://github.com/page.txt');
  assert.equal(res.status, 200);
  assert.equal(await res.text(), 'hi');
});

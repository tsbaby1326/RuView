// Executed tests for the service worker's request routing (ADR-271/272).
//
// WHY THIS EXISTS.
// Browser sign-in appeared to fail: after a successful OAuth round-trip the
// settings panel still offered "Sign in with Cognitum", and only a hard reload
// showed the truth. The server was correct throughout — `/oauth/status` fell
// through to the service worker's cache-first catch-all, so the first
// (signed-out) response was stored in the Cache API and replayed forever.
//
// The Cache API is not the HTTP cache. It ignores `Cache-Control` entirely, so
// the `no-store` the server already sent could not prevent this. Nothing in the
// Rust suite, the UI unit tests, or a curl probe could observe it: curl has no
// service worker, and a hard reload bypasses one. It was only visible by
// driving a real browser.
//
// These tests load the REAL `sw.js` with stubbed worker globals and assert on
// which strategy each path is routed to.
//
// Run: node --test ui/sw.test.mjs ui/services/ws-ticket.test.mjs
// (a directory argument does not work — Node resolves it as a module)

import { test, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const SW_SOURCE = readFileSync(fileURLToPath(new URL('./sw.js', import.meta.url)), 'utf8');
const ORIGIN = 'http://127.0.0.1:8099';

/** Load sw.js in a fresh context and return its registered `fetch` listener. */
function loadServiceWorker() {
  const listeners = {};
  const cachePuts = [];

  const cacheStub = {
    addAll: async () => {},
    put: async (req, res) => { cachePuts.push(String(req.url ?? req)); return undefined; },
    keys: async () => [],
    match: async () => undefined,
  };

  const sandbox = {
    self: {
      addEventListener: (name, fn) => { listeners[name] = fn; },
      location: { origin: ORIGIN },
      skipWaiting: () => {},
      clients: { claim: () => {} },
    },
    caches: {
      open: async () => cacheStub,
      keys: async () => [],
      delete: async () => true,
      match: async () => undefined,
    },
    // Never actually reached: every assertion below inspects the routing
    // decision, not the network.
    fetch: async () => ({ ok: true, clone: () => ({}) }),
    URL,
    Response: class { constructor(body, init) { this.body = body; Object.assign(this, init); } },
    console,
  };
  vm.createContext(sandbox);
  vm.runInContext(SW_SOURCE, sandbox);

  return { listeners, cachePuts, sandbox };
}

/**
 * Route one request and report the decision.
 * `handled: false` means the SW called neither respondWith nor cache — the
 * request goes to the network untouched, which is the only safe outcome for a
 * credentialed endpoint.
 */
function route(path, { method = 'GET', mode = 'cors', headers = {} } = {}) {
  const { listeners } = loadServiceWorker();
  let handled = false;
  const request = {
    url: `${ORIGIN}${path}`,
    method,
    mode,
    headers: { get: (k) => headers[k] ?? headers[k.toLowerCase()] ?? null },
  };
  listeners.fetch({ request, respondWith: () => { handled = true; } });
  return handled;
}

let sw;
beforeEach(() => { sw = loadServiceWorker(); });

// --- the defect this file exists for ----------------------------------------

test('/oauth/status is never handled by the service worker', () => {
  // The exact request whose cached signed-out copy made sign-in look broken.
  assert.equal(route('/oauth/status'), false);
});

test('every /oauth/ path bypasses the worker, not just status', () => {
  // start/callback/logout all carry or clear credentials. A cached redirect or
  // Set-Cookie replayed later is worse than a cached status.
  for (const p of ['/oauth/start', '/oauth/callback?code=x&state=y', '/oauth/logout']) {
    assert.equal(route(p), false, `${p} must go straight to the network`);
  }
});

// --- the underlying defect: cache-first was the catch-all -------------------

test('an unrecognised path is left to the network rather than cached', () => {
  // This is what made the OAuth bug possible in the first place: any endpoint
  // added outside /api/ was silently frozen on its first response. An
  // allowlist means the next such endpoint is safe by default.
  assert.equal(route('/some/future/endpoint'), false);
});

test('static assets are still served cache-first', () => {
  // The offline shell is the point of the worker; the fix must not disable it.
  for (const p of ['/app.js', '/style.css', '/components/TabManager.js', '/icons/logo.svg']) {
    assert.equal(route(p), true, `${p} should be cache-first`);
  }
});

test('a navigation request is still served cache-first', () => {
  assert.equal(route('/ui/', { mode: 'navigate' }), true);
});

test('API paths are still handled, so offline fallback survives', () => {
  assert.equal(route('/api/v1/models'), true);
  assert.equal(route('/health/live'), true);
});

// --- pre-existing guards, pinned so the rewrite did not drop them -----------

test('non-GET requests are ignored', () => {
  assert.equal(route('/api/v1/models', { method: 'POST' }), false);
});

test('websocket upgrades are ignored', () => {
  assert.equal(route('/ws/sensing', { headers: { Upgrade: 'websocket' } }), false);
});

test('cross-origin requests are ignored', () => {
  const { listeners } = loadServiceWorker();
  let handled = false;
  listeners.fetch({
    request: {
      url: 'https://auth.cognitum.one/oauth/authorize',
      method: 'GET',
      mode: 'cors',
      headers: { get: () => null },
    },
    respondWith: () => { handled = true; },
  });
  assert.equal(handled, false);
});

// --- cache hygiene -----------------------------------------------------------

test('the cache name is bumped so clients holding the poisoned v1 evict it', () => {
  // `activate` deletes every cache whose name !== CACHE_NAME. Browsers that
  // already ran the old worker hold a signed-out /oauth/status in `ruview-v1`;
  // only a name change removes it for them.
  assert.ok(!/['"]ruview-v1['"]/.test(SW_SOURCE), 'CACHE_NAME must not still be ruview-v1');
  assert.ok(/CACHE_NAME\s*=\s*['"]ruview-v[2-9]/.test(SW_SOURCE));
});

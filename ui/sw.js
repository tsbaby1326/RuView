// RuView Service Worker - Offline caching for the dashboard shell
// Strategy: Network-first for API calls, Cache-first for static assets

// Bumped from v1: an older SW cached `/oauth/status` cache-first, so browsers
// that ran it hold a permanently signed-out answer. `activate` deletes every
// cache whose name is not CACHE_NAME, so bumping is what evicts it from clients
// already in the field. Bump again if a future change poisons the cache.
const CACHE_NAME = 'ruview-v2';

// Requests whose response depends on the caller's credentials. These must never
// be served from the Cache API.
//
// The Cache API is NOT the HTTP cache: it ignores `Cache-Control` completely, so
// the `no-store, no-cache, must-revalidate` the server already sends on
// `/oauth/status` has no effect here. A cached signed-out response was returned
// to the page forever, and only a hard reload — which bypasses the service
// worker entirely — showed the true state. (ADR-271.)
const NEVER_CACHE_PREFIXES = ['/oauth/'];

// What may be served cache-first. Previously cache-first was the *catch-all* for
// everything outside `/api/` and `/health/`, which meant any endpoint added at a
// new path was frozen on first response. An allowlist fails safe instead: an
// unrecognised path goes to the network untouched.
const STATIC_ASSET = /\.(?:js|mjs|css|html|json|png|jpe?g|gif|svg|ico|webp|woff2?|ttf|map)$/i;
const SHELL_ASSETS = [
  '/',
  '/index.html',
  '/style.css',
  '/app.js',
  '/config/api.config.js',
  '/components/TabManager.js',
  '/components/DashboardTab.js',
  '/components/HardwareTab.js',
  '/components/LiveDemoTab.js',
  '/components/SensingTab.js',
  '/components/PoseDetectionCanvas.js',
  '/services/api.service.js',
  '/services/websocket.service.js',
  '/services/health.service.js',
  '/services/sensing.service.js',
  '/services/pose.service.js',
  '/services/stream.service.js',
  '/utils/backend-detector.js',
  '/utils/keyboard-shortcuts.js',
  '/utils/perf-monitor.js',
  '/utils/toast.js',
  '/utils/theme-toggle.js',
  '/utils/command-palette.js',
  '/utils/activity-log.js',
  '/utils/data-export.js',
  '/utils/fullscreen.js',
  '/utils/connection-status.js',
  '/utils/mobile-nav.js'
];

// Install - cache shell assets
self.addEventListener('install', (event) => {
  event.waitUntil(
    caches.open(CACHE_NAME).then((cache) => {
      return cache.addAll(SHELL_ASSETS).catch((err) => {
        // Don't fail install if some assets are missing (dev mode)
        console.warn('[SW] Some assets failed to cache:', err);
      });
    })
  );
  self.skipWaiting();
});

// Activate - clean old caches
self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches.keys().then((keys) => {
      return Promise.all(
        keys
          .filter((key) => key !== CACHE_NAME)
          .map((key) => caches.delete(key))
      );
    })
  );
  self.clients.claim();
});

// Fetch - network-first for API, cache-first for static
self.addEventListener('fetch', (event) => {
  const { request } = event;
  const url = new URL(request.url);

  // Skip non-GET requests
  if (request.method !== 'GET') return;

  // Skip WebSocket upgrade requests
  if (request.headers.get('Upgrade') === 'websocket') return;

  // Skip cross-origin requests
  if (url.origin !== self.location.origin) return;

  // Credentialed endpoints: hands off entirely. Not networkFirst — that still
  // writes a copy into the cache, which would be replayed the moment the server
  // is briefly unreachable, silently reinstating a stale sign-in state.
  if (NEVER_CACHE_PREFIXES.some((prefix) => url.pathname.startsWith(prefix))) return;

  // API calls: network-first with cache fallback
  if (url.pathname.startsWith('/api/') || url.pathname.startsWith('/health/')) {
    event.respondWith(networkFirst(request));
    return;
  }

  // Static assets and the app shell: cache-first with network fallback.
  if (request.mode === 'navigate' || STATIC_ASSET.test(url.pathname)) {
    event.respondWith(cacheFirst(request));
  }

  // Anything else is left alone and goes to the network as normal.
});

async function cacheFirst(request) {
  // `ignoreSearch` so the shell is a single entry. Sign-in redirects back to
  // `/ui/?signed_in=<ms>`, which would otherwise mint a fresh cache entry per
  // sign-in and never hit any of them again.
  const cached = await caches.match(request, { ignoreSearch: true });
  if (cached) return cached;

  try {
    const response = await fetch(request);
    if (response.ok) {
      const cache = await caches.open(CACHE_NAME);
      // Store under the search-less URL to match how it is looked up above.
      const key = new URL(request.url);
      key.search = '';
      cache.put(key.toString(), response.clone());
    }
    return response;
  } catch {
    // Return offline fallback for HTML navigation
    if (request.headers.get('Accept')?.includes('text/html')) {
      const fallback = await caches.match('/index.html');
      if (fallback) return fallback;
    }
    return new Response('Offline', { status: 503, statusText: 'Service Unavailable' });
  }
}

async function networkFirst(request) {
  try {
    const response = await fetch(request);
    if (response.ok) {
      const cache = await caches.open(CACHE_NAME);
      cache.put(request, response.clone());
    }
    return response;
  } catch {
    const cached = await caches.match(request);
    if (cached) return cached;
    return new Response(JSON.stringify({ error: 'offline' }), {
      status: 503,
      headers: { 'Content-Type': 'application/json' }
    });
  }
}

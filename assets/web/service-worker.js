const CACHE_NAME = "agent-console-shell-v6";

const SHELL_ASSETS = [
  "/",
  "/index.html",
  "/app.css",
  "/app.js",
  "/css/dashboard.css",
  "/css/overview.css",
  "/css/conversation.css",
  "/css/terminal.css",
  "/css/dialogs.css",
  "/css/alerts.css",
  "/css/doctor.css",
  "/js/api.js",
  "/js/clipboard.js",
  "/js/dom.js",
  "/js/lease.js",
  "/js/markdown.js",
  "/js/notifications.js",
  "/js/promptWatch.js",
  "/js/router.js",
  "/js/store.js",
  "/js/dialogs/newSession.js",
  "/js/dialogs/rename.js",
  "/js/dialogs/token.js",
  "/js/views/alerts.js",
  "/js/views/banners.js",
  "/js/views/conversation.js",
  "/js/views/dashboard.js",
  "/js/views/doctor.js",
  "/js/views/message.js",
  "/js/views/overview.js",
  "/js/views/shell.js",
  "/js/views/terminal.js",
  "/js/views/termview.js",
  "/manifest.webmanifest",
  "/vendor/xterm.js",
  "/vendor/xterm.css",
  "/vendor/xterm-addon-fit.js",
  "/icons/icon-192.png",
  "/icons/icon-512.png",
];

// Long-lived third-party bundles and icons are safe to serve straight from cache;
// everything else is the app shell, which must follow the running binary.
const CACHE_FIRST = /^\/(?:vendor|icons)\//;

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE_NAME)
      .then((cache) => cache.addAll(SHELL_ASSETS))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((key) => key !== CACHE_NAME).map((key) => caches.delete(key))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);

  // The control plane is always network-only: never cache API calls or websocket
  // upgrades (the latter never reach here anyway, but this keeps the intent explicit).
  if (url.pathname.startsWith("/api/") || url.pathname.startsWith("/ws/")) return;
  if (event.request.method !== "GET" || url.origin !== self.location.origin) return;

  if (CACHE_FIRST.test(url.pathname)) {
    event.respondWith(cacheFirst(event.request));
    return;
  }
  event.respondWith(networkFirst(event.request));
});

function cacheFirst(request) {
  return caches.match(request).then((cached) => cached || fetchAndStore(request));
}

/**
 * Shell files keep their URLs across upgrades, so a cache-first shell would pin
 * the UI to whatever version was installed first. Going to the network first and
 * falling back to the cache keeps the PWA usable offline without that trap.
 */
function networkFirst(request) {
  return fetchAndStore(request).catch(() =>
    caches.match(request).then((cached) => cached || caches.match("/index.html")),
  );
}

function fetchAndStore(request) {
  return fetch(request).then((response) => {
    if (response && response.ok) {
      const copy = response.clone();
      caches.open(CACHE_NAME).then((cache) => cache.put(request, copy));
    }
    return response;
  });
}

/**
 * Opening the session an alert came from.
 *
 * Notifications raised through the registration outlive the page, so the click has to be
 * handled here: focus an open window and move it to the session, or open one at that hash.
 */
self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const hash = (event.notification.data && event.notification.data.hash) || "#/";
  event.waitUntil(
    self.clients.matchAll({ type: "window", includeUncontrolled: true }).then((clients) => {
      for (const client of clients) {
        if (new URL(client.url).origin !== self.location.origin) continue;
        return client.focus().then(() => client.navigate(`/${hash}`).catch(() => client));
      }
      return self.clients.openWindow(`/${hash}`);
    }),
  );
});

// The service worker: the shell, kept, and nothing else.
//
// What it caches is the app itself -- the HTML, the stylesheet, the modules,
// the icons -- so opening evo from the home screen does not wait for the
// network to draw something. It never caches an answer from `/api/`: a library
// listing or a page image out of a stale cache would be evo telling a lie
// about what is in the library, and the library is the whole point.
//
// A phone that is offline gets the shell and a sentence saying why nothing is
// in it. Everything here is an improvement on being slow; nothing here is
// required for the app to work.

const CACHE = "evo-shell-v2";

const SHELL = [
  "/",
  "/index.html",
  "/style.css",
  "/api.js",
  "/app.js",
  "/viewer.js",
  "/chat.js",
  "/offline.html",
  "/manifest.webmanifest",
  "/icons/icon-192.png",
  "/icons/icon-512.png",
  "/icons/apple-touch-icon.png",
];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE)
      .then((cache) => cache.addAll(SHELL))
      // A shell that would not cache is not a reason to refuse to install.
      .catch(() => {})
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((names) =>
        Promise.all(names.filter((name) => name !== CACHE).map((name) => caches.delete(name))),
      )
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") return;

  const url = new URL(request.url);
  // Someone else's server is not ours to cache or to answer for.
  if (url.origin !== self.location.origin) return;
  // The library, always live.
  if (url.pathname.startsWith("/api/")) return;

  // Opening the app: the network first, so a new version is picked up as soon
  // as there is a network to pick it up from.
  if (request.mode === "navigate") {
    event.respondWith(
      fetch(request)
        .then((response) => {
          keep(request, response.clone());
          return response;
        })
        .catch(async () => {
          const cache = await caches.open(CACHE);
          return (
            (await cache.match("/index.html")) ||
            (await cache.match("/offline.html")) ||
            new Response("evo is offline.", {
              status: 503,
              headers: { "content-type": "text/plain" },
            })
          );
        }),
    );
    return;
  }

  // The rest of the shell: whatever was kept, checked against the network in
  // the background so the next start has the new one.
  event.respondWith(
    caches.match(request).then((cached) => {
      const live = fetch(request)
        .then((response) => {
          keep(request, response.clone());
          return response;
        })
        .catch(() => cached);
      return cached || live;
    }),
  );
});

function keep(request, response) {
  if (!response || !response.ok) return;
  caches.open(CACHE).then((cache) => cache.put(request, response));
}

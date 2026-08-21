const TAAROF_STATIC_CACHE = "taarof-static-v2";

function hasTokenQuery(request) {
  try {
    return new URL(request.url).searchParams.has("token");
  } catch (_error) {
    return false;
  }
}

async function purgeTokenBearingRequests(cacheName) {
  const cache = await caches.open(cacheName);
  const requests = await cache.keys();
  await Promise.all(
    requests
      .filter((request) => hasTokenQuery(request))
      .map((request) => cache.delete(request)),
  );
}

self.addEventListener("install", (event) => {
  event.waitUntil(self.skipWaiting());
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      const cacheKeys = await caches.keys();
      await Promise.all(
        cacheKeys
          .filter((cacheKey) => cacheKey !== TAAROF_STATIC_CACHE)
          .map((cacheKey) => caches.delete(cacheKey)),
      );
      await purgeTokenBearingRequests(TAAROF_STATIC_CACHE);
      await self.clients.claim();
    })(),
  );
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") {
    return;
  }

  const url = new URL(request.url);
  if (url.origin !== self.location.origin) {
    return;
  }

  if (url.pathname.startsWith("/api/") || url.pathname === "/health") {
    return;
  }

  if (url.searchParams.has("token")) {
    event.respondWith(fetch(request, { cache: "no-store" }));
    return;
  }

  if (url.search) {
    return;
  }

  event.respondWith(
    (async () => {
      const cache = await caches.open(TAAROF_STATIC_CACHE);

      try {
        const response = await fetch(request);
        if (response.ok) {
          cache.put(request, response.clone()).catch(() => {});
        }
        return response;
      } catch (error) {
        const cached = await cache.match(request);
        if (cached) {
          return cached;
        }
        throw error;
      }
    })(),
  );
});

const TOKEN_STORAGE_KEY = "taarof.web.token";

export function getStoredToken(): string | null {
  if (typeof window === "undefined") {
    return null;
  }
  return window.localStorage.getItem(TOKEN_STORAGE_KEY);
}

export function storeToken(token: string): void {
  if (typeof window === "undefined") {
    return;
  }
  window.localStorage.setItem(TOKEN_STORAGE_KEY, token);
  void purgeTokenBearingCacheEntries().catch((error: unknown) => {
    console.warn("taarof web: token cache cleanup failed", error);
  });
}

async function purgeTokenBearingCacheEntries(): Promise<void> {
  if (typeof window === "undefined" || !("caches" in window)) {
    return;
  }

  const cacheNames = await window.caches.keys();
  await Promise.all(
    cacheNames.map(async (cacheName) => {
      const cache = await window.caches.open(cacheName);
      const requests = await cache.keys();
      await Promise.all(
        requests
          .filter((request) => {
            try {
              return new URL(request.url).searchParams.has("token");
            } catch {
              return false;
            }
          })
          .map((request) => cache.delete(request)),
      );
    }),
  );
}

export function clearStoredToken(): void {
  if (typeof window === "undefined") {
    return;
  }
  window.localStorage.removeItem(TOKEN_STORAGE_KEY);
  void purgeTokenBearingCacheEntries().catch((error: unknown) => {
    console.warn("taarof web: token cache cleanup failed", error);
  });
}

export function bootstrapTokenFromUrl(): string | null {
  if (typeof window === "undefined") {
    return null;
  }

  const url = new URL(window.location.href);
  const token = url.searchParams.get("token");
  if (!token) {
    return null;
  }

  storeToken(token);
  url.searchParams.delete("token");

  const nextUrl = `${url.pathname}${url.search}${url.hash}` || "/";
  window.history.replaceState({}, document.title, nextUrl);
  return token;
}

import { browserLocalStorage } from "./browserStorage.js";

const TOKEN_STORAGE_KEY = "taarof.web.token";

export type TokenStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

export interface BootstrappedToken {
  token: string;
  /** False when browser storage refused the token; it lives only in memory. */
  persisted: boolean;
}

interface UrlTokenEnvironment {
  href: string;
  storage: TokenStorage | null;
  replaceUrl: (nextUrl: string) => void;
}

// Every storage call below can throw (quota, privacy mode), so each access
// degrades to absence or failure. Caught errors are never logged: their
// messages may echo the stored value.
function browserTokenStorage(): TokenStorage | null {
  return browserLocalStorage();
}

export function getStoredToken(storage = browserTokenStorage()): string | null {
  if (!storage) {
    return null;
  }
  try {
    return storage.getItem(TOKEN_STORAGE_KEY);
  } catch {
    return null;
  }
}

/** Returns whether the token was persisted for later page loads. */
export function storeToken(token: string, storage = browserTokenStorage()): boolean {
  void schedulePurgeTokenBearingCacheEntries();
  if (!storage) {
    return false;
  }
  try {
    storage.setItem(TOKEN_STORAGE_KEY, token);
    return true;
  } catch {
    return false;
  }
}

function browserCacheStorage(): CacheStorage | null {
  try {
    if (typeof window === "undefined" || !("caches" in window)) {
      return null;
    }
    return window.caches;
  } catch {
    // Access itself may be denied; exception details can contain a token URL.
    return null;
  }
}

/** Resolves once cleanup settles; failures are reported without their details. */
export function schedulePurgeTokenBearingCacheEntries(
  caches = browserCacheStorage(),
): Promise<void> {
  // The rejection may carry a token-bearing request URL, so it is never logged.
  return purgeTokenBearingCacheEntries(caches).catch(() => {
    console.warn("taarof web: token cache cleanup failed");
  });
}

async function purgeTokenBearingCacheEntries(caches: CacheStorage | null): Promise<void> {
  if (!caches) {
    return;
  }

  const cacheNames = await caches.keys();
  await Promise.all(
    cacheNames.map(async (cacheName) => {
      const cache = await caches.open(cacheName);
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

export function clearStoredToken(storage = browserTokenStorage()): void {
  void schedulePurgeTokenBearingCacheEntries();
  if (!storage) {
    return;
  }
  try {
    storage.removeItem(TOKEN_STORAGE_KEY);
  } catch {
    // Nothing persisted can be removed; in-memory auth state is cleared by the caller.
  }
}

function browserUrlTokenEnvironment(): UrlTokenEnvironment | null {
  if (typeof window === "undefined") {
    return null;
  }
  return {
    href: window.location.href,
    storage: browserTokenStorage(),
    replaceUrl: (nextUrl) => window.history.replaceState({}, document.title, nextUrl),
  };
}

export function bootstrapTokenFromUrl(
  environment = browserUrlTokenEnvironment(),
): BootstrappedToken | null {
  if (!environment) {
    return null;
  }

  const url = new URL(environment.href);
  if (!url.searchParams.has("token")) {
    return null;
  }
  // Presence decides cleanup; a usable value decides authentication.
  const token = url.searchParams.getAll("token").find((value) => value !== "") ?? null;

  let persisted = false;
  try {
    if (token) {
      persisted = storeToken(token, environment.storage);
    }
  } finally {
    url.searchParams.delete("token");
    const nextUrl = `${url.pathname}${url.search}${url.hash}` || "/";
    environment.replaceUrl(nextUrl);
  }
  return token ? { token, persisted } : null;
}

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

// Browser storage is optional persistence. Reading `window.localStorage` can
// itself throw (denied by site settings), and every call below can throw
// (quota, privacy mode), so each access degrades to absence or failure.
// Caught errors are never logged: their messages may echo the stored value.
function browserTokenStorage(): TokenStorage | null {
  if (typeof window === "undefined") {
    return null;
  }
  try {
    return window.localStorage;
  } catch {
    return null;
  }
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
  schedulePurgeTokenBearingCacheEntries();
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

function schedulePurgeTokenBearingCacheEntries() {
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

export function clearStoredToken(storage = browserTokenStorage()): void {
  schedulePurgeTokenBearingCacheEntries();
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
  const token = url.searchParams.get("token");
  if (!token) {
    return null;
  }

  let persisted = false;
  try {
    persisted = storeToken(token, environment.storage);
  } finally {
    url.searchParams.delete("token");
    const nextUrl = `${url.pathname}${url.search}${url.hash}` || "/";
    environment.replaceUrl(nextUrl);
  }
  return { token, persisted };
}

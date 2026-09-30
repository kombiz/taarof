export type EnumerableStorage = Pick<
  Storage,
  "getItem" | "setItem" | "removeItem" | "key" | "length"
>;

// Browser storage is optional persistence. Reading `window.localStorage` can
// itself throw (denied by site settings), so every caller resolves it here.
export function browserLocalStorage(): Storage | null {
  if (typeof window === "undefined") {
    return null;
  }
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

// Stands in for denied storage where callers need an object: reads find
// nothing and writes are refused like a full store, so callers' existing
// failure handling applies and state lives only for this page load.
// A single shared instance keeps hook dependencies stable across renders.
const unavailableStorage: EnumerableStorage = {
  length: 0,
  key: () => null,
  getItem: () => null,
  setItem: () => {
    throw new Error("browser storage is unavailable");
  },
  removeItem: () => {},
};

export function optionalLocalStorage(): EnumerableStorage {
  return browserLocalStorage() ?? unavailableStorage;
}

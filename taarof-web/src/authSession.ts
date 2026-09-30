import {
  bootstrapTokenFromUrl,
  clearStoredToken,
  getStoredToken,
  storeToken,
  type BootstrappedToken,
  type TokenStorage,
} from "./auth.js";

// The App's authentication transitions, kept outside React so they can be
// exercised against failing storage. Storage is optional persistence: the
// in-memory token is authoritative for this page load either way. An omitted
// `storage` argument means browser localStorage.
export interface AuthStateTarget {
  /** Stops event recovery and invalidates in-flight refreshes. */
  stopConnections(): void;
  setToken(token: string | null): void;
  setTokenPersisted(persisted: boolean): void;
  setTokenError(message: string | null): void;
  /** Drops snapshot, agent sessions, selection, and load state. */
  clearAuthenticatedState(): void;
}

export function resolveInitialAuth(
  bootstrap: () => BootstrappedToken | null = () => bootstrapTokenFromUrl(),
  storage?: TokenStorage | null,
): BootstrappedToken | null {
  const bootstrapped = bootstrap();
  if (bootstrapped) return bootstrapped;
  const stored = getStoredToken(storage);
  return stored ? { token: stored, persisted: true } : null;
}

export function submitToken(
  target: AuthStateTarget,
  nextToken: string,
  storage?: TokenStorage | null,
): void {
  if (!nextToken) {
    target.setTokenError("A bearer token is required.");
    return;
  }

  target.setTokenPersisted(storeToken(nextToken, storage));
  target.stopConnections();
  target.setToken(nextToken);
  target.setTokenError(null);
}

/** Shared by explicit reset (no message) and unauthorized responses. */
export function signOut(
  target: AuthStateTarget,
  errorMessage: string | null,
  storage?: TokenStorage | null,
): void {
  target.stopConnections();
  clearStoredToken(storage);
  target.clearAuthenticatedState();
  target.setToken(null);
  target.setTokenError(errorMessage);
}

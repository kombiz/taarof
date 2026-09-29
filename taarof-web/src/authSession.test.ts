import type { TokenStorage } from "./auth.js";
import { resolveInitialAuth, signOut, submitToken, type AuthStateTarget } from "./authSession.js";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function test(name: string, fn: () => void) {
  try {
    fn();
    console.log(`PASS ${name}`);
  } catch (error) {
    console.error(`FAIL ${name}`);
    throw error;
  }
}

const TOKEN = "secret-token-value-9c21";

class ThrowingTokenStorage implements TokenStorage {
  getItem(): string | null {
    throw new Error(`storage denied while reading ${TOKEN}`);
  }

  setItem(_key: string, value: string): void {
    throw new Error(`quota exceeded while writing ${value}`);
  }

  removeItem(): void {
    throw new Error(`storage denied while removing ${TOKEN}`);
  }
}

class MemoryTokenStorage implements TokenStorage {
  readonly values = new Map<string, string>();

  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }

  setItem(key: string, value: string): void {
    this.values.set(key, value);
  }

  removeItem(key: string): void {
    this.values.delete(key);
  }
}

// Mirrors the App state the transitions drive: token, persistence notice,
// error text, and the authenticated data (snapshot and agent sessions).
function fakeApp() {
  const state = {
    token: null as string | null,
    persisted: true,
    tokenError: null as string | null,
    snapshot: "snapshot" as string | null,
    agentSessions: "agent-sessions" as string | null,
    stopCount: 0,
  };
  const target: AuthStateTarget = {
    stopConnections: () => {
      state.stopCount += 1;
    },
    setToken: (token) => {
      state.token = token;
    },
    setTokenPersisted: (persisted) => {
      state.persisted = persisted;
    },
    setTokenError: (message) => {
      state.tokenError = message;
    },
    clearAuthenticatedState: () => {
      state.snapshot = null;
      state.agentSessions = null;
    },
  };
  return { state, target };
}

function captureConsole(fn: () => void): string {
  const lines: string[] = [];
  const original = { log: console.log, warn: console.warn, error: console.error };
  const record = (...args: unknown[]) => {
    lines.push(args.map((arg) => (arg instanceof Error ? `${arg.message} ${arg.stack}` : String(arg))).join(" "));
  };
  console.log = record;
  console.warn = record;
  console.error = record;
  try {
    fn();
  } finally {
    Object.assign(console, original);
  }
  return lines.join("\n");
}

test("submitted token authenticates in memory when storage refuses it", () => {
  const { state, target } = fakeApp();
  const output = captureConsole(() => submitToken(target, TOKEN, new ThrowingTokenStorage()));

  assert(state.token === TOKEN, "the submitted token is the active token");
  assert(state.persisted === false, "the UI is told the token was not saved");
  assert(state.tokenError === null, "no error is shown for a failed save");
  assert(state.stopCount === 1, "previous connections are stopped");
  assert(!output.includes(TOKEN), "the token must not appear in console output");
});

test("submitted token is persisted with working storage", () => {
  const { state, target } = fakeApp();
  const storage = new MemoryTokenStorage();
  submitToken(target, TOKEN, storage);

  assert(state.token === TOKEN, "the submitted token is the active token");
  assert(state.persisted === true, "the token is reported as saved");
  assert(storage.values.size === 1, "the token is written to storage");
});

test("empty submission reports an error and leaves auth unchanged", () => {
  const { state, target } = fakeApp();
  submitToken(target, "", new ThrowingTokenStorage());

  assert(state.token === null, "no token is set");
  assert(state.tokenError === "A bearer token is required.", "the empty token is rejected");
});

test("reset clears in-memory auth state when storage removal throws", () => {
  const { state, target } = fakeApp();
  const storage = new ThrowingTokenStorage();
  submitToken(target, TOKEN, storage);

  const output = captureConsole(() => signOut(target, null, storage));

  assert(state.token === null, "the token is cleared");
  assert(state.snapshot === null, "the snapshot is cleared");
  assert(state.agentSessions === null, "agent sessions are cleared");
  assert(state.tokenError === null, "reset shows no error");
  assert(!output.includes(TOKEN), "the token must not appear in console output");
});

test("unauthorized clears in-memory auth state when storage removal throws", () => {
  const { state, target } = fakeApp();
  const storage = new ThrowingTokenStorage();
  submitToken(target, TOKEN, storage);

  const output = captureConsole(() => signOut(target, "The taarof token was rejected.", storage));

  assert(state.token === null, "the token is cleared");
  assert(state.snapshot === null, "the snapshot is cleared");
  assert(state.agentSessions === null, "agent sessions are cleared");
  assert(state.tokenError === "The taarof token was rejected.", "the rejection is shown");
  assert(!state.tokenError.includes(TOKEN), "the error message must not carry the token");
  assert(!output.includes(TOKEN), "the token must not appear in console output");
});

test("sign-out removes a persisted token from working storage", () => {
  const { state, target } = fakeApp();
  const storage = new MemoryTokenStorage();
  submitToken(target, TOKEN, storage);
  signOut(target, null, storage);

  assert(state.token === null, "the token is cleared");
  assert(storage.values.size === 0, "the persisted token is removed");
});

test("initial auth prefers the URL token, then storage, and survives throwing storage", () => {
  const fromUrl = resolveInitialAuth(() => ({ token: TOKEN, persisted: false }), new ThrowingTokenStorage());
  assert(fromUrl?.token === TOKEN && fromUrl.persisted === false, "the URL token wins");

  const storage = new MemoryTokenStorage();
  storage.setItem("taarof.web.token", TOKEN);
  const fromStorage = resolveInitialAuth(() => null, storage);
  assert(fromStorage?.token === TOKEN && fromStorage.persisted === true, "the stored token is used");

  assert(resolveInitialAuth(() => null, new ThrowingTokenStorage()) === null, "throwing storage reads as signed out");
});

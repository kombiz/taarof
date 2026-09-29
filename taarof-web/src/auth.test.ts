import {
  bootstrapTokenFromUrl,
  clearStoredToken,
  getStoredToken,
  schedulePurgeTokenBearingCacheEntries,
  storeToken,
  type TokenStorage,
} from "./auth.js";

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

const TOKEN = "secret-token-value-7f3a";

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

class ThrowingTokenStorage implements TokenStorage {
  getItem(): string | null {
    throw new Error(`storage denied while reading ${TOKEN}`);
  }

  setItem(_key: string, value: string): void {
    throw new Error(`quota exceeded while writing ${value}`);
  }

  removeItem(): void {
    throw new Error("storage denied while removing");
  }
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

function urlEnvironment(href: string, storage: TokenStorage | null) {
  const replaced: string[] = [];
  return {
    replaced,
    environment: {
      href,
      storage,
      replaceUrl: (nextUrl: string) => {
        replaced.push(nextUrl);
      },
    },
  };
}

test("normal storage persists, reads, and clears the token", () => {
  const storage = new MemoryTokenStorage();

  assert(storeToken(TOKEN, storage), "a working store reports the token as persisted");
  assert(getStoredToken(storage) === TOKEN, "the persisted token is read back");
  clearStoredToken(storage);
  assert(getStoredToken(storage) === null, "clearing removes the persisted token");
});

test("normal storage bootstrap persists the URL token and strips it", () => {
  const storage = new MemoryTokenStorage();
  const { replaced, environment } = urlEnvironment(
    `http://127.0.0.1:7777/app?token=${TOKEN}&view=monitor#pane`,
    storage,
  );

  const result = bootstrapTokenFromUrl(environment);

  assert(result?.token === TOKEN, "the URL token is returned");
  assert(result?.persisted === true, "the URL token is persisted");
  assert(getStoredToken(storage) === TOKEN, "the URL token lands in storage");
  assert(replaced.length === 1, "the URL is rewritten once");
  assert(replaced[0] === "/app?view=monitor#pane", `unexpected rewritten URL ${replaced[0]}`);
});

test("bootstrap without a URL token leaves the URL alone", () => {
  const { replaced, environment } = urlEnvironment(
    "http://127.0.0.1:7777/?view=monitor",
    new MemoryTokenStorage(),
  );

  assert(bootstrapTokenFromUrl(environment) === null, "no token yields null");
  assert(replaced.length === 0, "a token-free URL is not rewritten");
});

test("throwing storage never throws and never logs the token", () => {
  const storage = new ThrowingTokenStorage();
  const results: { stored?: boolean; read?: string | null } = {};

  const output = captureConsole(() => {
    results.stored = storeToken(TOKEN, storage);
    results.read = getStoredToken(storage);
    clearStoredToken(storage);
  });
  const { stored, read } = results;

  assert(stored === false, "a failed write reports the token as not persisted");
  assert(read === null, "a failed read reports absence");
  assert(!output.includes(TOKEN), "the token must not appear in console output");
});

test("throwing storage bootstrap still strips the URL token and keeps it in memory", () => {
  const { replaced, environment } = urlEnvironment(
    `http://127.0.0.1:7777/?token=${TOKEN}`,
    new ThrowingTokenStorage(),
  );
  const results: { bootstrapped?: ReturnType<typeof bootstrapTokenFromUrl> } = {};

  const output = captureConsole(() => {
    results.bootstrapped = bootstrapTokenFromUrl(environment);
  });
  const { bootstrapped } = results;
  assert(bootstrapped?.token === TOKEN, "the URL token is still returned for in-memory use");
  assert(bootstrapped?.persisted === false, "the failed write is reported");
  assert(replaced.length === 1 && replaced[0] === "/", `URL token was not stripped: ${replaced}`);
  assert(!replaced[0].includes(TOKEN), "the rewritten URL must not carry the token");
  assert(!output.includes(TOKEN), "the token must not appear in console output");
});

test("unavailable storage behaves like empty optional persistence", () => {
  const { replaced, environment } = urlEnvironment(`http://127.0.0.1:7777/?token=${TOKEN}`, null);

  assert(getStoredToken(null) === null, "missing storage reads as absent");
  assert(storeToken(TOKEN, null) === false, "missing storage cannot persist");
  clearStoredToken(null);
  const result = bootstrapTokenFromUrl(environment);
  assert(result?.token === TOKEN && result.persisted === false, "missing storage keeps the token in memory");
  assert(replaced[0] === "/", "missing storage still strips the URL token");
});

test("an empty first token parameter still strips every token parameter", () => {
  const { replaced, environment } = urlEnvironment(
    `http://127.0.0.1:7777/app?token=&view=monitor&token=${TOKEN}`,
    new MemoryTokenStorage(),
  );

  const result = bootstrapTokenFromUrl(environment);

  assert(result?.token === TOKEN, "the first non-empty token value is used");
  assert(replaced.length === 1, "the URL is rewritten once");
  assert(replaced[0] === "/app?view=monitor", `every token parameter must be stripped: ${replaced[0]}`);
});

test("only empty token parameters are stripped without authenticating", () => {
  const { replaced, environment } = urlEnvironment(
    "http://127.0.0.1:7777/?token=&token=",
    new MemoryTokenStorage(),
  );

  assert(bootstrapTokenFromUrl(environment) === null, "empty token values yield no token");
  assert(replaced.length === 1 && replaced[0] === "/", `empty token parameters must be stripped: ${replaced}`);
});

async function captureConsoleAsync(fn: () => Promise<void>): Promise<string> {
  const lines: string[] = [];
  const original = { log: console.log, warn: console.warn, error: console.error };
  const record = (...args: unknown[]) => {
    lines.push(args.map((arg) => (arg instanceof Error ? `${arg.message} ${arg.stack}` : String(arg))).join(" "));
  };
  console.log = record;
  console.warn = record;
  console.error = record;
  try {
    await fn();
  } finally {
    Object.assign(console, original);
  }
  return lines.join("\n");
}

async function asyncTest(name: string, fn: () => Promise<void>) {
  try {
    await fn();
    console.log(`PASS ${name}`);
  } catch (error) {
    console.error(`FAIL ${name}`);
    throw error;
  }
}

await asyncTest("a rejected cache cleanup logs a fixed message without the token", async () => {
  const tokenUrl = `http://127.0.0.1:7777/?token=${TOKEN}`;
  const rejectingCaches = {
    keys: async () => ["taarof"],
    open: async () => ({
      keys: async () => [{ url: tokenUrl }],
      delete: async () => {
        throw new Error(`failed to delete ${tokenUrl}`);
      },
    }),
  } as unknown as Parameters<typeof schedulePurgeTokenBearingCacheEntries>[0];

  const output = await captureConsoleAsync(() => schedulePurgeTokenBearingCacheEntries(rejectingCaches));

  assert(output.includes("token cache cleanup failed"), `the failure is still reported: ${output}`);
  assert(!output.includes(TOKEN), "the token must not appear in console output");
});

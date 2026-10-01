import { fetchRuntimeIdentity, fetchState, ApiUnauthorizedError, type RuntimeIdentityResponse } from "./api.js";
import { loadMonitorSnapshot, monitorNamespace, type MonitorSnapshot } from "./monitorIdentity.js";
import { buildPaneTargets, MONITOR_ORDER_STORAGE_KEY, MONITOR_WATCH_STORAGE_KEY, readStoredMonitorOrder, readStoredWatchedKeys, writeStoredMonitorOrder, writeStoredWatchedKeys } from "./monitorBoard.js";
import { buildPaneIdentity, loadPaneJournal, type PaneJournalEnumerableStorage } from "./monitorPaneJournal.js";
import { createPaneJournalController } from "./usePaneJournal.js";
import { runGuardedRefresh } from "./refreshScheduler.js";
import type { TaarofStateSnapshot } from "./types.js";

function assert(value: unknown, message: string): asserts value {
  if (!value) throw new Error(message);
}

async function test(name: string, fn: () => void | Promise<void>) {
  await fn();
  console.log(`PASS ${name}`);
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

function identity(runtimeId = "host-a-runtime", sessionName = "shared"): RuntimeIdentityResponse {
  return { schema: "test", runtime_id: runtimeId, session_name: sessionName, identity: null };
}

function snapshot(sessionName = "shared"): TaarofStateSnapshot {
  return { schema: "test", generated_at_unix_ms: 0, session_name: sessionName,
    active_workspace: null, active_tab: null, workspaces: [] };
}

function paneIdentity(runtimeId = "host-a-runtime", tabName = "A") {
  return buildPaneIdentity({ runtimeId, sessionName: "shared", workspaceId: 1,
    workspaceName: "Main", tabId: 2, tabName, paneId: 0 });
}

class MemoryStorage implements PaneJournalEnumerableStorage {
  values = new Map<string, string>();
  get length() { return this.values.size; }
  key(index: number) { return [...this.values.keys()][index] ?? null; }
  getItem(key: string) { return this.values.get(key) ?? null; }
  setItem(key: string, value: string) { this.values.set(key, value); }
  removeItem(key: string) { this.values.delete(key); }
}

const scheduler = { setTimeout: () => 1, clearTimeout: () => {} };

function load(overrides: Partial<Parameters<typeof loadMonitorSnapshot>[0]> = {}) {
  return loadMonitorSnapshot({ signal: new AbortController().signal, isCurrent: () => true,
    fetchIdentity: async () => identity(), fetchSnapshot: async () => snapshot(),
    isUnauthorized: (error) => error instanceof ApiUnauthorizedError, ...overrides });
}

await test("two hosts reusing session and pane coordinates isolate watch, order and journal", () => {
  const storage = new MemoryStorage();
  const a = paneIdentity();
  const b = paneIdentity("host-b-runtime", "different pane");
  const namespaceA = monitorNamespace("host-a-runtime", "shared");
  const namespaceB = monitorNamespace("host-b-runtime", "shared");
  writeStoredMonitorOrder(storage, [a.paneKey], namespaceA);
  writeStoredWatchedKeys(storage, [a.paneKey], namespaceA);
  const controller = createPaneJournalController({ storage, identity: a, scheduler });
  controller.observe({ source: "snapshot", text: "host A", seenAtUnixMs: Date.now() });
  controller.flush();
  assert(a.paneKey !== b.paneKey, "runtimes need distinct pane keys");
  assert(readStoredMonitorOrder(storage, namespaceB).length === 0, "B cannot inherit A order");
  assert(readStoredWatchedKeys(storage, namespaceB).length === 0, "B cannot inherit A watch");
  assert(loadPaneJournal(storage, b.paneKey).entries.length === 0, "B cannot inherit A journal");
  writeStoredWatchedKeys(storage, [b.paneKey], namespaceB);
  writeStoredMonitorOrder(storage, [b.paneKey], namespaceB);
  const second = createPaneJournalController({ storage, identity: b, scheduler });
  second.observe({ source: "snapshot", text: "host B", seenAtUnixMs: Date.now() });
  second.flush();
  assert(readStoredWatchedKeys(storage, namespaceA)[0] === a.paneKey, "B write preserves A watch");
  assert(readStoredMonitorOrder(storage, namespaceA)[0] === a.paneKey, "B write preserves A order");
  assert(loadPaneJournal(storage, a.paneKey).entries[0]?.text === "host A", "B write preserves A journal");
});

await test("actual API snapshot verification preserves identity through refresh and token rotation", async () => {
  const originalFetch = globalThis.fetch;
  const authorization: string[] = [];
  globalThis.fetch = async (input, init) => {
    authorization.push(new Headers(init?.headers).get("Authorization") ?? "");
    const data = String(input).endsWith("runtime-identity") ? identity() : snapshot();
    return new Response(JSON.stringify({ ok: true, data }), { status: 200 });
  };
  try {
    const results: MonitorSnapshot[] = [];
    for (const token of ["test-token-before", "test-token-after"]) {
      results.push(await load({ fetchIdentity: () => fetchRuntimeIdentity(token), fetchSnapshot: () => fetchState(token) }));
    }
    assert(authorization.includes("Bearer test-token-before") && authorization.includes("Bearer test-token-after"), "actual requests use rotated authorization");
    assert(results[0].namespace !== null && results[0].namespace === results[1].namespace, "token rotation preserves namespace");
    const storage = new MemoryStorage();
    const pane = paneIdentity();
    writeStoredWatchedKeys(storage, [pane.paneKey], results[0].namespace);
    writeStoredMonitorOrder(storage, [pane.paneKey], results[0].namespace);
    const first = createPaneJournalController({ storage, identity: pane, scheduler });
    first.observe({ source: "snapshot", text: "before refresh", seenAtUnixMs: Date.now() });
    first.flush();
    first.cancel();
    assert(readStoredWatchedKeys(storage, results[1].namespace)[0] === pane.paneKey, "watch survives refresh");
    assert(readStoredMonitorOrder(storage, results[1].namespace)[0] === pane.paneKey, "order survives refresh");
    assert(loadPaneJournal(storage, paneIdentity().paneKey).entries[0]?.text === "before refresh", "journal survives refresh");
    assert([...storage.values.keys()].every((key) => !key.includes("test-token")), "no token in storage keys");
  } finally {
    globalThis.fetch = originalFetch;
  }
});

await test("runtime replacement while fetching snapshot leaves ordinary observation unverified", async () => {
  let reads = 0;
  const result = await load({ fetchIdentity: async () => identity(++reads === 1 ? "old" : "new") });
  assert(result.snapshot.session_name === "shared", "ordinary snapshot remains available");
  assert(result.runtimeId === null && result.namespace === null, "replacement cannot authorize attribution");
});

await test("identity session must match both probes and the actual snapshot", async () => {
  for (const session of ["other", ""]) {
    const result = await load({ fetchSnapshot: async () => snapshot(session) });
    assert(result.namespace === null, "mismatched snapshot session cannot persist");
  }
  let reads = 0;
  const result = await load({ fetchIdentity: async () => identity("same", ++reads === 1 ? "shared" : "other") });
  assert(result.namespace === null, "same runtime with changed identity session cannot persist");
});

await test("failed or empty identity has no stale preference or journal persistence", async () => {
  const forbiddenStorage = {
    getItem: () => { throw new Error("must not read storage"); },
    setItem: () => { throw new Error("must not write storage"); },
    removeItem: () => { throw new Error("must not remove storage"); },
  };
  for (const failureAt of [0, 1, 2]) {
    let reads = 0;
    const result = await load({ fetchIdentity: async () => {
      reads += 1;
      if (reads === failureAt) throw new Error("identity unavailable");
      return failureAt === 0 ? identity("") : identity();
    } });
    assert(result.namespace === null, "failure cannot reuse a last-seen ID");
    assert(readStoredMonitorOrder(forbiddenStorage, result.namespace).length === 0, "unknown order is empty");
    assert(readStoredWatchedKeys(forbiddenStorage, result.namespace).length === 0, "unknown watch is empty");
    assert(!writeStoredMonitorOrder(forbiddenStorage, ["guess"], result.namespace), "unknown order is not written");
    assert(!writeStoredWatchedKeys(forbiddenStorage, ["guess"], result.namespace), "unknown watch is not written");
    const controller = createPaneJournalController({ storage: forbiddenStorage, identity: null, scheduler });
    controller.observe({ source: "raw", text: "unattributed", seenAtUnixMs: Date.now() });
    assert(controller.flush() === null && controller.pendingCount() === 0, "unknown journal ignores observations");
  }
});

await test("a rejected identity authorization remains an authentication failure", async () => {
  let rejected = false;
  try {
    await load({ fetchIdentity: async () => { throw new ApiUnauthorizedError("denied"); } });
  } catch (error) { rejected = error instanceof ApiUnauthorizedError; }
  assert(rejected, "unauthorized must reach the existing sign-out path");
});

await test("delayed prior-generation identity and snapshot completion cannot publish proof", async () => {
  for (const stage of ["identity", "snapshot"]) {
    let current = true;
    const held = deferred<RuntimeIdentityResponse | TaarofStateSnapshot>();
    const entered = deferred<void>();
    let identityReads = 0;
    const proofs: Array<string | null> = [];
    const request = load({ isCurrent: () => current,
      fetchIdentity: async () => {
        identityReads += 1;
        if (stage === "identity") entered.resolve();
        return stage === "identity" ? await held.promise as RuntimeIdentityResponse : identity();
      },
      fetchSnapshot: async () => {
        if (stage === "snapshot") entered.resolve();
        return stage === "snapshot" ? await held.promise as TaarofStateSnapshot : snapshot();
      },
      onIdentity: (namespace) => { proofs.push(namespace); },
    });
    await entered.promise;
    current = false;
    const before = proofs.length;
    held.resolve(stage === "identity" ? identity("old") : snapshot());
    let rejected = false;
    try { await request; } catch (error) { rejected = error instanceof DOMException && error.name === "AbortError"; }
    assert(rejected && proofs.length === before, "late completion cannot publish identity");
    assert(identityReads === 1, "late completion cannot start another identity probe");
  }
});

await test("abort while waiting for second identity rejects delayed verified proof", async () => {
  const controller = new AbortController();
  const held = deferred<RuntimeIdentityResponse>();
  const entered = deferred<void>();
  let reads = 0;
  const request = load({ signal: controller.signal, fetchIdentity: () => {
    if (++reads === 1) return Promise.resolve(identity());
    entered.resolve();
    return held.promise;
  } });
  await entered.promise;
  controller.abort();
  held.resolve(identity());
  let rejected = false;
  try { await request; } catch (error) { rejected = error instanceof DOMException && error.name === "AbortError"; }
  assert(rejected, "aborted proof must never publish");
});

await test("production guarded refresh suppresses a queued write after generation replacement", async () => {
  let current = true;
  let published = false;
  const writes: Array<() => void> = [];
  await runGuardedRefresh({ signal: new AbortController().signal, isCurrent: () => current,
    load: () => load({ isCurrent: () => current }), enqueueWrite: (write) => writes.push(write),
    onSuccess: () => { published = true; }, onLoading: () => {}, onError: () => {},
    isUnauthorized: () => false, onUnauthorized: () => {} });
  assert(writes.length === 1, "verified result must exercise actual queued write");
  current = false;
  writes[0]();
  assert(!published, "old queued writes must not restore old persistence authority");
});

await test("cancelled previous-runtime frame controller cannot append after replacement", () => {
  const storage = new MemoryStorage();
  const a = paneIdentity();
  const b = paneIdentity("replacement-runtime", "new pane");
  const old = createPaneJournalController({ storage, identity: a, scheduler });
  const current = createPaneJournalController({ storage, identity: b, scheduler });
  old.cancel();
  old.observe({ source: "raw", text: "delayed old callback", seenAtUnixMs: Date.now() });
  old.flush();
  current.observe({ source: "snapshot", text: "current runtime", seenAtUnixMs: Date.now() });
  current.flush();
  assert(loadPaneJournal(storage, a.paneKey).entries.length === 0, "cancelled callback is discarded");
  assert(loadPaneJournal(storage, b.paneKey).entries.map((entry) => entry.text).join() === "current runtime", "old callback cannot contaminate current journal");
});

await test("historical coordinate keys remain untouched and never migrate by numeric guess", () => {
  const storage = new MemoryStorage();
  const legacyJournalKey = "taarof.web.paneJournal.v1." + encodeURIComponent("shared:1:2:0");
  storage.setItem(MONITOR_ORDER_STORAGE_KEY, '["1:2:0"]');
  storage.setItem(MONITOR_WATCH_STORAGE_KEY, '["1:2:0"]');
  storage.setItem(legacyJournalKey, "historical data");
  const historical = [...storage.values.entries()];
  const namespace = monitorNamespace("host-a-runtime", "shared");
  assert(readStoredMonitorOrder(storage, namespace).length === 0 && readStoredWatchedKeys(storage, namespace).length === 0, "legacy preferences are not adopted");
  assert(loadPaneJournal(storage, paneIdentity().paneKey).entries.length === 0, "legacy journal is not adopted");
  assert(historical.every(([key, value]) => storage.getItem(key) === value), "historical keys are preserved");
});

await test("namespace tuple encoding disambiguates session and runtime delimiters", () => {
  assert(monitorNamespace("runtime:session", "tail") !== monitorNamespace("runtime", "session:tail"), "tuple identities cannot collide");
  assert(buildPaneTargets(snapshot(), "verified").length === 0, "empty verified snapshot remains ordinary UI data");
});

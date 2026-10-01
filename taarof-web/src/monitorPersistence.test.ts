import { MonitorPersistence, monitorViewIdentity } from "./monitorPersistence.js";
import { loadMonitorSnapshot, monitorNamespace } from "./monitorIdentity.js";
import { submitToken, signOut, type AuthStateTarget } from "./authSession.js";
import { EventRecoveryController, type EventPage, type EventSocket } from "./eventRecovery.js";
import { buildPaneIdentity, appendPaneTextObservation, loadPaneJournal, type PaneJournalStorage } from "./monitorPaneJournal.js";
import { readStoredMonitorOrder, readStoredWatchedKeys, buildPaneTargets } from "./monitorBoard.js";
import type { TaarofStateSnapshot } from "./types.js";
import type { PaneJournalScheduler } from "./usePaneJournal.js";

function assert(value: unknown, message: string): asserts value {
  if (!value) throw new Error(message);
}

async function test(name: string, fn: () => void | Promise<void>) {
  try {
    await fn();
    console.log(`PASS ${name}`);
  } catch (error) {
    console.error(`FAIL ${name}`);
    throw error;
  }
}

class Storage implements PaneJournalStorage {
  values = new Map<string, string>();
  failWrites = false;
  reads = 0;
  writes = 0;
  removals = 0;
  getItem(key: string) { this.reads += 1; return this.values.get(key) ?? null; }
  setItem(key: string, value: string) {
    this.writes += 1;
    if (this.failWrites) throw new Error("storage refused");
    this.values.set(key, value);
  }
  removeItem(key: string) { this.removals += 1; this.values.delete(key); }
  counts() { return `${this.reads}:${this.writes}:${this.removals}`; }
}

class JournalScheduler implements PaneJournalScheduler {
  next = 0;
  callbacks = new Map<unknown, () => void>();
  setTimeout(callback: () => void) { const id = ++this.next; this.callbacks.set(id, callback); return id; }
  clearTimeout(id: unknown) { this.callbacks.delete(id); }
  run() { const callbacks = [...this.callbacks.values()]; this.callbacks.clear(); callbacks.forEach((fn) => fn()); }
}

class EventClock {
  next = 0;
  timers = new Map<number, { callback: () => void; delay: number }>();
  waiters = new Map<number, () => void>();
  now = () => Date.now();
  setTimeout = (callback: () => void, delay: number) => {
    const id = ++this.next;
    this.timers.set(id, { callback, delay });
    this.waiters.get(delay)?.();
    this.waiters.delete(delay);
    return id;
  };
  clearTimeout = (id: number) => { this.timers.delete(id); };
  async fire(delay: number) {
    if (![...this.timers.values()].some((timer) => timer.delay === delay)) {
      await new Promise<void>((resolve) => { this.waiters.set(delay, resolve); });
    }
    const next = [...this.timers.entries()].find(([, timer]) => timer.delay === delay);
    assert(next, "expected event timer must actually exist");
    this.timers.delete(next[0]);
    next[1].callback();
  }
}

class Socket implements EventSocket {
  listeners = new Map<string, Set<(event: unknown) => void>>();
  addEventListener(type: string, listener: (event: unknown) => void) {
    const listeners = this.listeners.get(type) ?? new Set();
    listeners.add(listener); this.listeners.set(type, listeners);
  }
  removeEventListener(type: string, listener: (event: unknown) => void) { this.listeners.get(type)?.delete(listener); }
  close() {}
  emit(type: string, event: unknown = {}) { this.listeners.get(type)?.forEach((listener) => listener(event)); }
}

function snapshot(session = "shared", panes = true): TaarofStateSnapshot {
  return {
    schema: "test", session_name: session, generated_at_unix_ms: 0, active_workspace: 1, active_tab: 2,
    workspaces: panes ? [{
      id: 1, name: "Main", active_tab: 2, collapsed: false, repo_root: "/repo", branch_name: "main",
      is_worktree: false, tmux_backed: true, host_config_name: null, tab_count: 1,
      tabs: [{ tab_id: 2, name: "Pane", kind: "agent", focused_pane: 0, agent_running: false,
        agent_name: null, agent_session_id: null, agent_pane_id: null, agent_activity: null,
        needs_attention: false, notification_msg: null, listening_ports: [],
        panes: [{ pane_id: 0, shell_running: true, has_child_process: false, remote_shell: false,
          cwd: "/repo", cwd_host: null, tmux_session: "shared", tmux_host: "localhost",
          attach_supported: true, attach_kind: "tmux" }] }],
    }] : [],
  };
}

function paneIdentity(runtimeId = "runtime-a", sessionName = "shared") {
  return buildPaneIdentity({ runtimeId, sessionName, workspaceId: 1, workspaceName: "Main",
    tabId: 2, tabName: "Pane", paneId: 0 });
}

async function verified(persistence: MonitorPersistence, runtimeId = "runtime-a", state = snapshot()) {
  const result = await loadMonitorSnapshot({ signal: new AbortController().signal, isCurrent: () => true,
    fetchIdentity: async () => ({ schema: "test", runtime_id: runtimeId, session_name: state.session_name, identity: null }),
    fetchSnapshot: async () => state, isUnauthorized: () => false,
    onIdentity: (namespace) => persistence.observeIdentity(namespace) });
  persistence.verify(result);
  return state;
}

function eventRecovery(persistence: MonitorPersistence) {
  const sockets: Socket[] = [];
  const clock = new EventClock();
  const verifiedWaiters: Array<() => void> = [];
  let disconnected = 0;
  const controller = new EventRecoveryController({
    createSocket: () => { const socket = new Socket(); sockets.push(socket); return socket; },
    fetchRuntimeIdentity: async (signal) => {
      signal.throwIfAborted();
      persistence.observeIdentity(monitorNamespace("runtime-a", "shared"));
      return "runtime-a";
    },
    fetchEvents: async () => ({ schema: "test", since_seq: null, limit: 256, next_seq: 0,
      high_watermark: 0, oldest_seq: null, gap: false, gap_from: null, gap_to: null,
      resnapshot_required: false, capacity: 1000, dropped: 0, last_dropped_at_unix_ms: null,
      events: [] } satisfies EventPage),
    recoverSnapshot: async (signal) => {
      const result = await loadMonitorSnapshot({ signal, isCurrent: () => !signal.aborted,
        fetchIdentity: async () => ({ schema: "test", runtime_id: "runtime-a", session_name: "shared", identity: null }),
        fetchSnapshot: async () => snapshot(), isUnauthorized: () => false,
        onIdentity: (namespace) => persistence.observeIdentity(namespace) });
      persistence.verify(result);
    },
    onEvent: () => {}, onRecoveryBoundary: () => {}, onRuntimeReset: () => {}, onUnauthorized: () => {},
    onStatus: (status) => {
      persistence.observeConnection(status.connection);
      if (status.connection === "disconnected") disconnected += 1;
      if (status.connection === "connected" && status.freshness === "verified") {
        verifiedWaiters.splice(0).forEach((resolve) => resolve());
      }
    },
    isUnauthorized: () => false, clock, random: () => 0.5,
  });
  const nextVerified = () => new Promise<void>((resolve) => { verifiedWaiters.push(resolve); });
  return { controller, sockets, clock, nextVerified, get disconnected() { return disconnected; } };
}

await test("actual token submission and EventRecovery stop retain refused journal and memory-only watch/order", async () => {
  const storage = new Storage();
  const scheduler = new JournalScheduler();
  const persistence = new MonitorPersistence(storage, scheduler);
  const identity = paneIdentity();
  const now = Date.now();
  appendPaneTextObservation(storage, identity, { source: "snapshot", text: "seed", seenAtUnixMs: now });
  const state = await verified(persistence);
  const initialKey = monitorViewIdentity(persistence.getSnapshot(), state).key;
  const lease = persistence.acquire(identity, () => {});
  assert(lease, "actual production card lease must exist");
  const events = eventRecovery(persistence);
  events.controller.start();
  const firstVerified = events.nextVerified();
  events.sockets[0].emit("open");
  await firstVerified;
  storage.failWrites = true;
  persistence.commitOrder([identity.paneKey], monitorNamespace("runtime-a", "shared"));
  persistence.commitWatchedKeys([identity.paneKey], monitorNamespace("runtime-a", "shared"));
  lease.observe({ source: "raw", text: "unsaved", seenAtUnixMs: now + 1 });
  // Capture a timer that might already be queued when teardown begins.
  const delayedTimer = [...scheduler.callbacks.values()][0];
  let tokenSet = false;
  const target: AuthStateTarget = {
    stopConnections: () => persistence.stopConnections(() => events.controller.stop()),
    setToken: () => {
      tokenSet = true;
      assert(persistence.getSnapshot().proof === null, "submitToken must suspend before the token setter");
    },
    setTokenPersisted: () => {}, setTokenError: () => {}, clearAuthenticatedState: () => persistence.reset(),
  };
  submitToken(target, "synthetic-rotated-authorization", null);
  assert(tokenSet && events.disconnected > 0, "actual submitToken must synchronously trigger EventRecovery.stop status");
  const paused = monitorViewIdentity(persistence.getSnapshot(), state);
  assert(paused.key === initialKey && paused.namespace === null && paused.runtimeId === null, "board lifetime survives while storage authority is revoked");
  const counts = storage.counts();
  delayedTimer();
  lease.observe({ source: "raw", text: "unknown must drop", seenAtUnixMs: now + 2 });
  lease.release(); // Actual usePaneJournal unmount cleanup seam.
  scheduler.run();
  assert(storage.counts() === counts, "paused timer and cleanup cannot touch historical storage");
  assert(persistence.getSnapshot().orderedKeys[0] === identity.paneKey && persistence.getSnapshot().watchedKeys[0] === identity.paneKey, "refused preferences survive rotation in memory");
  events.controller.start();
  const secondVerified = events.nextVerified();
  events.sockets[1].emit("open");
  await secondVerified;
  const resumed = persistence.acquire(identity, () => {});
  assert(resumed?.currentJournal()?.entries.map((entry) => entry.text).join() === "seed,unsaved", "exact same runtime resumes the original unsaved journal");
  assert(monitorViewIdentity(persistence.getSnapshot(), persistence.getSnapshot().proof?.snapshot ?? null).key === initialKey, "same-runtime recovery keeps the production board key");
  assert(loadPaneJournal(storage, identity.paneKey).entries.map((entry) => entry.text).join() === "seed", "refused retries leave the saved seed intact");
  storage.failWrites = false;
  resumed.observe({ source: "raw", text: "retry succeeds", seenAtUnixMs: now + 3 });
  scheduler.run();
  await verified(persistence);
  assert(loadPaneJournal(storage, identity.paneKey).entries.map((entry) => entry.text).join() === "seed,unsaved,retry succeeds", "successful retry preserves every verified observation");
  assert(readStoredMonitorOrder(storage, monitorNamespace("runtime-a", "shared"))[0] === identity.paneKey && readStoredWatchedKeys(storage, monitorNamespace("runtime-a", "shared"))[0] === identity.paneKey, "memory-only preferences retry to the same namespace");
  events.controller.stop();
});

await test("actual disconnect and reconnect resume quarantined state only after matching snapshot proof", async () => {
  const storage = new Storage();
  const scheduler = new JournalScheduler();
  const persistence = new MonitorPersistence(storage, scheduler);
  const identity = paneIdentity();
  const events = eventRecovery(persistence);
  events.controller.start();
  const firstVerified = events.nextVerified();
  events.sockets[0].emit("open");
  await firstVerified;
  const lease = persistence.acquire(identity, () => {});
  assert(lease, "verified card lease must exist");
  const now = Date.now();
  lease.observe({ source: "snapshot", text: "seed", seenAtUnixMs: now });
  scheduler.run();
  storage.failWrites = true;
  lease.observe({ source: "replace", text: "unsaved", seenAtUnixMs: now + 1 });
  scheduler.run();
  const namespace = monitorNamespace("runtime-a", "shared");
  persistence.commitOrder([identity.paneKey], namespace);
  persistence.commitWatchedKeys([identity.paneKey], namespace);
  const key = monitorViewIdentity(persistence.getSnapshot(), persistence.getSnapshot().proof?.snapshot ?? null).key;
  events.sockets[0].emit("close", { code: 1006 });
  assert(persistence.getSnapshot().proof === null, "actual close status immediately suspends authority");
  const counts = storage.counts();
  lease.release();
  scheduler.run();
  assert(storage.counts() === counts, "unmount while disconnected cannot write or read storage");
  // The reconnect authorization probe is not snapshot proof and cannot resume.
  await events.clock.fire(500);
  assert(persistence.getSnapshot().proof === null, "identity probe alone cannot resume retained persistence");
  storage.failWrites = false;
  const secondVerified = events.nextVerified();
  events.sockets[1].emit("open");
  await secondVerified;
  assert(monitorViewIdentity(persistence.getSnapshot(), persistence.getSnapshot().proof?.snapshot ?? null).key === key, "reconnect retains board lifetime");
  assert(loadPaneJournal(storage, identity.paneKey).entries.map((entry) => entry.text).join() === "seed,unsaved", "matching reconnect proof retries the retained replacement");
  assert(readStoredMonitorOrder(storage, namespace)[0] === identity.paneKey && readStoredWatchedKeys(storage, namespace)[0] === identity.paneKey, "matching reconnect proof restores refused preferences");
  events.controller.stop();
});

await test("unknown changed snapshot stays distinct and cannot consume quarantined panes or preferences", async () => {
  const storage = new Storage();
  const scheduler = new JournalScheduler();
  const persistence = new MonitorPersistence(storage, scheduler);
  const state = await verified(persistence);
  const identity = paneIdentity();
  const lease = persistence.acquire(identity, () => {});
  assert(lease, "known verified lease exists");
  storage.failWrites = true;
  lease.observe({ source: "raw", text: "retained", seenAtUnixMs: Date.now() });
  persistence.commitWatchedKeys([identity.paneKey], monitorNamespace("runtime-a", "shared"));
  const unknown = snapshot("different-session", false);
  persistence.verify({ snapshot: unknown, runtimeId: null, namespace: null });
  const counts = storage.counts();
  const view = monitorViewIdentity(persistence.getSnapshot(), unknown);
  assert(view.namespace === null && view.runtimeId === null, "unknown current snapshot never inherits old runtime attribution");
  assert(persistence.acquire(identity, () => {}) === null, "unknown proof cannot read or attach quarantined journals");
  persistence.commitWatchedKeys(["numeric guess"], null);
  persistence.commitOrder(["numeric guess"], null);
  lease.observe({ source: "raw", text: "unknown", seenAtUnixMs: Date.now() });
  lease.release();
  scheduler.run();
  assert(storage.counts() === counts, "unknown proof performs no storage I/O");
  storage.failWrites = false;
  await verified(persistence, "runtime-a", state);
  const resumed = persistence.acquire(identity, () => {});
  assert(resumed?.currentJournal()?.entries.map((entry) => entry.text).join() === "retained", "unknown topology cannot erase previously attributed state");
  assert(persistence.getSnapshot().watchedKeys[0] === identity.paneKey, "unknown guesses cannot replace quarantined preferences");
  persistence.reset();
});

await test("failed identity and snapshot session mismatch suspend retained state without guessed storage", async () => {
  for (const failure of ["request", "session"]) {
    const storage = new Storage();
    const scheduler = new JournalScheduler();
    const persistence = new MonitorPersistence(storage, scheduler);
    await verified(persistence);
    const identity = paneIdentity();
    const lease = persistence.acquire(identity, () => {});
    assert(lease, "original verified pane exists");
    storage.failWrites = true;
    lease.observe({ source: "snapshot", text: "retained verified text", seenAtUnixMs: Date.now() });
    persistence.commitWatchedKeys([identity.paneKey], monitorNamespace("runtime-a", "shared"));
    const unknown = await loadMonitorSnapshot({ signal: new AbortController().signal, isCurrent: () => true,
      fetchIdentity: async () => {
        if (failure === "request") throw new Error("identity unavailable");
        return { schema: "test", runtime_id: "runtime-a", session_name: "wrong-session", identity: null };
      },
      fetchSnapshot: async () => snapshot(), isUnauthorized: () => false,
      onIdentity: (namespace) => persistence.observeIdentity(namespace) });
    persistence.verify(unknown);
    assert(unknown.namespace === null && persistence.getSnapshot().proof === null, "failed identity cannot retain persistence authority");
    const counts = storage.counts();
    lease.observe({ source: "raw", text: "must not attribute", seenAtUnixMs: Date.now() + 1 });
    lease.release();
    assert(persistence.acquire(identity, () => {}) === null, "failed proof cannot read a quarantined controller");
    scheduler.run();
    assert(storage.counts() === counts, "failed proof cleanup and callbacks never access storage");
    storage.failWrites = false;
    await verified(persistence);
    assert(loadPaneJournal(storage, identity.paneKey).entries.map((entry) => entry.text).join() === "retained verified text", "same original identity resumes only previously attributed frames");
    assert(persistence.getSnapshot().watchedKeys[0] === identity.paneKey, "failure cannot erase refused preferences");
    persistence.reset();
  }
});

await test("runtime/session replacement discards prior buffers and rejects all old-generation leases", async () => {
  for (const [runtime, session] of [["runtime-b", "shared"], ["runtime-a", "other-session"]]) {
    const storage = new Storage();
    const scheduler = new JournalScheduler();
    const persistence = new MonitorPersistence(storage, scheduler);
    const state = await verified(persistence);
    const identity = paneIdentity();
    const old = persistence.acquire(identity, () => {});
    assert(old, "old verified lease exists");
    old.observe({ source: "snapshot", text: "saved old", seenAtUnixMs: Date.now() });
    scheduler.run();
    storage.failWrites = true;
    old.observe({ source: "raw", text: "unsaved old", seenAtUnixMs: Date.now() + 1 });
    persistence.commitOrder([identity.paneKey], monitorNamespace("runtime-a", "shared"));
    persistence.commitWatchedKeys([identity.paneKey], monitorNamespace("runtime-a", "shared"));
    const key = monitorViewIdentity(persistence.getSnapshot(), state).key;
    persistence.suspend();
    old.release();
    storage.failWrites = false;
    const newState = await verified(persistence, runtime, snapshot(session));
    assert(monitorViewIdentity(persistence.getSnapshot(), newState).key !== key, "actual replacement changes board lifetime");
    assert(persistence.getSnapshot().orderedKeys.length === 0 && persistence.getSnapshot().watchedKeys.length === 0, "old preferences never migrate to replacement");
    old.observe({ source: "raw", text: "delayed old callback", seenAtUnixMs: Date.now() + 2 });
    const nextIdentity = paneIdentity(runtime, session);
    const current = persistence.acquire(nextIdentity, () => {});
    assert(current, "replacement pane acquires a fresh controller");
    current.observe({ source: "snapshot", text: "new runtime", seenAtUnixMs: Date.now() + 3 });
    scheduler.run();
    assert(loadPaneJournal(storage, nextIdentity.paneKey).entries.map((entry) => entry.text).join() === "new runtime", "replacement cannot inherit or append old frames");
    assert(loadPaneJournal(storage, identity.paneKey).entries.map((entry) => entry.text).join() === "saved old", "historical persisted journal is preserved without migration");
    persistence.reset();
  }
});

await test("same-runtime re-verification cannot revive a delayed prior-generation frame lease", async () => {
  const storage = new Storage();
  const scheduler = new JournalScheduler();
  const persistence = new MonitorPersistence(storage, scheduler);
  await verified(persistence);
  const identity = paneIdentity();
  const old = persistence.acquire(identity, () => {});
  assert(old, "old lease exists");
  const generation = persistence.getSnapshot().generation;
  persistence.suspend();
  await verified(persistence);
  assert(!persistence.canAttribute(identity.paneKey, generation), "queueJournalFrame production guard rejects old-generation callbacks");
  old.observe({ source: "raw", text: "delayed old", seenAtUnixMs: Date.now() });
  const current = persistence.acquire(identity, () => {});
  assert(current, "new-generation lease exists");
  current.observe({ source: "snapshot", text: "verified current", seenAtUnixMs: Date.now() });
  scheduler.run();
  assert(loadPaneJournal(storage, identity.paneKey).entries.map((entry) => entry.text).join() === "verified current", "only the current lease may attribute new observations");
  persistence.reset();
});

await test("retained journal slots are bounded by the latest verified pane set and sign-out resets them", async () => {
  const storage = new Storage();
  const scheduler = new JournalScheduler();
  const persistence = new MonitorPersistence(storage, scheduler);
  const state = await verified(persistence);
  const target = buildPaneTargets(state, "runtime-a")[0];
  const lease = persistence.acquire(paneIdentity(), () => {});
  assert(lease && target.key === paneIdentity().paneKey, "production target and controller share exact identity");
  assert(persistence.acquire(paneIdentity("other-runtime"), () => {}) === null, "foreign/unseen panes cannot grow the retained set");
  storage.failWrites = true;
  lease.observe({ source: "raw", text: "removed pane", seenAtUnixMs: Date.now() });
  await verified(persistence, "runtime-a", snapshot("shared", false));
  lease.observe({ source: "raw", text: "late removed pane", seenAtUnixMs: Date.now() });
  assert(persistence.acquire(paneIdentity(), () => {}) === null, "a removed verified pane cannot retain an active slot");
  const targetState: AuthStateTarget = {
    stopConnections: () => persistence.stopConnections(() => {}),
    clearAuthenticatedState: () => persistence.reset(), setToken: () => {},
    setTokenPersisted: () => {}, setTokenError: () => {},
  };
  signOut(targetState, null, null);
  assert(persistence.getSnapshot().retainedNamespace === null && persistence.getSnapshot().proof === null, "actual sign-out drops retained identity and authority");
  scheduler.run();
  assert(loadPaneJournal(storage, paneIdentity().paneKey).entries.length === 0, "removed/canceled slots cannot flush after sign-out");
});

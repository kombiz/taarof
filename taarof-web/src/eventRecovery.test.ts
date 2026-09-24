import {
  EventRecoveryController,
  nextEventReconnectDelayMs,
  type EventConnectionStatus,
  type EventPage,
  type EventSocket,
} from "./eventRecovery.js";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function assertEqual<T>(actual: T, expected: T, message = "values differ") {
  if (actual !== expected) {
    throw new Error(`${message}: expected ${String(expected)}, got ${String(actual)}`);
  }
}

async function test(name: string, fn: () => Promise<void> | void) {
  try {
    await fn();
    console.log(`PASS ${name}`);
  } catch (error) {
    console.error(`FAIL ${name}`);
    throw error;
  }
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

async function flush() {
  for (let turn = 0; turn < 12; turn += 1) await Promise.resolve();
}

class FakeClock {
  nowMs = 1_000;
  nextTimer = 0;
  timers = new Map<number, { at: number; callback: () => void }>();

  now = () => this.nowMs;

  setTimeout = (callback: () => void, delayMs: number) => {
    this.nextTimer += 1;
    this.timers.set(this.nextTimer, { at: this.nowMs + delayMs, callback });
    return this.nextTimer;
  };

  clearTimeout = (timer: number) => {
    this.timers.delete(timer);
  };

  async advance(delayMs: number) {
    const target = this.nowMs + delayMs;
    while (true) {
      const next = [...this.timers.entries()]
        .filter(([, timer]) => timer.at <= target)
        .sort((left, right) => left[1].at - right[1].at || left[0] - right[0])[0];
      if (!next) break;
      const [id, timer] = next;
      this.timers.delete(id);
      this.nowMs = timer.at;
      timer.callback();
      await flush();
    }
    this.nowMs = target;
    await flush();
  }
}

class FakeSocket implements EventSocket {
  private listeners = new Map<string, Set<(event: unknown) => void>>();
  closed = false;

  addEventListener(type: string, listener: (event: unknown) => void) {
    const listeners = this.listeners.get(type) ?? new Set();
    listeners.add(listener);
    this.listeners.set(type, listeners);
  }

  removeEventListener(type: string, listener: (event: unknown) => void) {
    this.listeners.get(type)?.delete(listener);
  }

  close() {
    this.closed = true;
  }

  emit(type: string, event: unknown = {}) {
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }

  open() {
    this.emit("open");
  }

  message(data: string) {
    this.emit("message", { data });
  }

  disconnect(code = 1006) {
    this.emit("close", { code });
  }
}

function page(
  events: EventPage["events"],
  highWatermark: number,
  overrides: Partial<EventPage> = {},
): EventPage {
  return {
    schema: "taarof.events.v1",
    since_seq: null,
    limit: 256,
    next_seq: events[events.length - 1]?.seq ?? 0,
    high_watermark: highWatermark,
    oldest_seq: events[0]?.seq ?? null,
    gap: false,
    gap_from: null,
    gap_to: null,
    resnapshot_required: false,
    capacity: 1_000,
    dropped: 0,
    last_dropped_at_unix_ms: null,
    events,
    ...overrides,
  };
}

function event(seq: number, eventType = "work_recorded") {
  return { seq, ts_unix_ms: seq * 10, event_type: eventType, payload: {} };
}

function harness(overrides: {
  fetchRuntimeIdentity?: (signal: AbortSignal) => Promise<string>;
  fetchEvents?: (cursor: number, signal: AbortSignal) => Promise<EventPage>;
  recoverSnapshot?: (signal: AbortSignal) => Promise<void>;
  isUnauthorized?: (error: unknown) => boolean;
} = {}) {
  const clock = new FakeClock();
  const sockets: FakeSocket[] = [];
  const statuses: EventConnectionStatus[] = [];
  const applied: number[] = [];
  const appliedSources: Array<"live" | "recovery"> = [];
  const calls: string[] = [];
  let boundaries = 0;
  let resets = 0;
  let unauthorized = 0;
  let snapshots = 0;

  const controller = new EventRecoveryController({
    createSocket: () => {
      const socket = new FakeSocket();
      sockets.push(socket);
      return socket;
    },
    fetchRuntimeIdentity: overrides.fetchRuntimeIdentity ?? (async () => "runtime-a"),
    fetchEvents: overrides.fetchEvents ?? (async (cursor) => page([], cursor)),
    recoverSnapshot: overrides.recoverSnapshot ?? (async () => { snapshots += 1; }),
    onEvent: (data, source) => {
      const parsed = JSON.parse(data) as { seq?: number };
      if (parsed.seq !== undefined) applied.push(parsed.seq);
      appliedSources.push(source);
    },
    onRecoveryBoundary: () => {
      boundaries += 1;
      calls.push("boundary");
    },
    onRuntimeReset: () => {
      resets += 1;
      calls.push("reset");
    },
    onUnauthorized: () => {
      unauthorized += 1;
      calls.push("unauthorized");
    },
    onStatus: (status) => statuses.push(status),
    isUnauthorized: overrides.isUnauthorized ?? (() => false),
    clock,
    random: () => 0.5,
  });

  return {
    controller,
    clock,
    sockets,
    statuses,
    applied,
    appliedSources,
    calls,
    get boundaries() { return boundaries; },
    get resets() { return resets; },
    get unauthorized() { return unauthorized; },
    get snapshots() { return snapshots; },
  };
}

test("a silent reopened socket stays stale until replay and snapshot both succeed", async () => {
  const identity = deferred<string>();
  const snapshot = deferred<void>();
  let snapshotStarted = false;
  const h = harness({
    fetchRuntimeIdentity: () => identity.promise,
    fetchEvents: async () => page([], 0),
    recoverSnapshot: () => {
      snapshotStarted = true;
      return snapshot.promise;
    },
  });

  h.controller.start();
  h.sockets[0].open();
  assertEqual(h.boundaries, 1, "opening recovery must rotate stale request ownership first");
  assertEqual(h.statuses[h.statuses.length - 1]?.connection, "recovering");
  assertEqual(h.statuses[h.statuses.length - 1]?.freshness, "stale");

  identity.resolve("runtime-a");
  await flush();
  assertEqual(snapshotStarted, true);
  assertEqual(h.statuses[h.statuses.length - 1]?.freshness, "stale", "WebSocket open is not verified state");

  snapshot.resolve();
  await flush();
  assertEqual(h.statuses[h.statuses.length - 1]?.connection, "connected");
  assertEqual(h.statuses[h.statuses.length - 1]?.freshness, "verified");
  assertEqual(h.statuses[h.statuses.length - 1]?.last_verified_at_unix_ms, 1_000);
});

test("live events buffered during replay and snapshot apply once before verification", async () => {
  const firstSnapshot = deferred<void>();
  let snapshots = 0;
  const h = harness({
    fetchEvents: async (cursor) =>
      cursor === 0 ? page([event(1)], 1, { next_seq: 1 }) : page([], 1, { next_seq: cursor }),
    recoverSnapshot: () => {
      snapshots += 1;
      return snapshots === 1 ? firstSnapshot.promise : Promise.resolve();
    },
  });

  h.controller.start();
  h.sockets[0].open();
  await flush();
  h.sockets[0].message(JSON.stringify(event(2)));
  h.sockets[0].message(JSON.stringify(event(2)));
  firstSnapshot.resolve();
  await flush();

  assertEqual(h.applied.join(","), "1,2", "replay/live overlap must be deduplicated");
  assertEqual(snapshots, 2, "an event arriving during snapshot requires a closing snapshot");
  assertEqual(h.statuses[h.statuses.length - 1]?.cursor, 2);
  assertEqual(h.statuses[h.statuses.length - 1]?.freshness, "verified");
});

test("continuous events during snapshots hand off stale work without disconnecting", async () => {
  let snapshots = 0;
  let h!: ReturnType<typeof harness>;
  h = harness({
    fetchEvents: async (cursor) => page([], cursor, { next_seq: cursor }),
    recoverSnapshot: async () => {
      snapshots += 1;
      h.sockets[0].message(JSON.stringify(event(snapshots)));
    },
  });

  h.controller.start();
  h.sockets[0].open();
  await flush();

  assertEqual(snapshots, 2, "recovery should take one closing snapshot without waiting for quiet");
  assertEqual(h.applied.join(","), "1,2");
  assertEqual(h.appliedSources.join(","), "recovery,live");
  assertEqual(h.sockets[0].closed, false, "a healthy busy stream must stay connected");
  assertEqual(h.statuses[h.statuses.length - 1]?.connection, "connected");
  assertEqual(h.statuses[h.statuses.length - 1]?.freshness, "stale");
});

test("retention gaps and unsequenced lag frames force a snapshot", async () => {
  let fetches = 0;
  let snapshots = 0;
  const h = harness({
    fetchEvents: async () => {
      fetches += 1;
      return fetches === 1
        ? page([event(9)], 9, {
            next_seq: 9,
            oldest_seq: 9,
            gap: true,
            gap_from: 1,
            gap_to: 8,
            resnapshot_required: true,
          })
        : page([], 9, { next_seq: 9 });
    },
    recoverSnapshot: async () => { snapshots += 1; },
  });

  h.controller.start();
  h.sockets[0].open();
  await flush();
  assertEqual(snapshots, 1, "a ring gap requires a snapshot");

  h.sockets[0].message(JSON.stringify({
    event_type: "_lagged",
    skipped: 3,
    resnapshot_required: true,
  }));
  await flush();
  assertEqual(snapshots, 2, "_lagged requires a snapshot even when the ring page has no gap");
});

test("runtime identity change and an empty-page high-watermark reset discard the old cursor", async () => {
  let identity = "runtime-a";
  let phase = 0;
  const cursors: number[] = [];
  const h = harness({
    fetchRuntimeIdentity: async () => identity,
    fetchEvents: async (cursor) => {
      cursors.push(cursor);
      if (phase === 0) return page([event(1), event(2)], 2, { next_seq: 2 });
      return page([], 0, { since_seq: cursor, next_seq: cursor });
    },
  });

  h.controller.start();
  h.sockets[0].open();
  await flush();
  assertEqual(h.statuses[h.statuses.length - 1]?.cursor, 2);

  phase = 1;
  identity = "runtime-b";
  h.sockets[0].disconnect();
  await flush();
  await h.clock.advance(500);
  h.sockets[1].open();
  await flush();

  assertEqual(h.resets, 1, "runtime identity change must reset the old cursor");
  assertEqual(cursors[cursors.length - 1], 0);
  assertEqual(h.statuses[h.statuses.length - 1]?.cursor, 0);
});

test("a runtime restart during snapshot restarts recovery before verification", async () => {
  const identities = ["runtime-a", "runtime-b", "runtime-b"];
  let identityReads = 0;
  let snapshots = 0;
  const cursors: number[] = [];
  const h = harness({
    fetchRuntimeIdentity: async () => identities[Math.min(identityReads++, identities.length - 1)],
    fetchEvents: async (cursor) => {
      cursors.push(cursor);
      return cursor === 0 && snapshots === 0
        ? page([event(1)], 1, { next_seq: 1 })
        : page([], 0, { next_seq: cursor });
    },
    recoverSnapshot: async () => { snapshots += 1; },
  });

  h.controller.start();
  h.sockets[0].open();
  await flush();

  assertEqual(h.resets, 1, "the mid-recovery runtime change must reset the cursor");
  assertEqual(snapshots, 2, "the replacement runtime needs its own successful snapshot");
  assertEqual(cursors.join(","), "0,0");
  assertEqual(h.statuses[h.statuses.length - 1]?.freshness, "verified");
});

test("a lower high watermark resets even when the server echoes an empty-page cursor", async () => {
  let phase = 0;
  const cursors: number[] = [];
  const h = harness({
    fetchEvents: async (cursor) => {
      cursors.push(cursor);
      if (phase === 0) return page([event(1), event(2), event(3)], 3, { next_seq: 3 });
      return page([], 0, { since_seq: cursor, next_seq: cursor });
    },
  });
  h.controller.start();
  h.sockets[0].open();
  await flush();

  phase = 1;
  h.sockets[0].message(JSON.stringify({ event_type: "_lagged", skipped: 1, resnapshot_required: true }));
  await flush();

  assertEqual(h.resets, 1);
  assertEqual(cursors.slice(-2).join(","), "3,0", "reset must re-page from zero");
  assertEqual(h.statuses[h.statuses.length - 1]?.cursor, 0);
});

test("reconnect delay is jittered, exponential, and capped", () => {
  assertEqual(nextEventReconnectDelayMs(0, 0.5), 500);
  assertEqual(nextEventReconnectDelayMs(1, 0.5), 1_000);
  assertEqual(nextEventReconnectDelayMs(9, 0.5), 10_000);
  assertEqual(nextEventReconnectDelayMs(9, 0), 8_000);
  assertEqual(nextEventReconnectDelayMs(9, 1), 10_000);
});

test("repeated connection failures retry with bounded increasing delays", async () => {
  const clock = new FakeClock();
  let attempts = 0;
  const controller = new EventRecoveryController({
    createSocket: () => {
      attempts += 1;
      throw new Error("synthetic socket failure");
    },
    fetchRuntimeIdentity: async () => "runtime-a",
    fetchEvents: async () => page([], 0),
    recoverSnapshot: async () => undefined,
    onEvent: () => undefined,
    onRecoveryBoundary: () => undefined,
    onRuntimeReset: () => undefined,
    onUnauthorized: () => undefined,
    onStatus: () => undefined,
    isUnauthorized: () => false,
    clock,
    random: () => 0.5,
  });

  controller.start();
  assertEqual(attempts, 1);
  await clock.advance(499);
  assertEqual(attempts, 1);
  await clock.advance(1);
  assertEqual(attempts, 2);
  await clock.advance(999);
  assertEqual(attempts, 2);
  await clock.advance(1);
  assertEqual(attempts, 3);
  controller.stop();
});

test("a bounded recovery timeout closes the stale socket and schedules reconnect", async () => {
  const never = deferred<string>();
  const h = harness({ fetchRuntimeIdentity: () => never.promise });
  h.controller.start();
  h.sockets[0].open();

  await h.clock.advance(10_000);

  assertEqual(h.sockets[0].closed, true);
  assertEqual(h.statuses[h.statuses.length - 1]?.connection, "disconnected");
  assertEqual(h.clock.timers.size, 1, "timeout must leave exactly one reconnect timer");
  h.controller.stop();
});

test("terminal authorization failure and stop cancel retries and stale callbacks", async () => {
  class Unauthorized extends Error {}
  const probe = deferred<string>();
  const h = harness({
    fetchRuntimeIdentity: () => probe.promise,
    isUnauthorized: (error) => error instanceof Unauthorized,
  });

  h.controller.start();
  h.sockets[0].disconnect();
  probe.reject(new Unauthorized("expired"));
  await flush();
  assertEqual(h.unauthorized, 1);
  assertEqual(h.clock.timers.size, 0, "terminal auth rejection must not retain a retry timer");

  const lateIdentity = deferred<string>();
  const stopped = harness({ fetchRuntimeIdentity: () => lateIdentity.promise });
  stopped.controller.start();
  stopped.sockets[0].open();
  stopped.controller.stop();
  lateIdentity.resolve("runtime-a");
  await flush();
  assertEqual(stopped.snapshots, 0);
  assertEqual(stopped.clock.timers.size, 0);
  assertEqual(stopped.sockets[0].closed, true);
});

test("socket close rotates refresh ownership before the auth probe or backoff", async () => {
  const heldProbe = deferred<string>();
  const closing = harness({ fetchRuntimeIdentity: () => heldProbe.promise });
  closing.controller.start();
  closing.sockets[0].disconnect();

  assertEqual(closing.boundaries, 1, "disconnect must rotate generation synchronously");
  assertEqual(closing.clock.timers.size, 1, "the held probe owns only its timeout timer");
  closing.controller.stop();
});

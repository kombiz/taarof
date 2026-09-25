import {
  DatasetRefreshScheduler,
  RefreshRequestTimeoutError,
  classifyEventRefreshDomains,
  runGuardedRefresh,
} from "./refreshScheduler.js";

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

class FakeClock {
  nowMs = 0;
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
      for (let turn = 0; turn < 8; turn += 1) await Promise.resolve();
    }
    this.nowMs = target;
    for (let turn = 0; turn < 8; turn += 1) await Promise.resolve();
  }
}

test("event classification isolates known state events and safely refreshes ambiguous events", () => {
  assertEqual(
    classifyEventRefreshDomains(JSON.stringify({ event_type: "work_recorded" })).join(","),
    "state",
  );
  assertEqual(
    classifyEventRefreshDomains(JSON.stringify({ event_type: "agent_attention_changed" })).join(","),
    "state",
  );
  assertEqual(
    classifyEventRefreshDomains(JSON.stringify({ event_type: "runtime_probe_state_changed" })).join(","),
    "state,agent-sessions",
    "runtime probe changes can affect catalog live-binding status",
  );
  assertEqual(
    classifyEventRefreshDomains(JSON.stringify({ event_type: "command_exited" })).join(","),
    "state,agent-sessions",
    "process exits can affect catalog activity",
  );
  assertEqual(
    classifyEventRefreshDomains(JSON.stringify({ event_type: "agent_activity_changed" })).join(","),
    "state,agent-sessions",
  );
  assertEqual(
    classifyEventRefreshDomains(JSON.stringify({ event_type: "future_event" })).join(","),
    "state,agent-sessions",
  );
  assertEqual(classifyEventRefreshDomains("not-json").join(","), "state,agent-sessions");
});

test("continuous sub-150ms events refresh during the stream at a fixed bounded cadence", async () => {
  const clock = new FakeClock();
  const starts: number[] = [];
  const scheduler = new DatasetRefreshScheduler(
    async () => {
      starts.push(clock.now());
    },
    { clock, coalesceMs: 150, minIntervalMs: 1_000, requestTimeoutMs: 5_000 },
  );

  for (let event = 0; event < 301; event += 1) {
    scheduler.invalidate();
    if (event < 300) await clock.advance(100);
  }
  await clock.advance(1_200);

  assert(starts.length >= 30 && starts.length <= 32, `unexpected request count ${starts.length}`);
  assert(starts[0] <= 150, "the first event must start a refresh within the coalesce window");
  assert(
    starts.slice(1).every((start, index) => start - starts[index] <= 1_000),
    "continuous invalidation must not create a trailing-only starvation gap",
  );
  assert(starts.some((start) => start > 1_000 && start < 30_000), "refreshes must run during the event stream");
});

test("one active request collects repeated invalidations into one dirty follow-up", async () => {
  const clock = new FakeClock();
  const first = deferred<void>();
  let requests = 0;
  let active = 0;
  let maxActive = 0;
  const scheduler = new DatasetRefreshScheduler(
    async () => {
      requests += 1;
      active += 1;
      maxActive = Math.max(maxActive, active);
      if (requests === 1) await first.promise;
      active -= 1;
    },
    { clock, coalesceMs: 10, minIntervalMs: 50, requestTimeoutMs: 500 },
  );

  scheduler.invalidate();
  await clock.advance(10);
  for (let count = 0; count < 100; count += 1) scheduler.invalidate();
  assertEqual(requests, 1);
  assertEqual(scheduler.status().dirty, true);
  first.resolve();
  await clock.advance(40);
  await clock.advance(10);

  assertEqual(requests, 2, "the dirty flag should cause exactly one follow-up");
  assertEqual(maxActive, 1, "requests in one dataset must never overlap");
});

test("failed and timed-out requests release the dataset and preserve one follow-up", async () => {
  const clock = new FakeClock();
  let requests = 0;
  let timedOut = false;
  const scheduler = new DatasetRefreshScheduler(
    async (signal) => {
      requests += 1;
      if (requests === 1) {
        await new Promise<void>((_, reject) => {
          signal.addEventListener("abort", () => {
            timedOut = signal.reason instanceof RefreshRequestTimeoutError;
            reject(signal.reason);
          });
        });
      }
      if (requests === 2) throw new Error("synthetic failure");
    },
    { clock, coalesceMs: 10, minIntervalMs: 50, requestTimeoutMs: 100 },
  );

  scheduler.invalidate();
  await clock.advance(10);
  scheduler.invalidate();
  await clock.advance(100);
  await clock.advance(10);

  assertEqual(timedOut, true, "the active request should receive a typed timeout abort");
  assertEqual(requests, 2, "timeout should release the queued dirty follow-up");
  assertEqual(scheduler.status().active, false);
});

test("a timed-out request cannot commit late success or clear its active follow-up", async () => {
  const clock = new FakeClock();
  const first = deferred<string>();
  const second = deferred<string>();
  let requestNumber = 0;
  let loading = false;
  let timeoutErrors = 0;
  const successes: string[] = [];
  const scheduler = new DatasetRefreshScheduler((signal) => {
    requestNumber += 1;
    return runGuardedRefresh({
      signal,
      isCurrent: () => true,
      load: () => requestNumber === 1 ? first.promise : second.promise,
      enqueueWrite: (write) => write(),
      onLoading: (value) => { loading = value; },
      onSuccess: (value) => successes.push(value),
      onError: (error) => {
        if (error instanceof RefreshRequestTimeoutError) timeoutErrors += 1;
      },
      isUnauthorized: () => false,
      onUnauthorized: () => undefined,
    });
  }, { clock, coalesceMs: 10, minIntervalMs: 50, requestTimeoutMs: 100 });

  scheduler.invalidate();
  await clock.advance(10);
  scheduler.invalidate();
  await clock.advance(100);
  await clock.advance(10);
  assertEqual(requestNumber, 2);
  assertEqual(loading, true, "the follow-up owns loading after the timeout");
  assertEqual(timeoutErrors, 1, "the legitimate timeout should be reported once");

  first.resolve("stale-success");
  for (let turn = 0; turn < 8; turn += 1) await Promise.resolve();
  assertEqual(successes.length, 0, "late success from the aborted request must be inert");
  assertEqual(loading, true, "late settlement must not clear follow-up loading");

  second.resolve("fresh-success");
  for (let turn = 0; turn < 8; turn += 1) await Promise.resolve();
  assertEqual(successes.join(","), "fresh-success");
  assertEqual(loading, false);
});

test("a timed-out request cannot apply a late 401 to its active follow-up", async () => {
  class Unauthorized extends Error {}
  const clock = new FakeClock();
  const first = deferred<string>();
  const second = deferred<string>();
  let requestNumber = 0;
  let loading = false;
  let unauthorized = 0;
  let timeoutErrors = 0;
  const scheduler = new DatasetRefreshScheduler((signal) => {
    requestNumber += 1;
    return runGuardedRefresh({
      signal,
      isCurrent: () => true,
      load: () => requestNumber === 1 ? first.promise : second.promise,
      enqueueWrite: (write) => write(),
      onLoading: (value) => { loading = value; },
      onSuccess: () => undefined,
      onError: (error) => {
        if (error instanceof RefreshRequestTimeoutError) timeoutErrors += 1;
      },
      isUnauthorized: (error) => error instanceof Unauthorized,
      onUnauthorized: () => { unauthorized += 1; },
    });
  }, { clock, coalesceMs: 10, minIntervalMs: 50, requestTimeoutMs: 100 });

  scheduler.invalidate();
  await clock.advance(10);
  scheduler.invalidate();
  await clock.advance(100);
  await clock.advance(10);
  first.reject(new Unauthorized("stale token response"));
  for (let turn = 0; turn < 8; turn += 1) await Promise.resolve();

  assertEqual(timeoutErrors, 1);
  assertEqual(unauthorized, 0, "late 401 from the aborted request must be inert");
  assertEqual(loading, true, "late 401 must not clear follow-up loading");
  second.resolve("fresh-success");
});

test("state and agent schedulers remain independent while one request is slow", async () => {
  const clock = new FakeClock();
  const slowState = deferred<void>();
  let stateRequests = 0;
  let catalogRequests = 0;
  const state = new DatasetRefreshScheduler(async () => {
    stateRequests += 1;
    await slowState.promise;
  }, { clock, coalesceMs: 10, minIntervalMs: 50, requestTimeoutMs: 500 });
  const catalog = new DatasetRefreshScheduler(async () => {
    catalogRequests += 1;
    throw new Error("catalog failure");
  }, { clock, coalesceMs: 10, minIntervalMs: 50, requestTimeoutMs: 500 });

  for (const domain of classifyEventRefreshDomains(JSON.stringify({ event_type: "work_recorded" }))) {
    (domain === "state" ? state : catalog).invalidate();
  }
  await clock.advance(10);
  assertEqual(stateRequests, 1);
  assertEqual(catalogRequests, 0, "state-only events must not reload the catalog");

  for (const domain of classifyEventRefreshDomains(JSON.stringify({ event_type: "agent_message" }))) {
    (domain === "state" ? state : catalog).invalidate();
  }
  await clock.advance(10);
  assertEqual(catalogRequests, 1, "catalog work must run while state work is still active");
  slowState.resolve();
});

test("queued success is checked again inside the transition", async () => {
  let generation = 1;
  const load = deferred<string>();
  const queuedWrites: Array<() => void> = [];
  const successes: string[] = [];
  const request = runGuardedRefresh({
    signal: new AbortController().signal,
    isCurrent: () => generation === 1,
    load: () => load.promise,
    enqueueWrite: (write) => queuedWrites.push(write),
    onLoading: () => undefined,
    onSuccess: (value) => successes.push(value),
    onError: () => undefined,
    isUnauthorized: () => false,
    onUnauthorized: () => undefined,
  });

  load.resolve("old-token-data");
  await request;
  assertEqual(queuedWrites.length, 1);
  generation = 2;
  queuedWrites[0]();
  assertEqual(successes.length, 0, "a queued React transition from an old generation must be inert");
});

test("delayed 401 from a replaced token cannot log out or clear newer loading", async () => {
  class Unauthorized extends Error {}
  let generation = 1;
  let loading = false;
  let unauthorized = 0;
  let errors = 0;
  const load = deferred<string>();
  const request = runGuardedRefresh({
    signal: new AbortController().signal,
    isCurrent: () => generation === 1,
    load: () => load.promise,
    enqueueWrite: (write) => write(),
    onLoading: (value) => { loading = value; },
    onSuccess: () => undefined,
    onError: (error) => { if (error !== null) errors += 1; },
    isUnauthorized: (error) => error instanceof Unauthorized,
    onUnauthorized: () => { unauthorized += 1; },
  });

  assertEqual(loading, true);
  generation = 2;
  loading = true;
  load.reject(new Unauthorized("old token rejected"));
  await request;

  assertEqual(unauthorized, 0);
  assertEqual(errors, 0);
  assertEqual(loading, true, "the old finally block must not clear a newer request's loading state");
});

test("delayed failure after logout cannot restore error or loading state", async () => {
  let loggedIn = true;
  let loading = false;
  let errors = 0;
  const load = deferred<string>();
  const request = runGuardedRefresh({
    signal: new AbortController().signal,
    isCurrent: () => loggedIn,
    load: () => load.promise,
    enqueueWrite: (write) => write(),
    onLoading: (value) => { loading = value; },
    onSuccess: () => undefined,
    onError: (error) => { if (error !== null) errors += 1; },
    isUnauthorized: () => false,
    onUnauthorized: () => undefined,
  });

  loggedIn = false;
  loading = false;
  load.reject(new Error("late failure"));
  await request;

  assertEqual(errors, 0);
  assertEqual(loading, false);
});

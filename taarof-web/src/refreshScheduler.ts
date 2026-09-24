export type RefreshDomain = "state" | "agent-sessions";

export const REFRESH_COALESCE_MS = 150;
export const REFRESH_MIN_INTERVAL_MS = 1_000;
export const REFRESH_REQUEST_TIMEOUT_MS = 5_000;

// An event can arrive just after an active request starts. The dirty follow-up
// then waits for that request, the minimum interval, and its own response.
export const REFRESH_MAX_EVENT_TO_APPLY_MS =
  REFRESH_REQUEST_TIMEOUT_MS * 2 + REFRESH_MIN_INTERVAL_MS;

const ALL_REFRESH_DOMAINS: readonly RefreshDomain[] = ["state", "agent-sessions"];
const STATE_REFRESH_DOMAIN: readonly RefreshDomain[] = ["state"];

const STATE_ONLY_EVENT_TYPES = new Set([
  "alert_raised",
  "work_recorded",
  "work_preferences_changed",
  "work_ledger_cleared",
  "work_reconciled",
  "work_reconciled_batch",
]);

const AGENT_CATALOG_EVENT_TYPES = new Set([
  "agent_activity_changed",
  "agent_message",
  "agent_prompt_submitted",
]);

export function classifyEventRefreshDomains(data: unknown): readonly RefreshDomain[] {
  if (typeof data !== "string") {
    return ALL_REFRESH_DOMAINS;
  }

  let event: unknown;
  try {
    event = JSON.parse(data);
  } catch {
    return ALL_REFRESH_DOMAINS;
  }

  if (!event || typeof event !== "object") {
    return ALL_REFRESH_DOMAINS;
  }
  const eventType = (event as { event_type?: unknown }).event_type;
  if (typeof eventType !== "string") {
    return ALL_REFRESH_DOMAINS;
  }
  if (STATE_ONLY_EVENT_TYPES.has(eventType)) {
    return STATE_REFRESH_DOMAIN;
  }
  if (AGENT_CATALOG_EVENT_TYPES.has(eventType) || eventType.startsWith("agent_")) {
    return ALL_REFRESH_DOMAINS;
  }

  // `_lagged` and future or ambiguous event types require a full resnapshot.
  return ALL_REFRESH_DOMAINS;
}

export class RefreshRequestTimeoutError extends Error {
  constructor(timeoutMs: number) {
    super(`Refresh request timed out after ${timeoutMs} ms.`);
    this.name = "RefreshRequestTimeoutError";
  }
}

interface RefreshClock {
  now(): number;
  setTimeout(callback: () => void, delayMs: number): number;
  clearTimeout(timer: number): void;
}

const systemClock: RefreshClock = {
  now: () => Date.now(),
  setTimeout: (callback, delayMs) => globalThis.setTimeout(callback, delayMs),
  clearTimeout: (timer) => globalThis.clearTimeout(timer),
};

interface RefreshSchedulerOptions {
  clock?: RefreshClock;
  coalesceMs?: number;
  minIntervalMs?: number;
  requestTimeoutMs?: number;
}

export interface RefreshSchedulerStatus {
  active: boolean;
  dirty: boolean;
  scheduled: boolean;
}

export class DatasetRefreshScheduler {
  private readonly clock: RefreshClock;
  private readonly coalesceMs: number;
  private readonly minIntervalMs: number;
  private readonly requestTimeoutMs: number;
  private timer: number | null = null;
  private timeoutTimer: number | null = null;
  private activeController: AbortController | null = null;
  private active = false;
  private dirty = false;
  private disposed = false;
  private lastStartedAt: number | null = null;

  constructor(
    private readonly request: (signal: AbortSignal) => Promise<void>,
    options: RefreshSchedulerOptions = {},
  ) {
    this.clock = options.clock ?? systemClock;
    this.coalesceMs = options.coalesceMs ?? REFRESH_COALESCE_MS;
    this.minIntervalMs = options.minIntervalMs ?? REFRESH_MIN_INTERVAL_MS;
    this.requestTimeoutMs = options.requestTimeoutMs ?? REFRESH_REQUEST_TIMEOUT_MS;
  }

  invalidate({ immediate = false }: { immediate?: boolean } = {}) {
    if (this.disposed) {
      return;
    }
    if (this.active) {
      this.dirty = true;
      return;
    }
    if (this.timer !== null) {
      if (!immediate) {
        return;
      }
      this.clock.clearTimeout(this.timer);
      this.timer = null;
    }
    this.schedule(immediate);
  }

  status(): RefreshSchedulerStatus {
    return {
      active: this.active,
      dirty: this.dirty,
      scheduled: this.timer !== null,
    };
  }

  dispose() {
    if (this.disposed) {
      return;
    }
    this.disposed = true;
    this.dirty = false;
    if (this.timer !== null) {
      this.clock.clearTimeout(this.timer);
      this.timer = null;
    }
    if (this.timeoutTimer !== null) {
      this.clock.clearTimeout(this.timeoutTimer);
      this.timeoutTimer = null;
    }
    this.activeController?.abort(new DOMException("Refresh disposed.", "AbortError"));
    this.activeController = null;
  }

  private schedule(immediate: boolean) {
    const now = this.clock.now();
    const earliestByInterval =
      this.lastStartedAt === null ? now : this.lastStartedAt + this.minIntervalMs;
    const delayMs = immediate ? 0 : Math.max(this.coalesceMs, earliestByInterval - now);
    this.timer = this.clock.setTimeout(() => {
      this.timer = null;
      this.start();
    }, delayMs);
  }

  private start() {
    if (this.disposed || this.active) {
      return;
    }
    this.active = true;
    this.lastStartedAt = this.clock.now();
    const controller = new AbortController();
    this.activeController = controller;
    const timeoutError = new RefreshRequestTimeoutError(this.requestTimeoutMs);
    const timeout = new Promise<never>((_, reject) => {
      this.timeoutTimer = this.clock.setTimeout(() => {
        this.timeoutTimer = null;
        controller.abort(timeoutError);
        reject(timeoutError);
      }, this.requestTimeoutMs);
    });

    void Promise.race([this.request(controller.signal), timeout])
      .catch(() => undefined)
      .finally(() => {
        if (this.timeoutTimer !== null) {
          this.clock.clearTimeout(this.timeoutTimer);
          this.timeoutTimer = null;
        }
        if (this.activeController === controller) {
          this.activeController = null;
        }
        this.active = false;
        if (!this.disposed && this.dirty) {
          this.dirty = false;
          this.schedule(false);
        }
      });
  }
}

interface GuardedRefreshOptions<T> {
  signal: AbortSignal;
  isCurrent(): boolean;
  load(signal: AbortSignal): Promise<T>;
  enqueueWrite(write: () => void): void;
  onLoading(loading: boolean): void;
  onSuccess(value: T): void;
  onError(error: unknown): void;
  isUnauthorized(error: unknown): boolean;
  onUnauthorized(error: unknown): void;
}

export async function runGuardedRefresh<T>({
  signal,
  isCurrent,
  load,
  enqueueWrite,
  onLoading,
  onSuccess,
  onError,
  isUnauthorized,
  onUnauthorized,
}: GuardedRefreshOptions<T>) {
  if (!isCurrent()) {
    return;
  }
  onLoading(true);
  onError(null);

  let removeAbortListener: () => void = () => undefined;
  const aborted = new Promise<never>((_, reject) => {
    if (signal.aborted) {
      reject(signal.reason);
      return;
    }
    const handleAbort = () => reject(signal.reason);
    signal.addEventListener("abort", handleAbort, { once: true });
    removeAbortListener = () => signal.removeEventListener("abort", handleAbort);
  });

  try {
    const value = await Promise.race([load(signal), aborted]);
    if (!isCurrent() || signal.aborted) {
      return;
    }
    enqueueWrite(() => {
      if (isCurrent()) {
        onSuccess(value);
      }
    });
  } catch (error) {
    if (!isCurrent()) {
      return;
    }
    if (isUnauthorized(error)) {
      onUnauthorized(error);
      return;
    }
    if (signal.aborted && !(signal.reason instanceof RefreshRequestTimeoutError)) {
      return;
    }
    onError(signal.reason instanceof RefreshRequestTimeoutError ? signal.reason : error);
  } finally {
    removeAbortListener();
    if (isCurrent()) {
      onLoading(false);
    }
  }
}

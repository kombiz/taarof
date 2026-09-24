export interface EventRecord {
  seq: number;
  ts_unix_ms: number;
  event_type: string;
  payload: unknown;
}

export interface EventPage {
  schema: string;
  since_seq: number | null;
  limit: number;
  next_seq: number;
  high_watermark: number;
  oldest_seq: number | null;
  gap: boolean;
  gap_from: number | null;
  gap_to: number | null;
  resnapshot_required: boolean;
  capacity: number;
  dropped: number;
  last_dropped_at_unix_ms: number | null;
  events: EventRecord[];
}

export interface EventSocket {
  addEventListener(type: string, listener: (event: unknown) => void): void;
  removeEventListener(type: string, listener: (event: unknown) => void): void;
  close(): void;
}

export interface EventConnectionStatus {
  connection: "connecting" | "recovering" | "connected" | "disconnected";
  freshness: "stale" | "verified";
  cursor: number | null;
  last_verified_at_unix_ms: number | null;
  attempt: number;
}

interface EventRecoveryClock {
  now(): number;
  setTimeout(callback: () => void, delayMs: number): number;
  clearTimeout(timer: number): void;
}

interface EventRecoveryOptions {
  createSocket(): EventSocket;
  fetchRuntimeIdentity(signal: AbortSignal): Promise<string>;
  fetchEvents(cursor: number, signal: AbortSignal): Promise<EventPage>;
  recoverSnapshot(signal: AbortSignal): Promise<void>;
  onEvent(data: string, source: "live" | "recovery"): void;
  onRecoveryBoundary(): void;
  onRuntimeReset(): void;
  onUnauthorized(): void;
  onStatus(status: EventConnectionStatus): void;
  isUnauthorized(error: unknown): boolean;
  clock?: EventRecoveryClock;
  random?: () => number;
  requestTimeoutMs?: number;
}

const EVENT_RECONNECT_BASE_MS = 500;
const EVENT_RECONNECT_CAP_MS = 10_000;
const EVENT_RECOVERY_TIMEOUT_MS = 10_000;
const EVENT_RECOVERY_MAX_ROUNDS = 8;
const EVENT_REPLAY_MAX_PAGES = 100;

const systemClock: EventRecoveryClock = {
  now: () => Date.now(),
  setTimeout: (callback, delayMs) => globalThis.setTimeout(callback, delayMs),
  clearTimeout: (timer) => globalThis.clearTimeout(timer),
};

export function nextEventReconnectDelayMs(attempt: number, random: number): number {
  const boundedAttempt = Math.max(0, Math.min(attempt, 30));
  const base = Math.min(EVENT_RECONNECT_BASE_MS * 2 ** boundedAttempt, EVENT_RECONNECT_CAP_MS);
  const jitter = 0.8 + Math.max(0, Math.min(random, 1)) * 0.4;
  return Math.min(Math.round(base * jitter), EVENT_RECONNECT_CAP_MS);
}

function eventSequence(data: string): number | null {
  try {
    const event = JSON.parse(data) as { seq?: unknown };
    return typeof event.seq === "number" && Number.isSafeInteger(event.seq) && event.seq > 0
      ? event.seq
      : null;
  } catch {
    return null;
  }
}

function isLaggedFrame(data: string): boolean {
  try {
    const event = JSON.parse(data) as { event_type?: unknown; resnapshot_required?: unknown };
    return event.event_type === "_lagged" || event.resnapshot_required === true;
  } catch {
    return false;
  }
}

export class EventRecoveryController {
  private readonly clock: EventRecoveryClock;
  private readonly random: () => number;
  private readonly requestTimeoutMs: number;
  private active = false;
  private socket: EventSocket | null = null;
  private reconnectTimer: number | null = null;
  private recoveryController: AbortController | null = null;
  private operation = 0;
  private attempt = 0;
  private runtimeId: string | null = null;
  private cursor: number | null = null;
  private recovering = false;
  private bufferedMessages: string[] = [];
  private lastVerifiedAt: number | null = null;
  private connection: EventConnectionStatus["connection"] = "disconnected";
  private freshness: EventConnectionStatus["freshness"] = "stale";

  constructor(private readonly options: EventRecoveryOptions) {
    this.clock = options.clock ?? systemClock;
    this.random = options.random ?? Math.random;
    this.requestTimeoutMs = options.requestTimeoutMs ?? EVENT_RECOVERY_TIMEOUT_MS;
  }

  start() {
    if (this.active) return;
    this.active = true;
    this.connect();
  }

  stop() {
    if (!this.active) return;
    this.active = false;
    this.operation += 1;
    this.recovering = false;
    this.bufferedMessages = [];
    this.clearReconnectTimer();
    this.recoveryController?.abort(new DOMException("Event recovery stopped.", "AbortError"));
    this.recoveryController = null;
    const socket = this.socket;
    this.socket = null;
    socket?.close();
    this.connection = "disconnected";
    this.freshness = "stale";
    this.publishStatus();
  }

  markSnapshotVerified() {
    if (!this.active || this.connection !== "connected" || this.recovering) return;
    this.lastVerifiedAt = this.clock.now();
    this.freshness = "verified";
    this.publishStatus();
  }

  private connect() {
    if (!this.active) return;
    this.clearReconnectTimer();
    this.connection = "connecting";
    this.freshness = "stale";
    this.publishStatus();

    let socket: EventSocket;
    try {
      socket = this.options.createSocket();
    } catch {
      this.connection = "disconnected";
      this.publishStatus();
      this.scheduleReconnect();
      return;
    }
    this.socket = socket;

    const onOpen = () => {
      if (!this.active || this.socket !== socket) return;
      void this.beginRecovery();
    };
    const onMessage = (rawEvent: unknown) => {
      if (!this.active || this.socket !== socket) return;
      const data = (rawEvent as { data?: unknown }).data;
      if (typeof data !== "string") {
        this.handleLiveMessage("");
        return;
      }
      this.handleLiveMessage(data);
    };
    const onClose = () => {
      if (!this.active || this.socket !== socket) return;
      detach();
      this.socket = null;
      this.operation += 1;
      this.recovering = false;
      this.recoveryController?.abort(new DOMException("Event socket closed.", "AbortError"));
      this.recoveryController = null;
      this.options.onRecoveryBoundary();
      this.connection = "disconnected";
      this.freshness = "stale";
      this.publishStatus();
      void this.probeAuthorizationThenReconnect();
    };
    const onError = () => undefined;
    const detach = () => {
      socket.removeEventListener("open", onOpen);
      socket.removeEventListener("message", onMessage);
      socket.removeEventListener("close", onClose);
      socket.removeEventListener("error", onError);
    };
    socket.addEventListener("open", onOpen);
    socket.addEventListener("message", onMessage);
    socket.addEventListener("close", onClose);
    socket.addEventListener("error", onError);
  }

  private async probeAuthorizationThenReconnect() {
    const operation = this.operation;
    const controller = new AbortController();
    this.recoveryController = controller;
    try {
      await this.withTimeout(
        (signal) => this.options.fetchRuntimeIdentity(signal),
        controller,
      );
    } catch (error) {
      if (!this.isCurrent(operation, controller)) return;
      if (this.options.isUnauthorized(error)) {
        this.stopForUnauthorized();
        return;
      }
    } finally {
      if (this.recoveryController === controller) this.recoveryController = null;
    }
    if (this.active && operation === this.operation) this.scheduleReconnect();
  }

  private scheduleReconnect() {
    if (!this.active || this.reconnectTimer !== null) return;
    const delay = nextEventReconnectDelayMs(this.attempt, this.random());
    this.attempt += 1;
    this.publishStatus();
    this.reconnectTimer = this.clock.setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, delay);
  }

  private clearReconnectTimer() {
    if (this.reconnectTimer === null) return;
    this.clock.clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
  }

  private async beginRecovery() {
    if (!this.active || this.recovering) return;
    this.recovering = true;
    const operation = ++this.operation;
    this.recoveryController?.abort(new DOMException("Recovery replaced.", "AbortError"));
    const controller = new AbortController();
    this.recoveryController = controller;
    this.connection = "recovering";
    this.freshness = "stale";
    this.options.onRecoveryBoundary();
    this.publishStatus();

    try {
      const verified = await this.withTimeout((signal) => this.recover(signal), controller);
      if (!this.isCurrent(operation, controller)) return;
      this.attempt = 0;
      this.connection = "connected";
      this.freshness = verified ? "verified" : "stale";
      if (verified) this.lastVerifiedAt = this.clock.now();
      this.publishStatus();
    } catch (error) {
      if (!this.isCurrent(operation, controller)) return;
      if (this.options.isUnauthorized(error)) {
        this.stopForUnauthorized();
        return;
      }
      this.connection = "disconnected";
      this.freshness = "stale";
      this.publishStatus();
      const socket = this.socket;
      this.socket = null;
      socket?.close();
      this.scheduleReconnect();
    } finally {
      if (this.operation === operation) this.recovering = false;
      if (this.recoveryController === controller) this.recoveryController = null;
    }
  }

  private async recover(signal: AbortSignal) {
    const runtimeId = await this.options.fetchRuntimeIdentity(signal);
    signal.throwIfAborted();
    this.observeRuntime(runtimeId);

    let replayRequired = true;
    let closingSnapshot = false;
    for (let round = 0; round < EVENT_RECOVERY_MAX_ROUNDS; round += 1) {
      let snapshotFloor = this.cursor ?? 0;
      if (replayRequired) snapshotFloor = await this.replayPages(signal);
      signal.throwIfAborted();
      await this.options.recoverSnapshot(signal);
      signal.throwIfAborted();
      this.cursor = Math.max(this.cursor ?? 0, snapshotFloor);

      const verifiedRuntimeId = await this.options.fetchRuntimeIdentity(signal);
      signal.throwIfAborted();
      if (this.runtimeId !== verifiedRuntimeId) {
        this.observeRuntime(verifiedRuntimeId);
        replayRequired = true;
        closingSnapshot = false;
        continue;
      }

      const buffered = this.bufferedMessages;
      this.bufferedMessages = [];
      if (buffered.length === 0) return true;

      replayRequired = false;
      const pending: Array<{ data: string; seq: number | null }> = [];
      let scanCursor = this.cursor ?? 0;
      for (const data of buffered) {
        if (isLaggedFrame(data)) {
          replayRequired = true;
          continue;
        }
        const seq = eventSequence(data);
        if (seq === null) {
          pending.push({ data, seq });
          continue;
        }
        if (seq <= scanCursor) continue;
        if (seq > scanCursor + 1) {
          this.bufferedMessages.push(data);
          replayRequired = true;
          continue;
        }
        pending.push({ data, seq });
        scanCursor = seq;
      }

      const source = closingSnapshot && !replayRequired ? "live" : "recovery";
      for (const item of pending) {
        this.options.onEvent(item.data, source);
        if (item.seq !== null) this.cursor = item.seq;
      }

      if (replayRequired) {
        closingSnapshot = false;
        continue;
      }
      if (pending.length === 0) return true;
      if (closingSnapshot) return false;
      closingSnapshot = true;
    }
    throw new Error("Event recovery could not reach a stable snapshot.");
  }

  private async replayPages(signal: AbortSignal) {
    let cursor = this.cursor ?? 0;
    let snapshotFloor = cursor;
    for (let pageNumber = 0; pageNumber < EVENT_REPLAY_MAX_PAGES; pageNumber += 1) {
      const page = await this.options.fetchEvents(cursor, signal);
      signal.throwIfAborted();
      if (page.high_watermark < cursor) {
        this.resetRuntimeCursor();
        cursor = 0;
        snapshotFloor = 0;
        continue;
      }
      snapshotFloor = Math.max(snapshotFloor, page.high_watermark);
      for (const record of page.events) {
        if (!Number.isSafeInteger(record.seq) || record.seq <= cursor) continue;
        this.options.onEvent(JSON.stringify(record), "recovery");
        cursor = record.seq;
        this.cursor = cursor;
      }
      if (cursor >= page.high_watermark || page.events.length === 0) return snapshotFloor;
    }
    throw new Error("Event replay exceeded its page bound.");
  }

  private handleLiveMessage(data: string) {
    if (this.recovering) {
      this.bufferedMessages.push(data);
      return;
    }
    if (isLaggedFrame(data)) {
      void this.beginRecovery();
      return;
    }
    const seq = eventSequence(data);
    if (seq === null) {
      this.options.onEvent(data, "live");
      this.freshness = "stale";
      this.publishStatus();
      return;
    }
    if (this.cursor === null || seq > this.cursor + 1) {
      this.bufferedMessages.push(data);
      void this.beginRecovery();
      return;
    }
    if (seq <= this.cursor) return;
    this.options.onEvent(data, "live");
    this.cursor = seq;
    this.freshness = "stale";
    this.publishStatus();
  }

  private resetRuntimeCursor() {
    this.cursor = 0;
    this.bufferedMessages = [];
    this.options.onRuntimeReset();
  }

  private observeRuntime(runtimeId: string) {
    if (this.runtimeId !== null && this.runtimeId !== runtimeId) this.resetRuntimeCursor();
    this.runtimeId = runtimeId;
  }

  private stopForUnauthorized() {
    if (!this.active) return;
    this.active = false;
    this.operation += 1;
    this.recovering = false;
    this.bufferedMessages = [];
    this.clearReconnectTimer();
    this.recoveryController?.abort(new DOMException("Authorization rejected.", "AbortError"));
    this.recoveryController = null;
    const socket = this.socket;
    this.socket = null;
    socket?.close();
    this.connection = "disconnected";
    this.freshness = "stale";
    this.publishStatus();
    this.options.onUnauthorized();
  }

  private isCurrent(operation: number, controller: AbortController) {
    return this.active && this.operation === operation && this.recoveryController === controller;
  }

  private async withTimeout<T>(
    operation: (signal: AbortSignal) => Promise<T>,
    controller: AbortController,
  ): Promise<T> {
    let timer: number | null = null;
    const timeout = new Promise<never>((_, reject) => {
      timer = this.clock.setTimeout(() => {
        timer = null;
        const error = new Error(`Event recovery timed out after ${this.requestTimeoutMs} ms.`);
        controller.abort(error);
        reject(error);
      }, this.requestTimeoutMs);
    });
    try {
      return await Promise.race([operation(controller.signal), timeout]);
    } finally {
      if (timer !== null) this.clock.clearTimeout(timer);
    }
  }

  private publishStatus() {
    this.options.onStatus({
      connection: this.connection,
      freshness: this.freshness,
      cursor: this.cursor,
      last_verified_at_unix_ms: this.lastVerifiedAt,
      attempt: this.attempt,
    });
  }
}

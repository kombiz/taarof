export interface PaneTerminalTransportInput {
  hasTerminalHost: boolean;
  hasTab: boolean;
  hasPane: boolean;
  attachSupported: boolean;
  usePtyTransport: boolean;
  isSelected: boolean;
  isControlUnlocked: boolean;
  hasControlTarget: boolean;
}

/**
 * Legacy panes keep their observe stream for the lifetime of the mounted tile.
 * Control is a second, selected-only stream; it must never replace observation.
 */
export function planPaneTerminalTransport(input: PaneTerminalTransportInput) {
  const observeEnabled =
    input.hasTerminalHost &&
    input.hasTab &&
    input.hasPane &&
    input.attachSupported &&
    !input.usePtyTransport;
  const controlEnabled =
    input.isSelected &&
    input.isControlUnlocked &&
    input.hasControlTarget &&
    input.attachSupported &&
    !input.usePtyTransport;
  return { observeEnabled, controlEnabled };
}

export interface TerminalResetIdentity {
  key: string;
  host: unknown;
}

export function paneTerminalIdentity(tabId: number | null, paneId: number | null) {
  return `${tabId ?? "none"}:${paneId ?? "none"}`;
}

export function shouldResetPaneTerminal(
  previous: TerminalResetIdentity | null,
  current: TerminalResetIdentity,
  isUnsupportedPane: boolean,
) {
  return (
    previous === null ||
    previous.key !== current.key ||
    previous.host !== current.host ||
    isUnsupportedPane
  );
}

export function paneTileBoundaryNeedsReset(
  previous: { paneId: number; refreshGeneration: number },
  current: { paneId: number; refreshGeneration: number },
) {
  return (
    previous.paneId !== current.paneId ||
    previous.refreshGeneration !== current.refreshGeneration
  );
}

type TimerApi = {
  setTimeout: (callback: () => void, delay: number) => number;
  clearTimeout: (timer: number) => void;
};

const browserTimers: TimerApi = {
  setTimeout: (callback, delay) => window.setTimeout(callback, delay),
  clearTimeout: (timer) => window.clearTimeout(timer),
};

/** Owns cancelable tile-local requests and delayed callbacks. */
export class PaneTileWork {
  private readonly requests = new Map<string, AbortController>();
  private readonly timers = new Set<number>();

  constructor(private readonly timersApi: TimerApi = browserTimers) {}

  beginRequest(name: string) {
    this.requests.get(name)?.abort();
    const controller = new AbortController();
    this.requests.set(name, controller);
    return controller;
  }

  completeRequest(name: string, controller: AbortController) {
    if (this.requests.get(name) === controller) {
      this.requests.delete(name);
    }
  }

  schedule(callback: () => void, delay: number) {
    let timer: number | undefined;
    timer = this.timersApi.setTimeout(() => {
      if (timer !== undefined) {
        this.timers.delete(timer);
      }
      callback();
    }, delay);
    this.timers.add(timer);
    return timer;
  }

  cancelTimer(timer: number | undefined) {
    if (timer !== undefined) {
      this.timersApi.clearTimeout(timer);
      this.timers.delete(timer);
    }
  }

  cancel() {
    for (const controller of this.requests.values()) {
      controller.abort();
    }
    this.requests.clear();
    for (const timer of this.timers) {
      this.timersApi.clearTimeout(timer);
    }
    this.timers.clear();
  }
}

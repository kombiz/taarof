import { useEffect, useRef, useState } from "react";
import {
  appendPaneTextObservation,
  compactJournal,
  hashText,
  loadPaneJournal,
  savePaneJournal,
  type PaneIdentity,
  type PaneJournal,
  type PaneJournalStorage,
  type PaneObservationInput,
} from "./monitorPaneJournal.js";

export interface PaneJournalFrame extends PaneObservationInput {
  sequence: number;
}

export interface PaneJournalScheduler {
  setTimeout(callback: () => void, delayMs: number): unknown;
  clearTimeout(handle: unknown): void;
}

export interface PaneJournalControllerOptions {
  storage: PaneJournalStorage;
  identity: PaneIdentity | null;
  onJournal?: (journal: PaneJournal) => void;
  scheduler?: PaneJournalScheduler;
  throttleMs?: number;
}

export interface PaneJournalFlushOptions {
  notify?: boolean;
}

export interface PaneJournalController {
  updateIdentity(identity: PaneIdentity): void;
  observe(frame: PaneObservationInput): void;
  flush(options?: PaneJournalFlushOptions): PaneJournal | null;
  cancel(): void;
  pendingCount(): number;
}

export interface UsePaneJournalOptions {
  throttleMs?: number;
}

const DEFAULT_PANE_JOURNAL_THROTTLE_MS = 250;

function defaultScheduler(): PaneJournalScheduler {
  return {
    setTimeout: (callback, delayMs) => window.setTimeout(callback, delayMs),
    clearTimeout: (handle) => window.clearTimeout(handle as number),
  };
}

function computeFrameSignature(frame: PaneObservationInput): string {
  const text = frame.text.trimEnd();
  return `${hashText(text)}:${text.length}`;
}

function isConsecutiveDuplicate(
  left: PaneObservationInput,
  right: PaneObservationInput,
): boolean {
  return computeFrameSignature(left) === computeFrameSignature(right);
}

export function createPaneJournalController({
  storage,
  identity,
  onJournal,
  scheduler = defaultScheduler(),
  throttleMs = DEFAULT_PANE_JOURNAL_THROTTLE_MS,
}: PaneJournalControllerOptions): PaneJournalController {
  if (identity === null) {
    return {
      updateIdentity: () => {},
      observe: () => {},
      flush: () => null,
      cancel: () => {},
      pendingCount: () => 0,
    };
  }
  // Keep the narrowed identity separate from the nullable input captured by closures.
  let paneIdentity = identity;
  const pending: PaneObservationInput[] = [];
  let timer: unknown = null;
  let journal: PaneJournal | null = null;
  let cancelled = false;

  function clearTimer() {
    if (timer !== null) {
      scheduler.clearTimeout(timer);
      timer = null;
    }
  }

  function scheduleFlush() {
    if (timer !== null) {
      return;
    }
    timer = scheduler.setTimeout(() => {
      timer = null;
      flush();
    }, throttleMs);
  }

  function observe(frame: PaneObservationInput) {
    if (cancelled) return;
    const previous = pending[pending.length - 1];
    if (!previous || !isConsecutiveDuplicate(previous, frame)) {
      pending.push(frame);
    }
    scheduleFlush();
  }

  function flush(options: PaneJournalFlushOptions = {}): PaneJournal | null {
    if (cancelled) return journal;
    const { notify = true } = options;
    clearTimer();
    if (pending.length === 0) {
      if (journal) {
        journal = compactJournal(paneIdentity.paneKey, journal.entries, Date.now());
        savePaneJournal(storage, journal);
        if (notify) onJournal?.(journal);
      }
      return journal;
    }

    const frames = pending.splice(0, pending.length);
    for (const frame of frames) {
      journal = appendPaneTextObservation(storage, paneIdentity, frame, journal ?? undefined).journal;
    }

    if (journal && notify) {
      onJournal?.(journal);
    }
    return journal;
  }

  function cancel() {
    cancelled = true;
    clearTimer();
    pending.splice(0, pending.length);
  }

  return {
    updateIdentity: (nextIdentity) => {
      if (nextIdentity.paneKey !== paneIdentity.paneKey) {
        throw new Error("A pane journal controller cannot change pane keys");
      }
      paneIdentity = nextIdentity;
    },
    observe,
    flush,
    cancel,
    pendingCount: () => pending.length,
  };
}

export function usePaneJournal(
  storage: PaneJournalStorage,
  identity: PaneIdentity | null,
  latestFrame: PaneJournalFrame | null,
  options: UsePaneJournalOptions = {},
): PaneJournal {
  const [journal, setJournal] = useState<PaneJournal>(() =>
    identity ? loadPaneJournal(storage, identity.paneKey) : compactJournal("", [], Date.now()),
  );
  const controllerRef = useRef<PaneJournalController | null>(null);
  const throttleMs = options.throttleMs ?? DEFAULT_PANE_JOURNAL_THROTTLE_MS;

  useEffect(() => {
    const controller = createPaneJournalController({
      storage,
      identity,
      onJournal: setJournal,
      throttleMs,
    });
    controllerRef.current = controller;
    setJournal(identity ? loadPaneJournal(storage, identity.paneKey) : compactJournal("", [], Date.now()));

    return () => {
      controller.flush({ notify: false });
      controller.cancel();
      if (controllerRef.current === controller) {
        controllerRef.current = null;
      }
    };
  }, [identity?.paneKey, storage, throttleMs]);

  useEffect(() => {
    if (identity) controllerRef.current?.updateIdentity(identity);
  }, [identity]);

  useEffect(() => {
    if (latestFrame) {
      controllerRef.current?.observe(latestFrame);
    }
  }, [latestFrame]);

  return journal;
}

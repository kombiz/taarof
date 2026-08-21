import { useEffect, useRef, useState } from "react";
import {
  appendPaneTextObservation,
  hashText,
  loadPaneJournal,
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
  identity: PaneIdentity;
  onJournal?: (journal: PaneJournal) => void;
  scheduler?: PaneJournalScheduler;
  throttleMs?: number;
}

export interface PaneJournalFlushOptions {
  notify?: boolean;
}

export interface PaneJournalController {
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
  const pending: PaneObservationInput[] = [];
  let timer: unknown = null;

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
    const previous = pending[pending.length - 1];
    if (!previous || !isConsecutiveDuplicate(previous, frame)) {
      pending.push(frame);
    }
    scheduleFlush();
  }

  function flush(options: PaneJournalFlushOptions = {}): PaneJournal | null {
    const { notify = true } = options;
    clearTimer();
    if (pending.length === 0) {
      return null;
    }

    const frames = pending.splice(0, pending.length);
    let journal: PaneJournal | null = null;
    for (const frame of frames) {
      journal = appendPaneTextObservation(storage, identity, frame).journal;
    }

    if (journal && notify) {
      onJournal?.(journal);
    }
    return journal;
  }

  function cancel() {
    clearTimer();
    pending.splice(0, pending.length);
  }

  return {
    observe,
    flush,
    cancel,
    pendingCount: () => pending.length,
  };
}

export function usePaneJournal(
  storage: PaneJournalStorage,
  identity: PaneIdentity,
  latestFrame: PaneJournalFrame | null,
  options: UsePaneJournalOptions = {},
): PaneJournal {
  const [journal, setJournal] = useState<PaneJournal>(() =>
    loadPaneJournal(storage, identity.paneKey),
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
    setJournal(loadPaneJournal(storage, identity.paneKey));

    return () => {
      controller.flush({ notify: false });
      controller.cancel();
      if (controllerRef.current === controller) {
        controllerRef.current = null;
      }
    };
  }, [identity, storage, throttleMs]);

  useEffect(() => {
    if (latestFrame) {
      controllerRef.current?.observe(latestFrame);
    }
  }, [latestFrame]);

  return journal;
}

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
  attributionGeneration?: number;
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
  setEnabled(enabled: boolean): void;
  currentJournal(): PaneJournal | null;
  updateIdentity(identity: PaneIdentity): void;
  observe(frame: PaneObservationInput): void;
  flush(options?: PaneJournalFlushOptions): PaneJournal | null;
  cancel(): void;
  pendingCount(): number;
}

export interface UsePaneJournalOptions {
  throttleMs?: number;
  controllerOwner?: PaneJournalOwner;
  generation?: number;
}

export interface PaneJournalLease {
  updateIdentity(identity: PaneIdentity): void;
  observe(frame: PaneObservationInput): void;
  currentJournal(): PaneJournal | null;
  release(): void;
}

export interface PaneJournalOwner {
  acquire(identity: PaneIdentity, onJournal: (journal: PaneJournal) => void): PaneJournalLease | null;
}

// Quarantined observations were already attributed while verified. Compact
// them in memory without touching browser storage while proof is unavailable.
const suspendedStorage: PaneJournalStorage = {
  getItem: () => null,
  setItem: () => { throw new Error("journal persistence is suspended"); },
  removeItem: () => {},
};

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
      setEnabled: () => {},
      currentJournal: () => null,
      updateIdentity: () => {},
      observe: () => {},
      flush: () => null,
      cancel: () => {},
      pendingCount: () => 0,
    };
  }
  // Keep the narrowed identity separate from the nullable input captured by closures.
  let paneIdentity = identity;
  const pending: Array<{ identity: PaneIdentity; frame: PaneObservationInput }> = [];
  let timer: unknown = null;
  let journal: PaneJournal | null = loadPaneJournal(storage, paneIdentity.paneKey);
  let cancelled = false;
  let enabled = true;

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
    if (cancelled || !enabled) return;
    const previous = pending[pending.length - 1];
    if (!previous || !isConsecutiveDuplicate(previous.frame, frame)) {
      pending.push({ identity: { ...paneIdentity }, frame });
    }
    scheduleFlush();
  }

  function flush(options: PaneJournalFlushOptions = {}): PaneJournal | null {
    if (cancelled) return journal;
    const { notify = true } = options;
    const persistence = enabled ? storage : suspendedStorage;
    clearTimer();
    if (pending.length === 0) {
      if (journal) {
        journal = compactJournal(paneIdentity.paneKey, journal.entries, Date.now());
        savePaneJournal(persistence, journal);
        if (notify) onJournal?.(journal);
      }
      return journal;
    }

    const frames = pending.splice(0, pending.length);
    for (const { identity: captureIdentity, frame } of frames) {
      journal = appendPaneTextObservation(persistence, captureIdentity, frame, journal ?? undefined).journal;
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
    setEnabled: (nextEnabled) => {
      if (cancelled || nextEnabled === enabled) return;
      enabled = nextEnabled;
      flush({ notify: nextEnabled });
    },
    currentJournal: () => journal,
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
    identity && !options.controllerOwner
      ? loadPaneJournal(storage, identity.paneKey) : compactJournal("", [], Date.now()),
  );
  const controllerRef = useRef<PaneJournalController | null>(null);
  const leaseRef = useRef<PaneJournalLease | null>(null);
  const throttleMs = options.throttleMs ?? DEFAULT_PANE_JOURNAL_THROTTLE_MS;
  const owner = options.controllerOwner;

  useEffect(() => {
    if (owner) {
      const lease = identity ? owner.acquire(identity, setJournal) : null;
      leaseRef.current = lease;
      setJournal(lease?.currentJournal() ?? compactJournal("", [], Date.now()));
      return () => {
        lease?.release();
        if (leaseRef.current === lease) leaseRef.current = null;
      };
    }
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
  }, [identity?.paneKey, storage, throttleMs, owner, options.generation]);

  useEffect(() => {
    if (identity) {
      controllerRef.current?.updateIdentity(identity);
      leaseRef.current?.updateIdentity(identity);
    }
  }, [identity]);

  useEffect(() => {
    if (latestFrame) {
      controllerRef.current?.observe(latestFrame);
      if (latestFrame.attributionGeneration === options.generation) {
        leaseRef.current?.observe(latestFrame);
      }
    }
  }, [latestFrame]);

  return journal;
}

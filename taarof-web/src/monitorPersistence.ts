import { buildPaneTargets, readStoredMonitorOrder, readStoredWatchedKeys, writeStoredMonitorOrder, writeStoredWatchedKeys, type StringArrayStorage } from "./monitorBoard.js";
import { monitorNamespace, type MonitorSnapshot } from "./monitorIdentity.js";
import type { PaneIdentity, PaneJournal, PaneJournalStorage } from "./monitorPaneJournal.js";
import { createPaneJournalController, type PaneJournalController, type PaneJournalLease, type PaneJournalOwner, type PaneJournalScheduler } from "./usePaneJournal.js";
import type { TaarofStateSnapshot } from "./types.js";

export interface MonitorPersistenceState {
  proof: MonitorSnapshot | null;
  retainedNamespace: string | null;
  generation: number;
  orderedKeys: string[];
  watchedKeys: string[];
}

/** The board lifetime may retain one known namespace; proof alone grants I/O. */
export function monitorViewIdentity(state: MonitorPersistenceState, snapshot: TaarofStateSnapshot | null) {
  const proof = state.proof?.snapshot === snapshot ? state.proof : null;
  return {
    key: state.retainedNamespace ?? "monitor-unverified",
    runtimeId: proof?.runtimeId ?? null,
    namespace: proof?.namespace ?? null,
    generation: state.generation,
  };
}

interface JournalSlot {
  controller: PaneJournalController;
  listeners: Set<(journal: PaneJournal) => void>;
}

/**
 * Quarantine one prior verified namespace during auth/recovery gaps. Journals
 * are limited to its latest verified pane set and retain their existing caps.
 * Unknown panes cannot acquire slots; replacement cancels without migration.
 */
export class MonitorPersistence implements PaneJournalOwner {
  private state: MonitorPersistenceState = {
    proof: null, retainedNamespace: null, generation: 0, orderedKeys: [], watchedKeys: [],
  };
  private listeners = new Set<() => void>();
  private journals = new Map<string, JournalSlot>();
  private paneKeys = new Set<string>();

  constructor(
    private readonly storage: PaneJournalStorage & StringArrayStorage,
    private readonly scheduler?: PaneJournalScheduler,
  ) {}

  getSnapshot = () => this.state;
  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    return () => { this.listeners.delete(listener); };
  };

  private publish(next: MonitorPersistenceState) {
    this.state = next;
    this.listeners.forEach((listener) => listener());
  }

  suspend() {
    if (this.state.proof === null) return;
    // Controller suspension is synchronous: pending timers or React unmount
    // cleanup cannot perform storage I/O after the connection boundary.
    this.journals.forEach(({ controller }) => controller.setEnabled(false));
    this.publish({ ...this.state, proof: null, generation: this.state.generation + 1 });
  }

  stopConnections(stop: () => void) {
    this.suspend();
    stop();
  }

  observeConnection(connection: string) {
    if (connection === "disconnected") this.suspend();
  }

  observeIdentity(namespace: string | null) {
    if (namespace === null || namespace !== this.state.retainedNamespace) this.suspend();
  }

  private discardJournals() {
    this.journals.forEach(({ controller }) => controller.cancel());
    this.journals.clear();
    this.paneKeys.clear();
  }

  reset() {
    this.discardJournals();
    this.publish({ proof: null, retainedNamespace: null, generation: this.state.generation + 1,
      orderedKeys: [], watchedKeys: [] });
  }

  verify(next: MonitorSnapshot) {
    if (next.namespace === null || next.runtimeId === null ||
      next.namespace !== monitorNamespace(next.runtimeId, next.snapshot.session_name)) {
      this.suspend();
      return;
    }
    const replaced = this.state.retainedNamespace !== next.namespace;
    if (replaced) this.discardJournals();
    const paneKeys = new Set(buildPaneTargets(next.snapshot, next.runtimeId).map((target) => target.key));
    for (const [key, slot] of this.journals) {
      if (!paneKeys.has(key)) {
        slot.controller.cancel();
        this.journals.delete(key);
      }
    }
    this.paneKeys = paneKeys;
    this.publish({
      proof: next,
      retainedNamespace: next.namespace,
      generation: this.state.generation + (replaced ? 1 : 0),
      orderedKeys: replaced ? readStoredMonitorOrder(this.storage, next.namespace) : this.state.orderedKeys,
      watchedKeys: replaced ? readStoredWatchedKeys(this.storage, next.namespace) : this.state.watchedKeys,
    });
    // Retry retained preference writes and bounded journal replacements only
    // after proof for this exact runtime/session and snapshot is restored.
    writeStoredMonitorOrder(this.storage, this.state.orderedKeys, next.namespace);
    writeStoredWatchedKeys(this.storage, this.state.watchedKeys, next.namespace);
    this.journals.forEach(({ controller }) => controller.setEnabled(true));
  }

  commitOrder(keys: string[], namespace: string | null) {
    if (namespace === null || this.state.proof?.namespace !== namespace) return;
    this.publish({ ...this.state, orderedKeys: [...keys] });
    writeStoredMonitorOrder(this.storage, keys, namespace);
  }

  commitWatchedKeys(keys: string[], namespace: string | null) {
    if (namespace === null || this.state.proof?.namespace !== namespace) return;
    this.publish({ ...this.state, watchedKeys: [...keys] });
    writeStoredWatchedKeys(this.storage, keys, namespace);
  }

  canAttribute(paneKey: string, generation: number) {
    return this.state.proof !== null && this.state.generation === generation && this.paneKeys.has(paneKey);
  }

  acquire(identity: PaneIdentity, onJournal: (journal: PaneJournal) => void): PaneJournalLease | null {
    if (!this.state.proof || !this.paneKeys.has(identity.paneKey)) return null;
    let slot = this.journals.get(identity.paneKey);
    if (!slot) {
      const listeners = new Set<(journal: PaneJournal) => void>();
      const controller = createPaneJournalController({ storage: this.storage, identity, scheduler: this.scheduler,
        onJournal: (journal) => listeners.forEach((listener) => listener(journal)) });
      slot = { controller, listeners };
      this.journals.set(identity.paneKey, slot);
    }
    const retained = slot;
    retained.controller.updateIdentity(identity);
    retained.listeners.add(onJournal);
    const generation = this.state.generation;
    let released = false;
    const isCurrent = () => !released && this.canAttribute(identity.paneKey, generation) &&
      this.journals.get(identity.paneKey) === retained;
    return {
      currentJournal: () => retained.controller.currentJournal(),
      updateIdentity: (next) => { if (isCurrent()) retained.controller.updateIdentity(next); },
      observe: (frame) => { if (isCurrent()) retained.controller.observe(frame); },
      release: () => {
        if (released) return;
        released = true;
        retained.listeners.delete(onJournal);
        retained.controller.flush({ notify: false });
      },
    };
  }
}

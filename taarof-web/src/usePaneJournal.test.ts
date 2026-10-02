import { appendPaneTextObservation, buildPaneIdentity, loadPaneJournal, PANE_JOURNAL_LIMITS, type PaneJournal, type PaneJournalStorage } from "./monitorPaneJournal.js";
import { createPaneJournalController, type PaneJournalScheduler } from "./usePaneJournal.js";

class MemoryStorage implements PaneJournalStorage {
  protected values = new Map<string, string>();

  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }

  setItem(key: string, value: string): void {
    this.values.set(key, value);
  }

  removeItem(key: string): void {
    this.values.delete(key);
  }
}

class CountingStorage extends MemoryStorage {
  setItemCount = 0;

  setItem(key: string, value: string): void {
    this.setItemCount += 1;
    super.setItem(key, value);
  }
}

class FailingReplacementStorage extends MemoryStorage {
  failWrites = false;

  setItem(key: string, value: string): void {
    if (this.failWrites) throw new Error("storage unavailable");
    super.setItem(key, value);
  }

  storedJson(): string {
    return Array.from(this.values.values()).join("");
  }
}

class ManualScheduler implements PaneJournalScheduler {
  private nextHandle = 1;
  private callbacks = new Map<unknown, () => void>();

  setTimeout(callback: () => void): unknown {
    const handle = this.nextHandle;
    this.nextHandle += 1;
    this.callbacks.set(handle, callback);
    return handle;
  }

  clearTimeout(handle: unknown): void {
    this.callbacks.delete(handle);
  }

  runPending(): void {
    const callbacks = Array.from(this.callbacks.values());
    this.callbacks.clear();
    callbacks.forEach((callback) => callback());
  }

  get scheduledCount(): number {
    return this.callbacks.size;
  }
}

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(message);
  }
}

function test(name: string, fn: () => void) {
  try {
    fn();
    console.log(`PASS ${name}`);
  } catch (error) {
    console.error(`FAIL ${name}`);
    throw error;
  }
}

function createTestPaneIdentity() {
  return buildPaneIdentity({
    runtimeId: "runtime-a", sessionName: "taarof web",
    workspaceId: 1,
    workspaceName: "Main",
    tabId: 2,
    tabName: "Codex",
    paneId: 0,
  });
}

test("a burst of identical frames within the throttle window produces at most one persisted observation", () => {
  const storage = new CountingStorage();
  const paneIdentity = createTestPaneIdentity();
  const scheduler = new ManualScheduler();
  const controller = createPaneJournalController({
    storage,
    identity: paneIdentity,
    scheduler,
    throttleMs: 100,
  });

  controller.observe({ source: "snapshot", text: "same", cols: 80, rows: 24, seenAtUnixMs: 1000 });
  controller.observe({ source: "replace", text: "same", cols: 80, rows: 24, seenAtUnixMs: 1001 });
  controller.observe({ source: "replace", text: "same\n", cols: 80, rows: 24, seenAtUnixMs: 1002 });

  assert(scheduler.scheduledCount === 1, "the burst should share one scheduled flush");
  assert(storage.setItemCount === 0, "frames should not persist synchronously from observe");

  scheduler.runPending();
  const journal = loadPaneJournal(storage, paneIdentity.paneKey, 2000);

  assert(journal.entries.length === 1, "consecutive identical frames should coalesce to one observation");
  assert(journal.entries[0].text === "same", "the persisted text should match the normalized frame text");
  assert(Number(storage.setItemCount) === 1, "the identical burst should produce one storage write");
});

test("distinct frames are eventually all persisted in order", () => {
  const storage = new CountingStorage();
  const paneIdentity = createTestPaneIdentity();
  const scheduler = new ManualScheduler();
  const controller = createPaneJournalController({
    storage,
    identity: paneIdentity,
    scheduler,
    throttleMs: 100,
  });

  controller.observe({ source: "snapshot", text: "one", cols: 80, rows: 24, seenAtUnixMs: 1000 });
  controller.observe({ source: "replace", text: "two", cols: 80, rows: 24, seenAtUnixMs: 1001 });
  controller.observe({ source: "raw", text: "three", seenAtUnixMs: 1002 });

  assert(storage.setItemCount === 0, "distinct frames should wait for the throttled flush");

  scheduler.runPending();
  const journal = loadPaneJournal(storage, paneIdentity.paneKey, 2000);

  assert(journal.entries.length === 3, "all distinct frames should be persisted");
  assert(
    journal.entries.map((entry) => entry.text).join(",") === "one,two,three",
    "distinct frames should retain arrival order",
  );
  assert(
    journal.entries.map((entry) => entry.source).join(",") === "snapshot,replace,raw",
    "distinct frames should retain sources",
  );
});

test("failed replacements preserve stored history and successive flushes retain observations for retry", () => {
  const storage = new FailingReplacementStorage();
  const identity = createTestPaneIdentity();
  const now = Date.now();
  appendPaneTextObservation(storage, identity, { source: "snapshot", text: "seed", seenAtUnixMs: now });
  const original = storage.storedJson();
  storage.failWrites = true;
  const controller = createPaneJournalController({ storage, identity, scheduler: new ManualScheduler() });

  for (const [index, text] of ["first", "second"].entries()) {
    controller.observe({ source: "raw", text, seenAtUnixMs: now + index + 1 });
    const journal = controller.flush();
    assert(storage.storedJson() === original, "failed replacement must preserve the exact previous JSON");
    assert(journal?.entries.map((entry) => entry.text).join(",") === ["seed", "first", "second"].slice(0, index + 2).join(","), "each flush must retain preceding unsaved observations");
  }

  storage.failWrites = false;
  controller.flush();
  const recovered = loadPaneJournal(storage, identity.paneKey, now + 3);
  assert(recovered.entries.map((entry) => entry.text).join(",") === "seed,first,second", "an empty flush should retry all retained observations");
});

test("unpersisted journals remain bounded by entry, byte and age limits", () => {
  const storage = new FailingReplacementStorage();
  storage.failWrites = true;
  const identity = createTestPaneIdentity();
  const now = Date.now();
  const controller = createPaneJournalController({ storage, identity, scheduler: new ManualScheduler() });
  let journal: PaneJournal | null = null;
  for (let index = 0; index < 30; index += 1) {
    controller.observe({ source: "raw", text: String(index), seenAtUnixMs: now + index });
    journal = controller.flush();
  }
  assert(journal?.entries.length === PANE_JOURNAL_LIMITS.maxEntries, "failed writes must retain only the entry cap");
  assert(journal.entries[0].text === "5", "the most recent entries should survive compaction");
  for (let index = 0; index < 3; index += 1) {
    controller.observe({ source: "raw", text: `${index}${"x".repeat(60 * 1024)}`, seenAtUnixMs: now + 31 + index });
    journal = controller.flush();
    assert(journal && journal.totalTextBytes <= PANE_JOURNAL_LIMITS.maxTextBytesPerPane, "unsaved bytes must remain capped");
  }
  controller.observe({ source: "raw", text: "fresh", seenAtUnixMs: now + PANE_JOURNAL_LIMITS.maxAgeMs + 100 });
  journal = controller.flush();
  assert(journal?.entries.length === 1 && journal.entries[0].text === "fresh", "expired unsaved observations must be removed");
  storage.failWrites = false;
  controller.flush();
  assert(loadPaneJournal(storage, identity.paneKey, now + PANE_JOURNAL_LIMITS.maxAgeMs + 101).entries.length === 1, "retry should persist the compacted journal");
});

test("cancel drops pending frames and a new identity has its own retained journal", () => {
  const storage = new FailingReplacementStorage();
  storage.failWrites = true;
  const identity = createTestPaneIdentity();
  const scheduler = new ManualScheduler();
  const controller = createPaneJournalController({ storage, identity, scheduler });
  controller.observe({ source: "raw", text: "retained", seenAtUnixMs: Date.now() });
  controller.flush();
  controller.observe({ source: "raw", text: "cancelled", seenAtUnixMs: Date.now() });
  controller.cancel();
  assert(controller.pendingCount() === 0 && scheduler.scheduledCount === 0, "cancel must discard pending work and its timer");
  storage.failWrites = false;
  assert(controller.flush()?.entries.map((entry) => entry.text).join(",") === "retained", "cancelled observations must not reach the retry");
  const next = createPaneJournalController({ storage, identity: { ...identity, paneKey: "another-pane" }, scheduler });
  next.observe({ source: "raw", text: "separate", seenAtUnixMs: Date.now() });
  assert(next.flush()?.entries.map((entry) => entry.text).join(",") === "separate", "a replacement controller must not inherit another identity's journal");
});

test("queued observations retain capture names across a same-key rename before flush", () => {
  const storage = new CountingStorage();
  const scheduler = new ManualScheduler();
  const now = Date.now();
  const identity = {
    ...createTestPaneIdentity(),
    tabNameAtCapture: "Original tab",
    workspaceNameAtCapture: "Original workspace",
    displayName: "Original display",
  };
  const controller = createPaneJournalController({ storage, identity, scheduler });
  controller.observe({ source: "raw", text: "before rename", seenAtUnixMs: now });
  assert(controller.pendingCount() === 1, "the pre-rename observation must still be queued");
  controller.updateIdentity({
    ...identity,
    tabNameAtCapture: "Renamed tab",
    workspaceNameAtCapture: "Renamed workspace",
    displayName: "Renamed display",
  });
  assert(storage.setItemCount === 0, "renaming must not bypass the scheduled flush");
  controller.flush();
  const before = loadPaneJournal(storage, identity.paneKey, now + 1);
  assert(before.entries.length === 1 && before.entries[0].text === "before rename", "the queued pre-rename text must persist");
  assert(before.entries[0].tabNameAtCapture === "Original tab", "the queued frame must retain the original tab name");
  assert(before.entries[0].workspaceNameAtCapture === "Original workspace", "the queued frame must retain the original workspace name");
  assert(before.entries[0].displayName === "Original display", "the queued frame must retain the original display name");

  controller.observe({ source: "raw", text: "after rename", seenAtUnixMs: now + 2 });
  controller.flush();
  const after = loadPaneJournal(storage, identity.paneKey, now + 3);
  assert(after.entries.map((entry) => entry.text).join(",") === "before rename,after rename", "both observations must persist in order");
  assert(after.entries[1].tabNameAtCapture === "Renamed tab", "the later frame must capture the renamed tab");
  assert(after.entries[1].workspaceNameAtCapture === "Renamed workspace", "the later frame must capture the renamed workspace");
  assert(after.entries[1].displayName === "Renamed display", "the later frame must capture the renamed display name");
});

test("same-key metadata updates preserve unsaved frames and capture new names on recovery", () => {
  const storage = new FailingReplacementStorage();
  const identity = createTestPaneIdentity();
  const now = Date.now();
  appendPaneTextObservation(storage, identity, { source: "snapshot", text: "seed", seenAtUnixMs: now });
  const original = storage.storedJson();
  const controller = createPaneJournalController({ storage, identity, scheduler: new ManualScheduler() });
  storage.failWrites = true;
  controller.observe({ source: "raw", text: "unsaved", seenAtUnixMs: now + 1 });
  controller.flush();
  const renamed = buildPaneIdentity({ runtimeId: "runtime-a", sessionName: "taarof web", workspaceId: 1, workspaceName: "Renamed workspace", tabId: 2, tabName: "Renamed tab", paneId: 0 });
  assert(renamed.paneKey === identity.paneKey, "a metadata rename must keep pane identity");
  controller.updateIdentity(renamed);
  assert(storage.storedJson() === original, "updating metadata must preserve persisted history");
  storage.failWrites = false;
  controller.observe({ source: "raw", text: "later", seenAtUnixMs: now + 2 });
  controller.flush();
  const recovered = loadPaneJournal(storage, identity.paneKey, now + 3);
  assert(recovered.entries.map((entry) => entry.text).join(",") === "seed,unsaved,later", "same-pane rename must retain unsaved frames through recovery");
  assert(recovered.entries[1].tabNameAtCapture === "Codex", "old observations must retain original metadata");
  assert(recovered.entries[2].tabNameAtCapture === "Renamed tab" && recovered.entries[2].workspaceNameAtCapture === "Renamed workspace", "new observations must capture updated metadata");
  let rejected = false;
  try {
    controller.updateIdentity({ ...renamed, paneKey: "different-pane" });
  } catch {
    rejected = true;
  }
  assert(rejected, "a distinct pane key must require a separate controller");
  assert(controller.flush()?.paneKey === identity.paneKey, "a rejected identity change must leave the retained journal isolated");
});

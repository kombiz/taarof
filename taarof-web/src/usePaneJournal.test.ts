import { buildPaneIdentity, loadPaneJournal, type PaneJournalStorage } from "./monitorPaneJournal.js";
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
    sessionName: "taarof web",
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

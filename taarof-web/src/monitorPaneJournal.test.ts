import {
  appendPaneTextObservation,
  buildPaneIdentity,
  formatPaneJournalText,
  loadPaneJournal,
  PANE_JOURNAL_LIMITS,
  pruneExpiredPaneJournals,
  type PaneJournalEnumerableStorage,
  type PaneJournalStorage,
} from "./monitorPaneJournal.js";

class MemoryStorage implements PaneJournalStorage, PaneJournalEnumerableStorage {
  protected values = new Map<string, string>();

  get length(): number {
    return this.values.size;
  }

  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }

  key(index: number): string | null {
    return Array.from(this.values.keys())[index] ?? null;
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
  removeItemCount = 0;

  setItem(key: string, value: string): void {
    this.setItemCount += 1;
    super.setItem(key, value);
  }

  removeItem(key: string): void {
    this.removeItemCount += 1;
    super.removeItem(key);
  }

  resetCounts(): void {
    this.setItemCount = 0;
    this.removeItemCount = 0;
  }
}

class FailingWriteStorage extends MemoryStorage {
  setItem(): void {
    throw new Error("storage unavailable");
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

test("pane identity includes tab name and stable unique ids", () => {
  const first = buildPaneIdentity({
    sessionName: "taarof web",
    workspaceId: 7,
    workspaceName: "Sample",
    tabId: 42,
    tabName: "KMUX",
    paneId: 0,
  });
  const second = buildPaneIdentity({
    sessionName: "taarof web",
    workspaceId: 7,
    workspaceName: "Sample",
    tabId: 43,
    tabName: "KMUX",
    paneId: 0,
  });

  assert(first.paneKey === "taarof web:7:42:0", "paneKey should include session, workspace, tab, pane");
  assert(first.displayName.startsWith("KMUX_pane-0_"), "displayName should preserve safe tab name");
  assert(first.displayName !== second.displayName, "duplicate tab names must remain distinguishable");
});

test("append dedupes repeated terminal text", () => {
  const storage = new MemoryStorage();
  const identity = buildPaneIdentity({
    sessionName: "taarof web",
    workspaceId: 1,
    workspaceName: "Main",
    tabId: 2,
    tabName: "Codex",
    paneId: 0,
  });

  const first = appendPaneTextObservation(storage, identity, {
    source: "snapshot",
    text: "one",
    cols: 80,
    rows: 24,
    seenAtUnixMs: 1000,
  });
  const duplicate = appendPaneTextObservation(storage, identity, {
    source: "replace",
    text: "one",
    cols: 80,
    rows: 24,
    seenAtUnixMs: 2000,
  });
  const changed = appendPaneTextObservation(storage, identity, {
    source: "replace",
    text: "two",
    cols: 80,
    rows: 24,
    seenAtUnixMs: 3000,
  });

  assert(first.changed, "first observation should append");
  assert(!duplicate.changed, "duplicate observation should not append");
  assert(changed.changed, "changed text should append");
  assert(changed.journal.entries.length === 2, "journal should contain only two unique observations");
  assert(
    !Object.prototype.hasOwnProperty.call(changed.journal.entries[0], "isLatest"),
    "older observation should not be stamped with isLatest",
  );
  assert(
    !Object.prototype.hasOwnProperty.call(changed.journal.entries[1], "isLatest"),
    "newer observation should not be stamped with isLatest",
  );
});

test("stored journals with legacy isLatest fields still load", () => {
  const storage = new MemoryStorage();
  const identity = buildPaneIdentity({
    sessionName: "taarof web",
    workspaceId: 1,
    workspaceName: "Main",
    tabId: 2,
    tabName: "Codex",
    paneId: 0,
  });
  const key = `taarof.web.paneJournal.v1.${encodeURIComponent(identity.paneKey)}`;

  storage.setItem(
    key,
    `{"paneKey":"${identity.paneKey}","entries":[{"paneKey":"${identity.paneKey}","displayName":"Codex_pane-0_test","tabNameAtCapture":"Codex","workspaceNameAtCapture":"Main","workspaceId":1,"tabId":2,"paneId":0,"seenAtUnixMs":1000,"source":"snapshot","text":"one","textHash":"00000000","isLatest":true}],"totalTextBytes":3}`,
  );

  const journal = loadPaneJournal(storage, identity.paneKey, 1000);

  assert(journal.entries.length === 1, "legacy journal should load");
  assert(journal.entries[0].text === "one", "legacy text should be preserved");
  assert(
    !Object.prototype.hasOwnProperty.call(journal.entries[0], "isLatest"),
    "legacy isLatest should be ignored on load",
  );
});

test("loadPaneJournal does not write storage for a valid journal", () => {
  const storage = new CountingStorage();
  const identity = buildPaneIdentity({
    sessionName: "taarof web",
    workspaceId: 1,
    workspaceName: "Main",
    tabId: 2,
    tabName: "Codex",
    paneId: 0,
  });

  appendPaneTextObservation(storage, identity, {
    source: "snapshot",
    text: "one",
    seenAtUnixMs: 1000,
  });
  storage.resetCounts();

  const journal = loadPaneJournal(storage, identity.paneKey, 2000);

  assert(journal.entries.length === 1, "valid journal should load");
  assert(storage.setItemCount === 0, "valid load should not rewrite storage");
  assert(storage.removeItemCount === 0, "valid load should not clear storage");
});

test("append on an unchanged frame performs no storage write", () => {
  const storage = new CountingStorage();
  const identity = buildPaneIdentity({
    sessionName: "taarof web",
    workspaceId: 1,
    workspaceName: "Main",
    tabId: 2,
    tabName: "Codex",
    paneId: 0,
  });

  appendPaneTextObservation(storage, identity, {
    source: "snapshot",
    text: "one",
    seenAtUnixMs: 1000,
  });
  storage.resetCounts();

  const duplicate = appendPaneTextObservation(storage, identity, {
    source: "replace",
    text: "one",
    seenAtUnixMs: 2000,
  });

  assert(!duplicate.changed, "unchanged frame should report changed=false");
  assert(storage.setItemCount === 0, "unchanged frame should not rewrite storage");
  assert(storage.removeItemCount === 0, "unchanged frame should not clear storage");
});

test("append on a changed frame writes exactly once", () => {
  const storage = new CountingStorage();
  const identity = buildPaneIdentity({
    sessionName: "taarof web",
    workspaceId: 1,
    workspaceName: "Main",
    tabId: 2,
    tabName: "Codex",
    paneId: 0,
  });

  appendPaneTextObservation(storage, identity, {
    source: "snapshot",
    text: "one",
    seenAtUnixMs: 1000,
  });
  storage.resetCounts();

  const changed = appendPaneTextObservation(storage, identity, {
    source: "replace",
    text: "two",
    seenAtUnixMs: 2000,
  });

  assert(changed.changed, "changed frame should append");
  assert(storage.setItemCount === 1, "changed frame should persist exactly once");
  assert(storage.removeItemCount === 0, "changed frame should not clear storage");
});

test("append tolerates storage write failures", () => {
  const storage = new FailingWriteStorage();
  const identity = buildPaneIdentity({
    sessionName: "taarof web",
    workspaceId: 1,
    workspaceName: "Main",
    tabId: 2,
    tabName: "Codex",
    paneId: 0,
  });

  const result = appendPaneTextObservation(storage, identity, {
    source: "snapshot",
    text: "one",
    seenAtUnixMs: 1000,
  });

  assert(result.changed, "write failure should still report the in-memory observation");
  assert(result.journal.entries.length === 1, "write failure should return the unsaved journal");
});

test("retention enforces age, entry, and text caps", () => {
  const storage = new MemoryStorage();
  const identity = buildPaneIdentity({
    sessionName: "taarof web",
    workspaceId: 1,
    workspaceName: "Main",
    tabId: 2,
    tabName: "Codex",
    paneId: 0,
  });

  appendPaneTextObservation(storage, identity, {
    source: "snapshot",
    text: "expired",
    seenAtUnixMs: 1,
  });

  for (let index = 0; index < PANE_JOURNAL_LIMITS.maxEntries + 5; index += 1) {
    appendPaneTextObservation(storage, identity, {
      source: "replace",
      text: `line-${index}-${"x".repeat(5000)}`,
      seenAtUnixMs: PANE_JOURNAL_LIMITS.maxAgeMs + 1000 + index,
    });
  }

  const journal = loadPaneJournal(storage, identity.paneKey, PANE_JOURNAL_LIMITS.maxAgeMs + 2000);
  assert(journal.entries.length <= PANE_JOURNAL_LIMITS.maxEntries, "entry count cap should hold");
  assert(journal.totalTextBytes <= PANE_JOURNAL_LIMITS.maxTextBytesPerPane, "text byte cap should hold");
  assert(!formatPaneJournalText(journal).includes("expired"), "expired text should be pruned");
});

test("invalid storage is cleared defensively", () => {
  const storage = new MemoryStorage();
  const identity = buildPaneIdentity({
    sessionName: "taarof web",
    workspaceId: 1,
    workspaceName: "Main",
    tabId: 2,
    tabName: "Codex",
    paneId: 0,
  });

  storage.setItem(`taarof.web.paneJournal.v1.${encodeURIComponent(identity.paneKey)}`, "{not-json");
  const journal = loadPaneJournal(storage, identity.paneKey, 1000);
  assert(journal.entries.length === 0, "invalid journal should load as empty");
  assert(storage.getItem(`taarof.web.paneJournal.v1.${encodeURIComponent(identity.paneKey)}`) === null, "invalid storage should be removed");
});

test("invalid non-finite dimensions are cleared defensively", () => {
  const storage = new MemoryStorage();
  const identity = buildPaneIdentity({
    sessionName: "taarof web",
    workspaceId: 1,
    workspaceName: "Main",
    tabId: 2,
    tabName: "Codex",
    paneId: 0,
  });
  const key = `taarof.web.paneJournal.v1.${encodeURIComponent(identity.paneKey)}`;

  storage.setItem(
    key,
    `{"paneKey":"${identity.paneKey}","entries":[{"paneKey":"${identity.paneKey}","displayName":"Codex_pane-0_test","tabNameAtCapture":"Codex","workspaceNameAtCapture":"Main","workspaceId":1,"tabId":2,"paneId":0,"seenAtUnixMs":1000,"source":"snapshot","cols":1e999,"text":"one","textHash":"00000000","isLatest":true}],"totalTextBytes":3}`,
  );

  const journal = loadPaneJournal(storage, identity.paneKey, 1000);
  assert(journal.entries.length === 0, "non-finite dimensions should load as empty");
  assert(storage.getItem(key) === null, "non-finite dimensions should clear storage");
});

test("prune removes stale orphan journals without loading a card directly", () => {
  const storage = new MemoryStorage();
  const identity = buildPaneIdentity({
    sessionName: "taarof web",
    workspaceId: 1,
    workspaceName: "Closed",
    tabId: 2,
    tabName: "Closed Codex",
    paneId: 0,
  });
  const key = `taarof.web.paneJournal.v1.${encodeURIComponent(identity.paneKey)}`;

  appendPaneTextObservation(storage, identity, {
    source: "snapshot",
    text: "stale orphan",
    seenAtUnixMs: 1000,
  });

  const changedCount = pruneExpiredPaneJournals(storage, PANE_JOURNAL_LIMITS.maxAgeMs + 2001);
  assert(changedCount === 1, "stale orphan cleanup should report one changed journal");
  assert(storage.getItem(key) === null, "stale orphan journal should be removed");
});

test("prune keeps fresh journals", () => {
  const storage = new MemoryStorage();
  const identity = buildPaneIdentity({
    sessionName: "taarof web",
    workspaceId: 1,
    workspaceName: "Main",
    tabId: 2,
    tabName: "Codex",
    paneId: 0,
  });
  const key = `taarof.web.paneJournal.v1.${encodeURIComponent(identity.paneKey)}`;

  appendPaneTextObservation(storage, identity, {
    source: "snapshot",
    text: "fresh",
    seenAtUnixMs: 1000,
  });

  const before = storage.getItem(key);
  const changedCount = pruneExpiredPaneJournals(storage, 2000);
  assert(changedCount === 0, "fresh journal cleanup should not report a change");
  assert(storage.getItem(key) === before, "fresh journal should remain unchanged");
});

test("prune removes invalid encoded keys and malformed journals defensively", () => {
  const storage = new MemoryStorage();
  const identity = buildPaneIdentity({
    sessionName: "taarof web",
    workspaceId: 1,
    workspaceName: "Main",
    tabId: 2,
    tabName: "Codex",
    paneId: 0,
  });
  const malformedKey = `taarof.web.paneJournal.v1.${encodeURIComponent(identity.paneKey)}`;
  const invalidEncodedKey = "taarof.web.paneJournal.v1.%E0%A4%A";

  storage.setItem(malformedKey, "{not-json");
  storage.setItem(invalidEncodedKey, "not reachable");

  const changedCount = pruneExpiredPaneJournals(storage, 1000);
  assert(changedCount === 2, "invalid cleanup should report both removed journal keys");
  assert(storage.getItem(malformedKey) === null, "malformed journal should be removed");
  assert(storage.getItem(invalidEncodedKey) === null, "invalid encoded journal key should be removed");
});

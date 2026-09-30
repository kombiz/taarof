import { browserLocalStorage, optionalLocalStorage } from "./browserStorage.js";
import {
  readStoredMonitorOrder,
  readStoredWatchedKeys,
  writeStoredMonitorOrder,
  writeStoredWatchedKeys,
} from "./monitorBoard.js";
import { loadPaneJournal, pruneExpiredPaneJournals, savePaneJournal } from "./monitorPaneJournal.js";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
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

// Installs a `window` whose `localStorage` getter throws, as browsers do when
// site settings deny storage, for the duration of `fn`.
function withDeniedLocalStorage(fn: () => void) {
  const descriptor = Object.getOwnPropertyDescriptor(globalThis, "window");
  const deniedWindow = {};
  Object.defineProperty(deniedWindow, "localStorage", {
    get() {
      throw new Error("SecurityError: access to localStorage is denied");
    },
  });
  Object.defineProperty(globalThis, "window", { configurable: true, value: deniedWindow });
  try {
    fn();
  } finally {
    if (descriptor) {
      Object.defineProperty(globalThis, "window", descriptor);
    } else {
      delete (globalThis as { window?: unknown }).window;
    }
  }
}

test("a throwing localStorage getter resolves to no storage", () => {
  withDeniedLocalStorage(() => {
    assert(browserLocalStorage() === null, "denied storage is absent");
  });
});

test("Monitor preference reads and writes tolerate a throwing localStorage getter", () => {
  withDeniedLocalStorage(() => {
    const storage = optionalLocalStorage();
    assert(optionalLocalStorage() === storage, "the fallback is stable across renders");

    assert(readStoredMonitorOrder(storage).length === 0, "order reads as empty");
    assert(readStoredWatchedKeys(storage).length === 0, "watch list reads as empty");
    assert(writeStoredMonitorOrder(storage, ["1:1:1"]) === false, "order write is refused without throwing");
    assert(writeStoredWatchedKeys(storage, ["1:1:1"]) === false, "watch write is refused without throwing");
    assert(readStoredWatchedKeys(storage).length === 0, "refused writes are not persisted");
  });
});

test("Monitor pane journal tolerates a throwing localStorage getter", () => {
  withDeniedLocalStorage(() => {
    const storage = optionalLocalStorage();
    assert(pruneExpiredPaneJournals(storage) === 0, "pruning finds nothing");
    assert(loadPaneJournal(storage, "pane").entries.length === 0, "journal loads empty");
    savePaneJournal(storage, { paneKey: "pane", entries: [], totalTextBytes: 0 });
    const entry = {
      paneKey: "pane",
      displayName: "pane",
      tabNameAtCapture: "tab",
      workspaceNameAtCapture: "ws",
      workspaceId: 1,
      tabId: 1,
      paneId: 1,
      seenAtUnixMs: Date.now(),
      source: "snapshot" as const,
      text: "hello",
      textHash: "h",
    };
    savePaneJournal(storage, { paneKey: "pane", entries: [entry], totalTextBytes: 5 });
  });
});

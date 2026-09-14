import {
  PaneTileWork,
  paneTerminalIdentity,
  paneTileBoundaryNeedsReset,
  planPaneTerminalTransport,
  shouldResetPaneTerminal,
} from "./PaneTerminal.lifecycle.js";

function assertEqual<T>(actual: T, expected: T, message = "values differ") {
  if (actual !== expected) throw new Error(`${message}: expected ${String(expected)}, got ${String(actual)}`);
}

function test(name: string, fn: () => void) {
  try { fn(); console.log(`PASS ${name}`); } catch (error) { console.error(`FAIL ${name}`); throw error; }
}

const legacyTile = {
  hasTerminalHost: true,
  hasTab: true,
  hasPane: true,
  attachSupported: true,
  usePtyTransport: false,
  isSelected: true,
  isControlUnlocked: false,
  hasControlTarget: true,
};

test("legacy control adds a selected-only socket without replacing observation", () => {
  const observe = planPaneTerminalTransport(legacyTile);
  const unlocked = planPaneTerminalTransport({ ...legacyTile, isControlUnlocked: true });
  const deselected = planPaneTerminalTransport({ ...legacyTile, isSelected: false, isControlUnlocked: true });

  assertEqual(observe.observeEnabled, true);
  assertEqual(observe.controlEnabled, false);
  assertEqual(unlocked.observeEnabled, true, "unlock must retain the observe socket");
  assertEqual(unlocked.controlEnabled, true);
  assertEqual(deselected.observeEnabled, true, "focus changes must retain observation");
  assertEqual(deselected.controlEnabled, false);
});

test("empty, unsupported, and raw-pty tiles never open a legacy attachment", () => {
  assertEqual(planPaneTerminalTransport({ ...legacyTile, hasPane: false }).observeEnabled, false);
  assertEqual(planPaneTerminalTransport({ ...legacyTile, attachSupported: false }).observeEnabled, false);
  assertEqual(planPaneTerminalTransport({ ...legacyTile, usePtyTransport: true }).observeEnabled, false);
});

test("focus and control changes do not create a terminal reset identity", () => {
  const host = {};
  const identity = { key: paneTerminalIdentity(4, 9), host };
  assertEqual(shouldResetPaneTerminal(identity, identity, false), false);
  assertEqual(
    shouldResetPaneTerminal(identity, { key: paneTerminalIdentity(4, 10), host }, false),
    true,
    "pane promotion must reset only the promoted tile",
  );
});

test("refresh retries an isolated failed pane without remounting healthy siblings", () => {
  assertEqual(
    paneTileBoundaryNeedsReset(
      { paneId: 9, refreshGeneration: 3 },
      { paneId: 9, refreshGeneration: 4 },
    ),
    true,
  );
  assertEqual(
    paneTileBoundaryNeedsReset(
      { paneId: 9, refreshGeneration: 3 },
      { paneId: 9, refreshGeneration: 3 },
    ),
    false,
  );
});

test("tile cleanup aborts preview/stat work and clears delayed callbacks on eviction, view, and auth changes", () => {
  let nextTimer = 0;
  const callbacks = new Map<number, () => void>();
  const cleared: number[] = [];
  const work = new PaneTileWork({
    setTimeout: (callback) => {
      nextTimer += 1;
      callbacks.set(nextTimer, callback);
      return nextTimer;
    },
    clearTimeout: (timer) => {
      cleared.push(timer);
      callbacks.delete(timer);
    },
  });

  const firstPreview = work.beginRequest("file-preview");
  const preview = work.beginRequest("file-preview");
  const stat = work.beginRequest("file-stat");
  let callbacksRun = 0;
  work.schedule(() => { callbacksRun += 1; }, 0);
  work.schedule(() => { callbacksRun += 1; }, 150);
  work.cancel();

  assertEqual(firstPreview.signal.aborted, true, "a newer preview replaces the stale request");
  assertEqual(preview.signal.aborted, true);
  assertEqual(stat.signal.aborted, true);
  assertEqual(cleared.length, 2);
  for (const callback of callbacks.values()) callback();
  assertEqual(callbacksRun, 0);
});

import {
  clampTerminalLinkChoiceAnchor,
  dismissTerminalLinkChoice,
  initialTerminalLinkChoiceIndex,
  nextTerminalLinkChoiceIndex,
  terminalLinkChoiceKeyboardDecision,
  terminalLinkChoiceItems,
} from "./terminalLinkDisambiguation.js";
import type { TerminalLinkCandidates } from "./terminalLinks.js";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(message);
  }
}

const tests: Array<{ name: string; fn: () => void }> = [];

function test(name: string, fn: () => void) {
  tests.push({ name, fn });
}

function assertEqual(actual: unknown, expected: unknown, message: string) {
  const actualJson = JSON.stringify(actual);
  const expectedJson = JSON.stringify(expected);
  assert(actualJson === expectedJson, `${message}\nactual: ${actualJson}\nexpected: ${expectedJson}`);
}

function run() {
  for (const { name, fn } of tests) {
    try {
      fn();
      console.log(`PASS ${name}`);
    } catch (error) {
      console.error(`FAIL ${name}`);
      throw error;
    }
  }
}

test("terminal link choice items rank a known existing file before URL", () => {
  const candidates: TerminalLinkCandidates = {
    ambiguous: true,
    candidates: [
      { kind: "file", path: "README.md", line: 12, exists: true },
      { kind: "url", url: "https://README.md:12" },
    ],
  };

  assertEqual(
    terminalLinkChoiceItems(candidates).map((item) => ({
      action: item.action,
      label: item.label,
      destination: item.destination,
    })),
    [
      { action: "open-file", label: "Open file", destination: "README.md:12" },
      { action: "open-url", label: "Open URL", destination: "https://README.md:12" },
    ],
    "known existing file should be first and labels should retain full destinations",
  );
});

test("terminal link choice initial selection does not arm a navigating item", () => {
  assertEqual(
    initialTerminalLinkChoiceIndex([{ action: "open-url" }]),
    -1,
    "initial focus should remain on the panel until the user explicitly selects an item",
  );
});

test("terminal link keyboard flow requires selection before explicit activation", () => {
  assertEqual(
    terminalLinkChoiceKeyboardDecision(-1, 1, "Enter"),
    { kind: "ignore" },
    "immediate Enter must not activate the unselected URL",
  );
  assertEqual(
    terminalLinkChoiceKeyboardDecision(-1, 1, "Escape"),
    { kind: "ignore" },
    "Escape remains owned by the document capture dismissal handler",
  );

  const select = terminalLinkChoiceKeyboardDecision(-1, 1, "ArrowDown");
  assertEqual(
    select,
    { kind: "select", selectedIndex: 0 },
    "arrow navigation should explicitly select the first item",
  );
  assertEqual(
    terminalLinkChoiceKeyboardDecision(0, 1, "Enter"),
    { kind: "activate", selectedIndex: 0 },
    "Enter should activate only after explicit arrow selection",
  );
});

test("OSC-8 confirmation discloses its full destination without arming navigation", () => {
  const items = terminalLinkChoiceItems({
    ambiguous: false,
    candidates: [{ kind: "url", url: "https://evil.example/landing?q=1" }],
  });
  assertEqual(
    items.map((item) => ({ label: item.label, destination: item.destination })),
    [{ label: "Open URL", destination: "https://evil.example/landing?q=1" }],
    "confirmation should show the complete OSC-8 target",
  );
  assertEqual(
    initialTerminalLinkChoiceIndex(items),
    -1,
    "the disclosed target should require an explicit second action",
  );
});

test("terminal link choice items suppress the file item when existence is unknown", () => {
  const candidates: TerminalLinkCandidates = {
    ambiguous: true,
    candidates: [
      { kind: "file", path: "build.sh" },
      { kind: "url", url: "https://build.sh" },
    ],
  };

  assertEqual(
    terminalLinkChoiceItems(candidates).map((item) => ({
      action: item.action,
      destination: item.destination,
    })),
    [{ action: "open-url", destination: "https://build.sh" }],
    "remote or missing files should not expose an open-file action",
  );
});

test("terminal link choice index reducer handles arrow keys with wrapping", () => {
  assert(nextTerminalLinkChoiceIndex(-1, 2, "ArrowDown") === 0, "down should select first from panel");
  assert(nextTerminalLinkChoiceIndex(-1, 2, "ArrowUp") === 1, "up should select last from panel");
  assert(nextTerminalLinkChoiceIndex(0, 2, "ArrowDown") === 1, "down should advance");
  assert(nextTerminalLinkChoiceIndex(1, 2, "ArrowDown") === 0, "down should wrap");
  assert(nextTerminalLinkChoiceIndex(0, 2, "ArrowUp") === 1, "up should wrap");
  assert(nextTerminalLinkChoiceIndex(1, 2, "ArrowUp") === 0, "up should go back");
  assert(nextTerminalLinkChoiceIndex(1, 2, "Home") === 0, "home should go first");
  assert(nextTerminalLinkChoiceIndex(0, 2, "End") === 1, "end should go last");
});

test("terminal link choice anchor clamps the full panel inside the viewport", () => {
  const panel = { panelWidth: 352, panelHeight: 180, viewportWidth: 1440, viewportHeight: 900 };
  assertEqual(
    clampTerminalLinkChoiceAnchor({ clientX: 0, clientY: 0, ...panel }),
    { x: 12, y: 12 },
    "top-left click should clamp to padding",
  );
  assertEqual(
    clampTerminalLinkChoiceAnchor({ clientX: 1440, clientY: 0, ...panel }),
    { x: 1076, y: 12 },
    "top-right click should keep the panel width inside the viewport",
  );
  assertEqual(
    clampTerminalLinkChoiceAnchor({ clientX: 0, clientY: 900, ...panel }),
    { x: 12, y: 708 },
    "bottom-left click should keep the panel height inside the viewport",
  );
  assertEqual(
    clampTerminalLinkChoiceAnchor({ clientX: 1440, clientY: 900, ...panel }),
    { x: 1076, y: 708 },
    "bottom-right click should keep the panel inside both viewport edges",
  );
  assertEqual(
    clampTerminalLinkChoiceAnchor({ clientX: 1400, clientY: 200, ...panel }),
    { x: 1076, y: 200 },
    "the reviewed 1400px click in a 1440px viewport should not render offscreen",
  );
});

test("terminal link dismiss decision does not open anything and restores focus", () => {
  assertEqual(
    dismissTerminalLinkChoice(),
    { shouldOpen: false, restoreFocus: true },
    "dismissal should be inert and return focus to the terminal",
  );
});

run();

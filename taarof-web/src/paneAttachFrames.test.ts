import {
  buildPaneControlInputFrame,
  buildPaneControlResizeFrame,
  decodeBase64Utf8,
  dispatchPaneAttachFrame,
  normalizePanePreviewText,
  paneAttachFrameText,
} from "./paneAttachFrames.js";
import type { PaneAttachReplaceFrame, PaneAttachSnapshotFrame } from "./types.js";

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

function encodeBase64Utf8(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let binary = "";
  bytes.forEach((byte) => {
    binary += String.fromCharCode(byte);
  });
  return globalThis.btoa(binary);
}

test("decodeBase64Utf8 round-trips UTF-8 payloads", () => {
  const text = "hello π جهان";
  assert(decodeBase64Utf8(encodeBase64Utf8(text)) === text, "decoded text should match input");
});

test("frame normalize strips ANSI/control sequences consistently for snapshot and replace frames", () => {
  const raw = "\x1b[31mred\x1b[0m\r\nnext\x07\rline";
  const snapshot: PaneAttachSnapshotFrame = {
    type: "snapshot",
    pane_id: 1,
    cols: 80,
    rows: 24,
    encoding: "base64",
    payload: encodeBase64Utf8(raw),
  };
  const replace: PaneAttachReplaceFrame = {
    type: "replace",
    pane_id: 1,
    cols: 80,
    rows: 24,
    encoding: "utf8",
    payload: raw,
  };

  assert(normalizePanePreviewText(raw) === "red\nnext\nline", "plain preview should be normalized");
  assert(paneAttachFrameText(snapshot) === "red\nnext\nline", "snapshot text should normalize through shared path");
  assert(paneAttachFrameText(replace) === "red\nnext\nline", "replace text should normalize through shared path");
});

test("frame dispatch classifies snapshot/replace/delta/error/resize and falls back to raw text on non-JSON", () => {
  const snapshot = dispatchPaneAttachFrame(
    JSON.stringify({ type: "snapshot", pane_id: 1, cols: 80, rows: 24, encoding: "base64", payload: "b25l" }),
  );
  const replace = dispatchPaneAttachFrame(
    JSON.stringify({ type: "replace", pane_id: 1, cols: 80, rows: 24, encoding: "utf8", payload: "two" }),
  );
  const delta = dispatchPaneAttachFrame(
    JSON.stringify({ type: "delta", pane_id: 1, cols: 80, rows: 24, encoding: "utf8", payload: "three" }),
  );
  const error = dispatchPaneAttachFrame(
    JSON.stringify({ type: "error", pane_id: 1, error: "boom" }),
  );
  const resize = dispatchPaneAttachFrame(
    JSON.stringify({ type: "resize", pane_id: 1, cols: 120, rows: 32, supported: false, message: "not supported" }),
  );
  const raw = dispatchPaneAttachFrame("not-json");

  assert(snapshot.kind === "snapshot", "snapshot should classify");
  assert(replace.kind === "replace", "replace should classify");
  assert(delta.kind === "delta" && delta.frame.payload === "three", "delta should classify");
  assert(error.kind === "error", "error should classify");
  assert(resize.kind === "resize" && resize.frame.supported === false, "resize should classify");
  assert(raw.kind === "raw" && raw.text === "not-json", "non-JSON should fall back to raw text");
});

test("control frame builders preserve raw input and resize dimensions", () => {
  assert(
    buildPaneControlInputFrame("\u0003abc\r\n") ===
      JSON.stringify({ type: "input", encoding: "utf8", payload: "\u0003abc\r\n" }),
    "input frame should preserve raw control bytes in the JSON payload",
  );
  assert(
    buildPaneControlResizeFrame(120, 32) ===
      JSON.stringify({ type: "resize", cols: 120, rows: 32 }),
    "resize frame should carry cols and rows",
  );
});

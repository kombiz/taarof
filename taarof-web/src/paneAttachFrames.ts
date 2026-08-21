import type {
  PaneAttachErrorFrame,
  PaneAttachReplaceFrame,
  PaneAttachSnapshotFrame,
  PaneAttachTextFrame,
  PaneControlDeltaFrame,
  PaneControlResizeFrame,
} from "./types.js";

export type PaneAttachPhase =
  | "idle"
  | "queued"
  | "connecting"
  | "live"
  | "fallback"
  | "unsupported"
  | "closed"
  | "error";

export type PaneAttachDispatchFrame =
  | { kind: "snapshot"; frame: PaneAttachSnapshotFrame }
  | { kind: "replace"; frame: PaneAttachReplaceFrame }
  | { kind: "delta"; frame: PaneControlDeltaFrame }
  | { kind: "resize"; frame: PaneControlResizeFrame }
  | { kind: "error"; frame: PaneAttachErrorFrame }
  | { kind: "raw"; text: string };

export function decodeBase64Utf8(payload: string): string {
  const decoder = new TextDecoder();
  const atobImpl = globalThis.atob;
  if (typeof atobImpl === "function") {
    const binary = atobImpl(payload);
    const bytes = Uint8Array.from(binary, (character) => character.charCodeAt(0));
    return decoder.decode(bytes);
  }

  const bufferCtor = (globalThis as typeof globalThis & {
    Buffer?: { from(value: string, encoding: "base64"): Uint8Array };
  }).Buffer;
  if (bufferCtor) {
    return decoder.decode(bufferCtor.from(payload, "base64"));
  }

  throw new Error("base64 decoding is unavailable in this runtime");
}

export function stripAnsiSequences(text: string): string {
  return text.replace(
    /\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1B\\))/g,
    "",
  );
}

export function normalizePanePreviewText(text: string): string {
  return stripAnsiSequences(text)
    .replace(/\r\n/g, "\n")
    .replace(/\r/g, "\n")
    .replace(/[\x00-\x08\x0B\x0C\x0E-\x1F\x7F]/g, "");
}

export function normalizePaneTerminalText(text: string): string {
  return text.replace(/\r?\n/g, "\r\n");
}

export function paneAttachFrameText(
  frame: PaneAttachSnapshotFrame | PaneAttachReplaceFrame,
): string {
  const text = frame.type === "snapshot" ? decodeBase64Utf8(frame.payload) : frame.payload;
  return normalizePanePreviewText(text);
}

export function dispatchPaneAttachFrame(payload: string): PaneAttachDispatchFrame {
  let frame: PaneAttachTextFrame;
  try {
    frame = JSON.parse(payload) as PaneAttachTextFrame;
  } catch {
    return { kind: "raw", text: payload };
  }

  if (frame.type === "snapshot") {
    return { kind: "snapshot", frame };
  }
  if (frame.type === "replace") {
    return { kind: "replace", frame };
  }
  if (frame.type === "delta") {
    return { kind: "delta", frame };
  }
  if (frame.type === "error") {
    return { kind: "error", frame };
  }
  if (frame.type === "resize") {
    return { kind: "resize", frame };
  }

  return { kind: "raw", text: payload };
}

export function buildPaneControlInputFrame(payload: string): string {
  return JSON.stringify({
    type: "input",
    encoding: "utf8",
    payload,
  });
}

export function buildPaneControlResizeFrame(cols: number, rows: number): string {
  return JSON.stringify({
    type: "resize",
    cols,
    rows,
  });
}

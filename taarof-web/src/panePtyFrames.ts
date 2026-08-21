// Pure frame codec + stream reducer for the raw PTY WebSocket transport
// (`GET /api/v1/tabs/{tab}/panes/{pane}/pty/ws`, served by
// `taarof-app/src/http/pty.rs`). This module is deliberately DOM/React-free so
// the ordering/reset semantics can be unit-tested in isolation. The hook
// `usePanePty.ts` owns the socket lifecycle and delegates every decision here.
//
// Invariants (mirrored from the server protocol and enforced by tests):
//   * `output_seq` is a JSON STRING that is also a u64 on the wire. It is kept
//     verbatim and echoed back on resume; it is NEVER Number()-parsed (precision
//     loss past 2^53) and NEVER compared.
//   * A terminal reset happens ONLY on a `checkpoint` frame. `output` frames are
//     written with no reset so scrollback accumulates.
//   * There is NO client-side sequence dedup: the server guarantees ordered
//     delivery, and `send_output_frame` may split one seq across multiple frames.

// ── Server frame types (mirrors pty.rs FrameContext; only consumed fields) ──

export interface PtyCheckpointFrame {
  kind: "checkpoint";
  epoch: string;
  output_seq: string; // STRING per pty.rs (u64 rendered as a string)
  cols: number;
  rows: number;
  checkpoint: { ansi_base64: string; byte_count: number; state_hash: string };
}

export interface PtyOutputFrame {
  kind: "output";
  epoch: string;
  output_seq: string; // STRING
  payload_base64: string;
  byte_count: number;
}

export interface PtyAckFrame {
  kind: "ack";
  epoch: string;
  ack_output_seq: string;
}

export interface PtyHeartbeatFrame {
  kind: "heartbeat";
  epoch: string;
  monotonic_ms: number;
}

export interface PtyErrorFrame {
  kind: "error";
  epoch: string;
  error: { code: string; message: string };
}

export interface PtyUnknownFrame {
  kind: "unknown";
  epoch?: string;
}

export type PtyServerFrame =
  | PtyCheckpointFrame
  | PtyOutputFrame
  | PtyAckFrame
  | PtyHeartbeatFrame
  | PtyErrorFrame
  | PtyUnknownFrame;

/// 30s signing window for client control frames. Absorbs clock skew while
/// keeping the server's anti-replay bound tight; the server rejects any frame
/// whose `deadline_ms` is already in the past.
export const PTY_INPUT_DEADLINE_MS = 30_000;

/// The single grant generation the server issues for one control-enabled
/// connection. Sent as a STRING to match the server's `ClientFrame` shape.
const PTY_GRANT_GENERATION = "1";

// ── base64 helpers (bytes in, bytes out — never String for the raw path) ──

interface BufferLike {
  from(value: string, encoding: "base64"): Uint8Array;
}

function bufferCtor(): BufferLike | undefined {
  return (globalThis as typeof globalThis & { Buffer?: BufferLike }).Buffer;
}

/// Decode a standard-base64 string to its exact bytes. Returns a `Uint8Array`,
/// NOT a decoded string — raw PTY bytes are fed to xterm verbatim so ANSI stays
/// intact.
export function decodeBase64ToBytes(b64: string): Uint8Array {
  const atobImpl = globalThis.atob;
  if (typeof atobImpl === "function") {
    const binary = atobImpl(b64);
    return Uint8Array.from(binary, (character) => character.charCodeAt(0));
  }

  const ctor = bufferCtor();
  if (ctor) {
    return new Uint8Array(ctor.from(b64, "base64"));
  }

  throw new Error("base64 decoding is unavailable in this runtime");
}

/// Encode text as UTF-8 then standard base64. `TextEncoder` guarantees multi-byte
/// characters (accents, CJK, emoji) encode as the exact UTF-8 bytes the server
/// base64-decodes on the input path.
export function encodeBase64Utf8(text: string): string {
  const bytes = new TextEncoder().encode(text);
  const btoaImpl = globalThis.btoa;
  if (typeof btoaImpl === "function") {
    let binary = "";
    bytes.forEach((byte) => {
      binary += String.fromCharCode(byte);
    });
    return btoaImpl(binary);
  }

  const ctor = (globalThis as typeof globalThis & {
    Buffer?: { from(value: Uint8Array): { toString(encoding: "base64"): string } };
  }).Buffer;
  if (ctor) {
    return ctor.from(bytes).toString("base64");
  }

  throw new Error("base64 encoding is unavailable in this runtime");
}

// ── Frame decode ──

function coerceSeq(value: unknown): string {
  return typeof value === "string" ? value : String(value ?? "");
}

/// Parse one server text frame. Never throws: malformed JSON or an unrecognized
/// `kind` classifies as `{kind:"unknown"}` so the hook can ignore it safely.
export function decodePtyServerFrame(raw: string): PtyServerFrame {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return { kind: "unknown" };
  }
  if (typeof parsed !== "object" || parsed === null) {
    return { kind: "unknown" };
  }

  const frame = parsed as Record<string, unknown>;
  const epoch = typeof frame.epoch === "string" ? frame.epoch : undefined;

  switch (frame.kind) {
    case "checkpoint": {
      const checkpoint = (frame.checkpoint ?? {}) as Record<string, unknown>;
      return {
        kind: "checkpoint",
        epoch: epoch ?? "",
        output_seq: coerceSeq(frame.output_seq),
        cols: typeof frame.cols === "number" ? frame.cols : 0,
        rows: typeof frame.rows === "number" ? frame.rows : 0,
        checkpoint: {
          ansi_base64: typeof checkpoint.ansi_base64 === "string" ? checkpoint.ansi_base64 : "",
          byte_count: typeof checkpoint.byte_count === "number" ? checkpoint.byte_count : 0,
          state_hash: typeof checkpoint.state_hash === "string" ? checkpoint.state_hash : "",
        },
      };
    }
    case "output":
      return {
        kind: "output",
        epoch: epoch ?? "",
        output_seq: coerceSeq(frame.output_seq),
        payload_base64: typeof frame.payload_base64 === "string" ? frame.payload_base64 : "",
        byte_count: typeof frame.byte_count === "number" ? frame.byte_count : 0,
      };
    case "ack":
      return {
        kind: "ack",
        epoch: epoch ?? "",
        ack_output_seq: coerceSeq(frame.ack_output_seq),
      };
    case "heartbeat":
      return {
        kind: "heartbeat",
        epoch: epoch ?? "",
        monotonic_ms: typeof frame.monotonic_ms === "number" ? frame.monotonic_ms : 0,
      };
    case "error": {
      const error = (frame.error ?? {}) as Record<string, unknown>;
      return {
        kind: "error",
        epoch: epoch ?? "",
        error: {
          code: typeof error.code === "string" ? error.code : "unknown",
          message: typeof error.message === "string" ? error.message : "",
        },
      };
    }
    default:
      return { kind: "unknown", epoch };
  }
}

// ── Client frame builders ──

export function buildPtyInputFrame(params: {
  epoch: string;
  deadlineMs: number;
  data: string;
}): string {
  return JSON.stringify({
    kind: "input",
    epoch: params.epoch,
    grant_generation: PTY_GRANT_GENERATION,
    deadline_ms: params.deadlineMs,
    payload_base64: encodeBase64Utf8(params.data),
  });
}

export function buildPtyResizeFrame(params: {
  epoch: string;
  deadlineMs: number;
  cols: number;
  rows: number;
}): string {
  return JSON.stringify({
    kind: "resize",
    epoch: params.epoch,
    grant_generation: PTY_GRANT_GENERATION,
    deadline_ms: params.deadlineMs,
    cols: params.cols,
    rows: params.rows,
  });
}

export function buildPtyAckFrame(): string {
  // The server accepts `ack` frames but reads no sequence off them.
  return JSON.stringify({ kind: "ack" });
}

/// Conservative default cap for a single input frame's decoded UTF-8 payload.
/// The server rejects input frames whose decoded payload exceeds 65536 bytes
/// (`frame_too_large`); 61440 leaves headroom for framing/encoding overhead so a
/// large paste is split into several accepted frames instead of being dropped.
export const PTY_INPUT_CHUNK_MAX_BYTES = 61440;

/// Split `text` into chunks whose individual UTF-8 encodings are each ≤ maxBytes,
/// never splitting a Unicode code point (so a multi-byte char that would straddle
/// a boundary is pushed wholly into the next chunk and the wire bytes stay valid
/// UTF-8). Used for the paste path only — per-keystroke input never approaches
/// the limit. Empty input yields an empty array.
export function chunkPtyInputText(
  text: string,
  maxBytes: number = PTY_INPUT_CHUNK_MAX_BYTES,
): string[] {
  if (maxBytes <= 0) {
    throw new Error("maxBytes must be positive");
  }
  if (text === "") {
    return [];
  }
  const encoder = new TextEncoder();
  // Fast path for the common case — every keystroke routes through here, and any
  // paste under the budget is one chunk. If the whole payload already fits, skip
  // the per-code-point split loop and return it verbatim.
  if (encoder.encode(text).length <= maxBytes) {
    return [text];
  }
  const chunks: string[] = [];
  let current = "";
  let currentBytes = 0;
  // `for...of` iterates by code point, so surrogate pairs (e.g. emoji) stay whole.
  for (const codePoint of text) {
    const cpBytes = encoder.encode(codePoint).length;
    if (current !== "" && currentBytes + cpBytes > maxBytes) {
      chunks.push(current);
      current = "";
      currentBytes = 0;
    }
    current += codePoint;
    currentBytes += cpBytes;
  }
  if (current !== "") {
    chunks.push(current);
  }
  return chunks;
}

// ── Transport decision (fallback logic, AC #5) ──

/// Only panes the server advertises as `raw_pty` take the raw transport. Every
/// other value — `legacy_snapshot`, an unknown string, or `undefined` — falls
/// back to the legacy snapshot `/attach` path.
export function decidePaneTransport(
  capability: string | undefined,
): "raw_pty" | "legacy_snapshot" {
  return capability === "raw_pty" ? "raw_pty" : "legacy_snapshot";
}

// ── Reconnect backoff (pure) ──

/// Exponential backoff for pty/ws reconnects: attempt 0 → 500ms, doubling, capped
/// at 10s.
export function nextPtyReconnectDelayMs(attempt: number): number {
  return Math.min(500 * 2 ** attempt, 10_000);
}

// ── Legacy-fallback decision (pure, AC #5) ──

/// Number of consecutive connection failures WITHOUT ever receiving a checkpoint
/// that must occur before a raw_pty pane is downgraded to the legacy snapshot
/// path. A single transient first-attach failure (HTTP 429 attach-slots
/// exhausted, a network blip) must NOT permanently strand the pane on legacy.
export const PTY_FALLBACK_NO_CHECKPOINT_THRESHOLD = 3;

/// Decide whether a raw_pty pane should give up and fall back to the legacy
/// snapshot transport. Once ANY checkpoint has ever been received the pane is a
/// confirmed raw_pty stream and must always resume, never fall back. Otherwise
/// only fall back after N consecutive no-checkpoint connection failures.
export function shouldFallbackToLegacy(params: {
  hasEverCheckpointed: boolean;
  consecutiveNoCheckpointCloses: number;
}): boolean {
  if (params.hasEverCheckpointed) {
    return false;
  }
  return params.consecutiveNoCheckpointCloses >= PTY_FALLBACK_NO_CHECKPOINT_THRESHOLD;
}

// ── Stream reducer (testable core of ordering/reset semantics) ──

export interface PtyStreamState {
  epoch: string | null;
  lastOutputSeq: string | null; // raw server STRING; echoed on resume, never compared
  hasCheckpoint: boolean; // did THIS connection receive a checkpoint baseline
}

export const initialPtyStreamState: PtyStreamState = {
  epoch: null,
  lastOutputSeq: null,
  hasCheckpoint: false,
};

export type PtyStreamAction =
  | { type: "reset-write"; bytes: Uint8Array } // checkpoint: terminal.reset() then write
  | { type: "write"; bytes: Uint8Array } // output: write, NO reset
  | { type: "error"; code: string; message: string }
  | { type: "none" };

export interface PtyStreamStep {
  state: PtyStreamState;
  action: PtyStreamAction;
}

/// Fold one decoded server frame into the stream state and emit the terminal
/// action. Reset is emitted ONLY for `checkpoint`; `output` never resets.
export function reducePtyStream(
  state: PtyStreamState,
  frame: PtyServerFrame,
): PtyStreamStep {
  switch (frame.kind) {
    case "checkpoint":
      return {
        state: {
          epoch: frame.epoch,
          lastOutputSeq: frame.output_seq,
          hasCheckpoint: true,
        },
        action: { type: "reset-write", bytes: decodeBase64ToBytes(frame.checkpoint.ansi_base64) },
      };
    case "output":
      return {
        state: { ...state, epoch: frame.epoch, lastOutputSeq: frame.output_seq },
        action: { type: "write", bytes: decodeBase64ToBytes(frame.payload_base64) },
      };
    case "ack":
      return { state: { ...state, epoch: frame.epoch }, action: { type: "none" } };
    case "heartbeat":
      return { state: { ...state, epoch: frame.epoch }, action: { type: "none" } };
    case "error":
      return {
        state: { ...state, epoch: frame.epoch },
        action: { type: "error", code: frame.error.code, message: frame.error.message },
      };
    default:
      return { state, action: { type: "none" } };
  }
}

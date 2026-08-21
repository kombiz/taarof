import {
  buildPtyInputFrame,
  buildPtyResizeFrame,
  chunkPtyInputText,
  decidePaneTransport,
  decodeBase64ToBytes,
  decodePtyServerFrame,
  encodeBase64Utf8,
  initialPtyStreamState,
  nextPtyReconnectDelayMs,
  PTY_FALLBACK_NO_CHECKPOINT_THRESHOLD,
  reducePtyStream,
  shouldFallbackToLegacy,
  type PtyStreamState,
} from "./panePtyFrames.js";

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

function bytesToString(bytes: Uint8Array): string {
  return new TextDecoder().decode(bytes);
}

const checkpointBytes = "\x1b[31mCHECK\x1b[0m";
const checkpointB64 = encodeBase64Utf8(checkpointBytes);

function checkpointJson(overrides: Record<string, unknown> = {}): string {
  return JSON.stringify({
    kind: "checkpoint",
    epoch: "epoch-a",
    output_seq: "5",
    cols: 80,
    rows: 24,
    checkpoint: { ansi_base64: checkpointB64, byte_count: 5, state_hash: "h" },
    ...overrides,
  });
}

function outputJson(seq: string, text: string): string {
  return JSON.stringify({
    kind: "output",
    epoch: "epoch-a",
    output_seq: seq,
    payload_base64: encodeBase64Utf8(text),
    byte_count: text.length,
  });
}

test("decodePtyServerFrame classifies each server frame kind", () => {
  assert(decodePtyServerFrame(checkpointJson()).kind === "checkpoint", "checkpoint should classify");
  assert(decodePtyServerFrame(outputJson("6", "x")).kind === "output", "output should classify");
  assert(
    decodePtyServerFrame(JSON.stringify({ kind: "ack", epoch: "e", ack_output_seq: "6" })).kind === "ack",
    "ack should classify",
  );
  assert(
    decodePtyServerFrame(JSON.stringify({ kind: "heartbeat", epoch: "e", monotonic_ms: 12 })).kind ===
      "heartbeat",
    "heartbeat should classify",
  );
  assert(
    decodePtyServerFrame(
      JSON.stringify({ kind: "error", epoch: "e", error: { code: "boom", message: "bad" } }),
    ).kind === "error",
    "error should classify",
  );
});

test("decodePtyServerFrame returns unknown on unknown kind and non-JSON without throwing", () => {
  assert(decodePtyServerFrame(JSON.stringify({ kind: "wat" })).kind === "unknown", "unknown kind -> unknown");
  assert(decodePtyServerFrame("not-json").kind === "unknown", "non-JSON -> unknown");
  assert(decodePtyServerFrame("42").kind === "unknown", "non-object JSON -> unknown");
});

test("decodeBase64ToBytes returns the exact bytes of a known payload", () => {
  const bytes = decodeBase64ToBytes(encodeBase64Utf8("\x1b[31mX"));
  assert(bytes instanceof Uint8Array, "result should be bytes");
  assert(bytes[0] === 0x1b, "first byte should be ESC");
  assert(bytesToString(bytes) === "\x1b[31mX", "decoded bytes should reconstruct the payload");
});

test("encodeBase64Utf8 round-trips multi-byte UTF-8", () => {
  const text = "héllo 世 🚀";
  const roundTripped = bytesToString(decodeBase64ToBytes(encodeBase64Utf8(text)));
  assert(roundTripped === text, "multi-byte UTF-8 should survive the encode/decode round-trip");
});

test("reducePtyStream — checkpoint resets, seeds epoch and seq, decodes bytes", () => {
  const frame = decodePtyServerFrame(checkpointJson());
  const step = reducePtyStream(initialPtyStreamState, frame);
  assert(step.action.type === "reset-write", "checkpoint must emit reset-write");
  assert(step.action.type === "reset-write" && bytesToString(step.action.bytes) === checkpointBytes, "checkpoint bytes decode");
  assert(step.state.epoch === "epoch-a", "checkpoint seeds epoch");
  assert(step.state.lastOutputSeq === "5", "checkpoint seeds lastOutputSeq");
  assert(step.state.hasCheckpoint === true, "checkpoint marks hasCheckpoint");
});

test("reducePtyStream — ordered output writes without reset and advances seq", () => {
  const base = reducePtyStream(initialPtyStreamState, decodePtyServerFrame(checkpointJson())).state;
  const first = reducePtyStream(base, decodePtyServerFrame(outputJson("6", "one")));
  assert(first.action.type === "write", "output must write, not reset");
  assert(first.state.lastOutputSeq === "6", "output advances seq to 6");
  const second = reducePtyStream(first.state, decodePtyServerFrame(outputJson("7", "two")));
  assert(second.action.type === "write", "second output must write");
  assert(second.state.lastOutputSeq === "7", "output advances seq to 7");
  assert(second.state.hasCheckpoint === true, "hasCheckpoint stays true across output");
});

test("reducePtyStream — resume continuity vs mid-stream restart differ from the same prior state", () => {
  const prior: PtyStreamState = { epoch: "epoch-a", lastOutputSeq: "5", hasCheckpoint: true };
  const replay = reducePtyStream(prior, decodePtyServerFrame(outputJson("6", "cont")));
  assert(replay.action.type === "write", "in-window resume replays output (write, no reset)");
  const restart = reducePtyStream(prior, decodePtyServerFrame(checkpointJson({ epoch: "epoch-b", output_seq: "40" })));
  assert(restart.action.type === "reset-write", "resume-miss supplies a fresh checkpoint (reset-write)");
  assert(restart.state.epoch === "epoch-b", "checkpoint reseeds epoch");
  assert(restart.state.lastOutputSeq === "40", "checkpoint reseeds seq");
});

test("reducePtyStream — error frame surfaces code/message, no reset/write, refreshes epoch", () => {
  const prior: PtyStreamState = { epoch: "epoch-a", lastOutputSeq: "5", hasCheckpoint: true };
  const step = reducePtyStream(
    prior,
    decodePtyServerFrame(JSON.stringify({ kind: "error", epoch: "epoch-a", error: { code: "epoch_mismatch", message: "stale" } })),
  );
  assert(step.action.type === "error", "error frame emits an error action");
  assert(step.action.type === "error" && step.action.code === "epoch_mismatch", "error code carried");
  assert(step.state.lastOutputSeq === "5", "error does not advance seq");
  assert(step.state.hasCheckpoint === true, "error does not clear the checkpoint latch");
});

test("reducePtyStream — ack and heartbeat are no-ops that refresh epoch", () => {
  const prior: PtyStreamState = { epoch: "epoch-a", lastOutputSeq: "5", hasCheckpoint: true };
  const ack = reducePtyStream(prior, decodePtyServerFrame(JSON.stringify({ kind: "ack", epoch: "epoch-a", ack_output_seq: "9" })));
  assert(ack.action.type === "none", "ack is a no-op action");
  assert(ack.state.lastOutputSeq === "5", "ack does not change lastOutputSeq");
  const heartbeat = reducePtyStream(prior, decodePtyServerFrame(JSON.stringify({ kind: "heartbeat", epoch: "epoch-a", monotonic_ms: 5 })));
  assert(heartbeat.action.type === "none", "heartbeat is a no-op action");
  assert(heartbeat.state.lastOutputSeq === "5", "heartbeat does not change lastOutputSeq");
});

test("buildPtyInputFrame produces the signed input frame with base64 payload", () => {
  const frame = buildPtyInputFrame({ epoch: "epoch-a", deadlineMs: 1234, data: "ls\n" });
  assert(
    frame ===
      JSON.stringify({
        kind: "input",
        epoch: "epoch-a",
        grant_generation: "1",
        deadline_ms: 1234,
        payload_base64: encodeBase64Utf8("ls\n"),
      }),
    "input frame JSON must match the server ClientFrame shape",
  );
  const multibyte = buildPtyInputFrame({ epoch: "e", deadlineMs: 1, data: "世" });
  assert(multibyte.includes(encodeBase64Utf8("世")), "multi-byte input encodes correctly");
});

test("buildPtyResizeFrame produces the signed resize frame", () => {
  const frame = buildPtyResizeFrame({ epoch: "epoch-a", deadlineMs: 9999, cols: 120, rows: 40 });
  assert(
    frame ===
      JSON.stringify({
        kind: "resize",
        epoch: "epoch-a",
        grant_generation: "1",
        deadline_ms: 9999,
        cols: 120,
        rows: 40,
      }),
    "resize frame JSON must match the server ClientFrame shape",
  );
});

test("decidePaneTransport routes only raw_pty to the raw transport", () => {
  assert(decidePaneTransport("raw_pty") === "raw_pty", "raw_pty -> raw");
  assert(decidePaneTransport("legacy_snapshot") === "legacy_snapshot", "legacy_snapshot -> legacy");
  assert(decidePaneTransport(undefined) === "legacy_snapshot", "undefined -> legacy (safe default)");
  assert(decidePaneTransport("something-else") === "legacy_snapshot", "arbitrary -> legacy");
});

test("nextPtyReconnectDelayMs backs off exponentially and caps at 10s", () => {
  assert(nextPtyReconnectDelayMs(0) === 500, "attempt 0 -> 500ms");
  assert(nextPtyReconnectDelayMs(1) === 1000, "attempt 1 -> 1000ms");
  assert(nextPtyReconnectDelayMs(2) === 2000, "attempt 2 -> 2000ms");
  assert(nextPtyReconnectDelayMs(1) > nextPtyReconnectDelayMs(0), "delay is monotonic");
  assert(nextPtyReconnectDelayMs(20) === 10_000, "delay caps at 10s");
});

test("shouldFallbackToLegacy waits for N consecutive no-checkpoint closes", () => {
  assert(PTY_FALLBACK_NO_CHECKPOINT_THRESHOLD === 3, "threshold is 3");
  // A single transient first-attach failure must NOT downgrade the pane.
  assert(
    shouldFallbackToLegacy({ hasEverCheckpointed: false, consecutiveNoCheckpointCloses: 1 }) === false,
    "1 no-checkpoint close -> keep retrying",
  );
  assert(
    shouldFallbackToLegacy({ hasEverCheckpointed: false, consecutiveNoCheckpointCloses: 2 }) === false,
    "2 no-checkpoint closes -> keep retrying",
  );
  assert(
    shouldFallbackToLegacy({ hasEverCheckpointed: false, consecutiveNoCheckpointCloses: 3 }) === true,
    "3 no-checkpoint closes -> fall back to legacy",
  );
  assert(
    shouldFallbackToLegacy({ hasEverCheckpointed: false, consecutiveNoCheckpointCloses: 4 }) === true,
    "beyond threshold -> still fall back",
  );
});

test("shouldFallbackToLegacy never falls back once a checkpoint was ever seen", () => {
  assert(
    shouldFallbackToLegacy({ hasEverCheckpointed: true, consecutiveNoCheckpointCloses: 0 }) === false,
    "checkpointed pane never falls back",
  );
  assert(
    shouldFallbackToLegacy({ hasEverCheckpointed: true, consecutiveNoCheckpointCloses: 99 }) === false,
    "checkpointed pane resumes regardless of later closes",
  );
});

test("chunkPtyInputText keeps small input as a single chunk and drops empty input", () => {
  assert(chunkPtyInputText("").length === 0, "empty input yields no chunks");
  const single = chunkPtyInputText("ls -la\n");
  assert(single.length === 1 && single[0] === "ls -la\n", "small input stays one chunk");
});

test("chunkPtyInputText fast-paths a payload at exactly the byte budget to one verbatim chunk", () => {
  // 3 two-byte chars = 6 bytes, budget 6: fits exactly, so the fast path returns
  // the input as a single chunk (never entering the split loop).
  const text = "ééé";
  const chunks = chunkPtyInputText(text, 6);
  assert(chunks.length === 1 && chunks[0] === text, "at-budget input stays one verbatim chunk");
  // One byte over the budget must split.
  assert(chunkPtyInputText("éééé", 6).length === 2, "one code point over the budget splits");
});

test("chunkPtyInputText splits on code-point boundaries within the byte budget", () => {
  // 10 two-byte characters ("é" = 0xC3 0xA9), budget 5 bytes -> 2 chars/chunk.
  const text = "é".repeat(10);
  const chunks = chunkPtyInputText(text, 5);
  const encoder = new TextEncoder();
  for (const chunk of chunks) {
    assert(encoder.encode(chunk).length <= 5, "each chunk's UTF-8 length is within budget");
  }
  assert(chunks.join("") === text, "chunks recombine to the original text losslessly");
  assert(chunks.length === 5, "10 two-byte chars in a 5-byte budget -> 5 chunks");
});

test("chunkPtyInputText never straddles a multi-byte char across a boundary", () => {
  // "aé" = 1 + 2 bytes = 3 bytes. Budget 2 must NOT split the "é" (0xC3 0xA9).
  const chunks = chunkPtyInputText("aé", 2);
  const encoder = new TextEncoder();
  assert(chunks.join("") === "aé", "lossless recombination across a straddle boundary");
  for (const chunk of chunks) {
    // Decoding each chunk in isolation must reproduce whole code points (no U+FFFD).
    const decoded = new TextDecoder("utf-8", { fatal: false }).decode(encoder.encode(chunk));
    assert(!decoded.includes("�"), "each chunk is independently valid UTF-8");
  }
  assert(chunks.length === 2 && chunks[0] === "a" && chunks[1] === "é", "'a' and 'é' land in separate chunks");
});

test("chunkPtyInputText handles a 4-byte emoji as one indivisible unit", () => {
  const chunks = chunkPtyInputText("🚀🚀", 4);
  assert(chunks.length === 2, "each 4-byte emoji is its own chunk under a 4-byte budget");
  assert(chunks.join("") === "🚀🚀", "emoji chunks recombine losslessly");
});

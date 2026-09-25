import {
  appendPreviewText,
  describePreviewCapability,
  formatRelativeTime,
  paneDimensions,
  previewPhaseLabel,
  projectLabel,
  sessionKey,
  shortSessionId,
  trimPreviewText,
} from "./AgentsView.helpers.js";
import type { AgentSessionRecord, PaneSnapshot } from "../types.js";

function assertEqual<T>(actual: T, expected: T, message = "values differ") {
  if (actual !== expected) {
    throw new Error(`${message}: expected ${String(expected)}, got ${String(actual)}`);
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

function session(overrides: Partial<AgentSessionRecord> = {}): AgentSessionRecord {
  return {
    agent: "claude",
    session_id: "1234567890abcdef",
    title: "Task",
    cwd: "/tmp/workspace/project",
    repo_root: "/tmp/workspace/project",
    updated_at_unix_ms: 1_700_000_000_000,
    status: "active",
    ...overrides,
  };
}

function pane(overrides: Partial<PaneSnapshot> = {}): PaneSnapshot {
  return {
    pane_id: 1,
    shell_running: true,
    has_child_process: false,
    remote_shell: false,
    cwd: "/repo",
    cwd_host: null,
    tmux_session: "session",
    tmux_host: "localhost",
    attach_supported: true,
    attach_kind: "tmux",
    ...overrides,
  };
}

test("session identity helpers format stable labels", () => {
  const value = session();

  assertEqual(sessionKey(value), "claude:1234567890abcdef");
  assertEqual(shortSessionId(value.session_id), "12345678...cdef");
  assertEqual(projectLabel(value), "project");
  assertEqual(projectLabel(session({ repo_root: null, cwd: "/" })), "/");
});

test("formatRelativeTime handles missing, future, minute, hour, and day ranges", () => {
  const originalNow = Date.now;
  Date.now = () => 1_700_000_000_000;
  try {
    assertEqual(formatRelativeTime(null), "unknown");
    assertEqual(formatRelativeTime(1_700_000_001_000), "just now");
    assertEqual(formatRelativeTime(1_699_999_880_000), "2m ago");
    assertEqual(formatRelativeTime(1_699_992_800_000), "2h ago");
    assertEqual(formatRelativeTime(1_699_740_800_000), "3d ago");
  } finally {
    Date.now = originalNow;
  }
});

test("preview text helpers normalize control sequences and cap retained text", () => {
  assertEqual(appendPreviewText("ok", "\u001b[31m!\u001b[0m\u0007"), "ok!");
  assertEqual(trimPreviewText("x".repeat(16_001)).length, 16_000);
});

test("preview labels distinguish browser viewing from native process attach", () => {
  assertEqual(previewPhaseLabel("fallback"), "fallback");
  assertEqual(describePreviewCapability(pane()), "live browser viewer available");
  assertEqual(describePreviewCapability(pane({ attach_supported: false, attach_kind: "unsupported" })), "live browser viewer unavailable for this pane type");
  assertEqual(
    describePreviewCapability(pane({ attach_supported: false, attach_unavailable_reason: "exact reason" })),
    "exact reason",
  );
  assertEqual(paneDimensions(pane({ cols: 80, rows: 24 }), null), "80x24");
  assertEqual(paneDimensions(pane({ cols: undefined, rows: undefined, tmux_probe: { width: 100, height: 30 } }), { cols: 120, rows: 40 }), "120x40");
});

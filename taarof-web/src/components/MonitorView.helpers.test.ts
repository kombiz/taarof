import {
  activityLabel,
  paneStatusLabel,
  previewFromTerminalText,
} from "./MonitorView.helpers.js";
import {
  applyManualOrder,
  buildFilterOptions,
  buildPaneTargets,
  isAttentionTarget,
  isBusyTarget,
  matchesActiveFilter,
  type PaneTarget,
} from "../monitorBoard.js";
import type { PaneSnapshot, TaarofStateSnapshot, TabSnapshot, WorkspaceSnapshot } from "../types.js";

function assertEqual<T>(actual: T, expected: T, message = "values differ") {
  if (actual !== expected) {
    throw new Error(`${message}: expected ${String(expected)}, got ${String(actual)}`);
  }
}

function assertDeepEqual<T>(actual: T, expected: T, message = "values differ") {
  const actualJson = JSON.stringify(actual);
  const expectedJson = JSON.stringify(expected);
  if (actualJson !== expectedJson) {
    throw new Error(`${message}: expected ${expectedJson}, got ${actualJson}`);
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

function tab(overrides: Partial<TabSnapshot> = {}): TabSnapshot {
  return {
    tab_id: 1,
    name: "tab",
    kind: "agent",
    focused_pane: 1,
    agent_running: false,
    agent_name: null,
    agent_session_id: null,
    agent_pane_id: null,
    agent_activity: null,
    needs_attention: false,
    notification_msg: null,
    listening_ports: [],
    panes: [pane()],
    ...overrides,
  };
}

function workspace(overrides: Partial<WorkspaceSnapshot> = {}): WorkspaceSnapshot {
  return {
    id: 1,
    name: "workspace",
    active_tab: 1,
    collapsed: false,
    repo_root: "/repo",
    branch_name: "main",
    is_worktree: false,
    tmux_backed: true,
    host_config_name: null,
    tab_count: 1,
    tabs: [tab()],
    ...overrides,
  };
}

function snapshot(overrides: Partial<TaarofStateSnapshot> = {}): TaarofStateSnapshot {
  return {
    schema: "test",
    generated_at_unix_ms: 0,
    session_name: "legacy-app",
    active_workspace: 1,
    active_tab: 1,
    workspaces: [workspace()],
    ...overrides,
  };
}

test("buildPaneTargets sorts high-signal panes before quiet panes", () => {
  const quiet = pane({ pane_id: 1, shell_running: false, attach_supported: false });
  const busy = pane({ pane_id: 2, has_child_process: true });
  const targets = buildPaneTargets(snapshot({
    workspaces: [workspace({ tabs: [tab({ needs_attention: true, agent_pane_id: 2, panes: [quiet, busy] })] })],
  }));

  assertDeepEqual(targets.map((target) => target.key), ["1:1:2", "1:1:1"]);
  assertEqual(isAttentionTarget(targets[0]), true);
  assertEqual(isBusyTarget(targets[0]), true);
});

test("applyManualOrder pins manual keys and preserves priority order for the rest", () => {
  const targets = buildPaneTargets(snapshot({
    workspaces: [workspace({ tabs: [tab({ panes: [pane({ pane_id: 1 }), pane({ pane_id: 2, has_child_process: true }), pane({ pane_id: 3 })] })] })],
  }));
  const ordered = applyManualOrder(targets, ["1:1:3"]);

  assertEqual(ordered[0].key, "1:1:3");
  assertDeepEqual(ordered.slice(1).map((target) => target.key), ["1:1:2", "1:1:1"]);
});

test("buildFilterOptions and active filter classify watch, signal, and done panes", () => {
  const done = pane({ pane_id: 1, shell_running: false, has_child_process: false });
  const active = pane({ pane_id: 2, has_child_process: true });
  const targets = buildPaneTargets(snapshot({
    workspaces: [workspace({ tabs: [tab({ agent_activity: { state: "running" }, panes: [done, active] })] })],
  }));
  const watchedKeys = new Set(["1:1:1"]);
  const options = buildFilterOptions(targets, watchedKeys);

  assertEqual(options.find((option) => option.id === "watch")?.count, 1);
  assertEqual(options.find((option) => option.id === "signal")?.count, 2);
  assertEqual(matchesActiveFilter(targets.find((target) => target.key === "1:1:1") as PaneTarget, "watch", watchedKeys), true);
  assertEqual(paneStatusLabel(targets.find((target) => target.key === "1:1:1") as PaneTarget), "live");
});

test("previewFromTerminalText keeps only the latest terminal lines", () => {
  const text = Array.from({ length: 20 }, (_, index) => `line-${index + 1}`).join("\n");

  assertEqual(previewFromTerminalText(text).startsWith("line-3"), true);
  assertEqual(previewFromTerminalText(text).endsWith("line-20"), true);
});

test("remote panes render unknown process state instead of idle or done", () => {
  const remotePane = pane({
    has_child_process: null,
    remote_shell: true,
    cwd_host: "devbox",
  });
  const [target] = buildPaneTargets(snapshot({
    workspaces: [workspace({ tabs: [tab({ panes: [remotePane] })] })],
  }));

  assertEqual(paneStatusLabel(target), "remote");
  assertEqual(activityLabel(target), "remote process state unavailable");
});

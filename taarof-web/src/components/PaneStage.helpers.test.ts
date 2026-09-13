import {
  classifyStageLayout,
  hasTerminalStageAttachmentTarget,
  paneStreamKey,
  projectStagePanes,
  selectStage,
  viewModeFromHash,
} from "./PaneStage.helpers.js";
import type { PaneSnapshot, TaarofStateSnapshot, TabSnapshot, WorkspaceSnapshot } from "../types.js";

function assertEqual<T>(actual: T, expected: T, message = "values differ") {
  if (actual !== expected) throw new Error(`${message}: expected ${String(expected)}, got ${String(actual)}`);
}

function assertDeepEqual<T>(actual: T, expected: T, message = "values differ") {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`${message}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}

function test(name: string, fn: () => void) {
  try { fn(); console.log(`PASS ${name}`); } catch (error) { console.error(`FAIL ${name}`); throw error; }
}

function pane(pane_id: number): PaneSnapshot {
  return { pane_id, shell_running: true, has_child_process: true, remote_shell: false, cwd: "/repo", cwd_host: null, tmux_session: "session", tmux_host: "local", attach_supported: true, attach_kind: "tmux" };
}
function tab(tab_id: number, panes = [pane(1)], overrides: Partial<TabSnapshot> = {}): TabSnapshot {
  return { tab_id, name: `tab-${tab_id}`, kind: "terminal", focused_pane: panes[0]?.pane_id ?? 0, agent_running: false, agent_name: null, needs_attention: false, notification_msg: null, listening_ports: [], panes, ...overrides };
}
function workspace(id: number, tabs = [tab(1)], overrides: Partial<WorkspaceSnapshot> = {}): WorkspaceSnapshot {
  return { id, name: `workspace-${id}`, active_tab: tabs[0]?.tab_id ?? 0, collapsed: false, repo_root: "/repo", branch_name: "main", is_worktree: false, tmux_backed: true, host_config_name: null, tab_count: tabs.length, tabs, ...overrides };
}
function snapshot(workspaces = [workspace(1)], active_workspace: number | null = 1): TaarofStateSnapshot {
  return { schema: "test", generated_at_unix_ms: 0, session_name: "legacy-app", active_workspace, active_tab: 1, workspaces };
}

test("selects active workspace and tab for the default stage", () => {
  const selected = selectStage(snapshot([workspace(1, [tab(11)]), workspace(2, [tab(21, [pane(7)])])], 2), { workspaceId: null, tabId: null, paneId: null });
  assertEqual(viewModeFromHash("#anything"), "panes");
  assertEqual(viewModeFromHash(""), "panes");
  assertEqual(selected.workspace?.id, 2);
  assertEqual(selected.tab?.tab_id, 21);
  assertEqual(selected.pane?.pane_id, 7);
});

test("keeps only explicit monitor agents and history hashes on secondary routes", () => {
  assertEqual(viewModeFromHash("#monitor"), "monitor");
  assertEqual(viewModeFromHash("#agents"), "agents");
  assertEqual(viewModeFromHash("#agent-session=abc"), "agents");
  assertEqual(viewModeFromHash("#history"), "history");
});

test("falls back to first selection and safely handles empty snapshots", () => {
  const selected = selectStage(snapshot([workspace(1, [tab(3, [pane(4)])])]), { workspaceId: 9, tabId: 8, paneId: 7 });
  assertDeepEqual([selected.workspace?.id, selected.tab?.tab_id, selected.pane?.pane_id], [1, 3, 4]);
  assertDeepEqual(selectStage(snapshot([], null), { workspaceId: null, tabId: null, paneId: null }), { workspace: null, tab: null, pane: null });
});

test("does not create attachment targets for empty or non-terminal tabs", () => {
  assertEqual(hasTerminalStageAttachmentTarget(null), false);
  assertEqual(hasTerminalStageAttachmentTarget(tab(2, [], { kind: "dashboard" })), false);
  assertEqual(hasTerminalStageAttachmentTarget(tab(3, [])), false);
  assertEqual(hasTerminalStageAttachmentTarget(tab(4, [pane(1)])), true);
});

test("classifies visible pane counts into deterministic stage layouts", () => {
  assertDeepEqual([0, 1, 2, 3, 4, 8].map(classifyStageLayout), ["empty", "one", "two", "three", "four", "four"]);
});

test("projects four panes, retaining order and promoting the selected overflow pane", () => {
  const panes = [1, 2, 3, 4, 5, 6].map(pane);
  const initial = projectStagePanes(panes, 1, []);
  assertDeepEqual(initial.visible.map((candidate) => candidate.pane_id), [1, 2, 3, 4]);
  assertDeepEqual(initial.hidden.map((candidate) => candidate.pane_id), [5, 6]);
  const promoted = projectStagePanes(panes, 6, initial.ordered.map((candidate) => candidate.pane_id));
  assertDeepEqual(promoted.visible.map((candidate) => candidate.pane_id), [1, 2, 3, 6]);
  assertDeepEqual(promoted.hidden.map((candidate) => candidate.pane_id), [4, 5]);
  const refocusedPeer = projectStagePanes(
    panes,
    1,
    promoted.ordered.map((candidate) => candidate.pane_id),
  );
  assertDeepEqual(refocusedPeer.visible.map((candidate) => candidate.pane_id), [1, 2, 3, 6]);
  assertDeepEqual(refocusedPeer.hidden.map((candidate) => candidate.pane_id), [4, 5]);
});

test("reconciles surviving pane order and keys streams by tab and pane", () => {
  const previous = [3, 1, 2, 4];
  const projected = projectStagePanes([pane(2), pane(3), pane(5), pane(1)], 3, previous);
  assertDeepEqual(projected.ordered.map((candidate) => candidate.pane_id), [3, 1, 2, 5]);
  assertEqual(paneStreamKey(10, 2), "10:2");
  assertEqual(paneStreamKey(11, 2), "11:2");
});

test("keeps unsupported panes representable without making them attach targets", () => {
  const unsupported = { ...pane(0), attach_supported: false, attach_kind: "unsupported" as const };
  const projected = projectStagePanes([unsupported, pane(1)], 0, []);
  assertDeepEqual(projected.visible.map((candidate) => candidate.pane_id), [0, 1]);
  assertEqual(projected.visible[0]?.attach_supported, false);
});

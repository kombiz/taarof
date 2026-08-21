import {
  closePanePreviewState,
  findLiveBindingTab,
  livePreviewPaneIds,
} from "./agentSessionPanes.js";
import { normalizePanePreviewText } from "./paneAttachFrames.js";
import type {
  LiveAgentBinding,
  PaneSnapshot,
  TaarofStateSnapshot,
  TabSnapshot,
  WorkspaceSnapshot,
} from "./types.js";

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

function pane(paneId: number): PaneSnapshot {
  return {
    pane_id: paneId,
    shell_running: true,
    has_child_process: false,
    remote_shell: false,
    cwd: `/repo/pane-${paneId}`,
    cwd_host: "local",
    tmux_session: "taarof",
    tmux_host: "local",
    attach_supported: true,
    attach_kind: "tmux",
    cols: 80,
    rows: 24,
  };
}

function tab(tabId: number, panes: PaneSnapshot[]): TabSnapshot {
  return {
    tab_id: tabId,
    name: `tab-${tabId}`,
    kind: "terminal",
    focused_pane: panes[0]?.pane_id ?? 0,
    agent_running: false,
    agent_name: null,
    needs_attention: false,
    notification_msg: null,
    discovery_cwd: "/repo",
    listening_ports: [],
    panes,
  };
}

function workspace(id: number, tabs: TabSnapshot[]): WorkspaceSnapshot {
  return {
    id,
    name: `workspace-${id}`,
    active_tab: tabs[0]?.tab_id ?? 0,
    collapsed: false,
    repo_root: "/repo",
    branch_name: "feature/pane-session-journal",
    is_worktree: false,
    tmux_backed: true,
    host_config_name: "local",
    tab_count: tabs.length,
    tabs,
  };
}

test("findLiveBindingTab returns the live tab with all sibling panes including pane 0", () => {
  const targetTab = tab(22, [pane(0), pane(3), pane(5)]);
  const snapshot: TaarofStateSnapshot = {
    schema: "taarof.state.v1",
    generated_at_unix_ms: 1,
    session_name: "taarof web",
    active_workspace: 1,
    active_tab: 10,
    workspaces: [
      workspace(1, [tab(10, [pane(1)])]),
      workspace(7, [tab(21, [pane(2)]), targetTab]),
    ],
  };
  const binding: LiveAgentBinding = {
    agent: "codex",
    session_id: "session-123",
    cwd: "/repo",
    workspace_id: 7,
    workspace_name: "workspace-7",
    tab_id: 22,
    tab_name: "tab-22",
    pane_id: 3,
  };

  const result = findLiveBindingTab(snapshot, binding);

  assert(result !== null, "binding should resolve to a live workspace and tab");
  assert(result.workspace.id === 7, "resolved workspace should match binding workspace id");
  assert(result.tab.tab_id === 22, "resolved tab should match binding tab id");
  assert(
    result.tab.panes.map((candidate) => candidate.pane_id).join(",") === "0,3,5",
    "resolved tab should preserve every sibling pane, including pane id 0",
  );
});

test("normalizePanePreviewText strips ANSI and control sequences for preformatted previews", () => {
  const normalized = normalizePanePreviewText("\x1b[31mred\x1b[0m\r\nnext\x07\rline");

  assert(normalized === "red\nnext\nline", "terminal preview text should be plain text");
});

test("closePanePreviewState preserves error messages after the socket closes", () => {
  const closed = closePanePreviewState("error", "attach slots exhausted");

  assert(closed.phase === "error", "error phase should remain visible after close");
  assert(
    closed.message === "attach slots exhausted",
    "error message should not be replaced by a generic close message",
  );
});

test("livePreviewPaneIds caps attachable panes without dropping pane 0", () => {
  const panes = [pane(0), pane(1), pane(2), pane(3), pane(4), pane(5), pane(6), pane(7)];
  panes[2] = { ...panes[2], attach_supported: false };

  const paneIds = livePreviewPaneIds(panes, 4);

  assert(
    paneIds.join(",") === "0,1,3,4",
    "live preview cap should include the first attachable pane ids, including pane 0",
  );
});

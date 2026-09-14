import type { PaneSnapshot, TabSnapshot, TaarofStateSnapshot, WorkspaceSnapshot } from "../types.js";

export type StageViewMode = "panes" | "monitor" | "agents" | "history";
export type StageLayout = "empty" | "one" | "two" | "three" | "four";

export interface StageSelection {
  workspaceId: number | null;
  tabId: number | null;
  paneId: number | null;
}

export interface StageTarget {
  workspace: WorkspaceSnapshot | null;
  tab: TabSnapshot | null;
  pane: PaneSnapshot | null;
}

export interface StageProjection {
  ordered: PaneSnapshot[];
  visible: PaneSnapshot[];
  hidden: PaneSnapshot[];
}

export function viewModeFromHash(hash: string): StageViewMode {
  if (hash === "#monitor") return "monitor";
  if (hash === "#agents" || hash.startsWith("#agent-session=")) return "agents";
  if (hash === "#history") return "history";
  return "panes";
}

export function selectWorkspace(
  snapshot: TaarofStateSnapshot | null,
  selectedWorkspaceId: number | null,
): WorkspaceSnapshot | null {
  if (!snapshot?.workspaces.length) return null;
  return snapshot.workspaces.find((workspace) => workspace.id === selectedWorkspaceId)
    ?? snapshot.workspaces.find((workspace) => workspace.id === snapshot.active_workspace)
    ?? snapshot.workspaces[0]
    ?? null;
}

export function selectTab(
  workspace: WorkspaceSnapshot | null,
  selectedTabId: number | null,
): TabSnapshot | null {
  if (!workspace?.tabs.length) return null;
  return workspace.tabs.find((tab) => tab.tab_id === selectedTabId)
    ?? workspace.tabs.find((tab) => tab.tab_id === workspace.active_tab)
    ?? workspace.tabs[0]
    ?? null;
}

export function selectPane(tab: TabSnapshot | null, selectedPaneId: number | null): PaneSnapshot | null {
  if (!tab?.panes.length) return null;
  return tab.panes.find((pane) => pane.pane_id === selectedPaneId)
    ?? tab.panes.find((pane) => pane.pane_id === tab.focused_pane)
    ?? tab.panes[0]
    ?? null;
}

export function selectStage(snapshot: TaarofStateSnapshot | null, selection: StageSelection): StageTarget {
  const workspace = selectWorkspace(snapshot, selection.workspaceId);
  const tab = selectTab(workspace, selection.tabId);
  return { workspace, tab, pane: selectPane(tab, selection.paneId) };
}

export function hasTerminalStageAttachmentTarget(tab: TabSnapshot | null | undefined) {
  return tab?.kind === "terminal" && tab.panes.length > 0;
}

export function classifyStageLayout(count: number): StageLayout {
  if (count <= 0) return "empty";
  if (count === 1) return "one";
  if (count === 2) return "two";
  if (count === 3) return "three";
  return "four";
}

export function paneStreamKey(tabId: number, paneId: number): string {
  return `${tabId}:${paneId}`;
}

/**
 * Reconcile snapshot churn without letting its incidental array order restart
 * terminal streams. The caller retains the complete prior order (including
 * hidden panes); a hidden selected pane replaces only the final peer.
 */
export function projectStagePanes(
  panes: PaneSnapshot[],
  selectedPaneId: number | null,
  previousPaneIds: number[],
): StageProjection {
  const byId = new Map(panes.map((pane) => [pane.pane_id, pane]));
  let ordered = [
    ...previousPaneIds.map((id) => byId.get(id)).filter((pane): pane is PaneSnapshot => Boolean(pane)),
    ...panes.filter((pane) => !previousPaneIds.includes(pane.pane_id)),
  ];
  const selected = selectedPaneId === null ? null : byId.get(selectedPaneId) ?? null;
  const selectedIndex = selected === null
    ? -1
    : ordered.findIndex((pane) => pane.pane_id === selected.pane_id);
  if (selected !== null && selectedIndex >= 4) {
    ordered = ordered.filter((pane) => pane.pane_id !== selected.pane_id);
    ordered.splice(3, 0, selected);
  }
  const visible = ordered.slice(0, 4);
  const visibleIds = new Set(visible.map((pane) => pane.pane_id));
  return { ordered, visible, hidden: ordered.filter((pane) => !visibleIds.has(pane.pane_id)) };
}

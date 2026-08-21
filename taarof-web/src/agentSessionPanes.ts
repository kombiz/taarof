import { type PaneAttachPhase } from "./paneAttachFrames.js";
import type {
  LiveAgentBinding,
  PaneSnapshot,
  TaarofStateSnapshot,
  TabSnapshot,
  WorkspaceSnapshot,
} from "./types.js";

export const SESSION_TAB_PANE_LIVE_PREVIEW_LIMIT = 6;

export interface LiveBindingTab {
  workspace: WorkspaceSnapshot;
  tab: TabSnapshot;
}

export function findLiveBindingTab(
  snapshot: TaarofStateSnapshot | null,
  binding: LiveAgentBinding | null | undefined,
): LiveBindingTab | null {
  if (!snapshot || !binding) {
    return null;
  }

  const workspace = snapshot.workspaces.find(
    (candidate) => candidate.id === binding.workspace_id,
  );
  const tab = workspace?.tabs.find((candidate) => candidate.tab_id === binding.tab_id);

  return workspace && tab ? { workspace, tab } : null;
}

export function closePanePreviewState(
  phase: PaneAttachPhase,
  message: string,
): { phase: PaneAttachPhase; message: string } {
  if (phase === "error" || phase === "unsupported") {
    return { phase, message };
  }

  return { phase: "closed", message: "Pane attach closed." };
}

export function livePreviewPaneIds(
  panes: readonly PaneSnapshot[],
  limit = SESSION_TAB_PANE_LIVE_PREVIEW_LIMIT,
): number[] {
  return panes
    .filter((pane) => pane.attach_supported)
    .slice(0, limit)
    .map((pane) => pane.pane_id);
}

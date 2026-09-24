import type {
  AgentBadgeSnapshot,
  AttentionEvidenceSnapshot,
  PaneSnapshot,
  TaarofStateSnapshot,
  TabAgentSnapshot,
  TabSnapshot,
  TaskTruthSnapshot,
  WorkRecordSnapshot,
  WorkspaceSnapshot,
} from "./types.js";

export const MONITOR_ORDER_STORAGE_KEY = "taarof.web.monitorOrder";
export const MONITOR_WATCH_STORAGE_KEY = "taarof.web.monitorWatch";

export type MonitorContentFilter = "all" | "signal" | "attention" | "live" | "busy" | "done";
export type MonitorFilter = MonitorContentFilter | "watch";

export interface PaneTarget {
  key: string;
  sessionName: string;
  workspace: WorkspaceSnapshot;
  tab: TabSnapshot;
  pane: PaneSnapshot;
  order: number;
  priority: number;
}

export interface WorkFocusTarget {
  workspaceId: number;
  tabId: number;
  paneId: number;
}

const titleCaseTruth = (value: string) => {
  if (value.toLowerCase() === "github") {
    return "GitHub";
  }
  return value
    .replace(/_/g, " ")
    .replace(/^./, (character: string) => character.toUpperCase());
};

function formatWorkAge(tsUnixMs: number, nowUnixMs: number): string {
  const seconds = Math.max(0, Math.floor((nowUnixMs - tsUnixMs) / 1000));
  if (seconds <= 9) {
    return "just now";
  }
  if (seconds <= 59) {
    return `${seconds}s ago`;
  }
  if (seconds <= 3_599) {
    return `${Math.floor(seconds / 60)}m ago`;
  }
  if (seconds <= 86_399) {
    return `${Math.floor(seconds / 3_600)}h ago`;
  }
  return `${Math.floor(seconds / 86_400)}d ago`;
}

export function workTruthLabels(item: TaskTruthSnapshot, nowUnixMs = Date.now()): string[] {
  const checked = item.last_checked_unix_ms == null
    ? "Never checked"
    : `Checked ${formatWorkAge(item.last_checked_unix_ms, nowUnixMs)}`;
  const labels = [
    titleCaseTruth(item.canonical),
    titleCaseTruth(item.binding),
    titleCaseTruth(item.execution),
    titleCaseTruth(item.origin),
    `${titleCaseTruth(item.verification)} (${item.verification_source})`,
    checked,
  ];
  if (item.mismatch) {
    labels.push(`${titleCaseTruth(item.mismatch.source)} mismatch: ${item.mismatch.detail}`);
  }
  return labels;
}

export function visibleWorkTruth(
  items: TaskTruthSnapshot[],
  showHistory: boolean,
): TaskTruthSnapshot[] {
  return items.filter((item) =>
    showHistory
      ? item.origin === "historical" || item.canonical === "done" || item.canonical === "cancelled"
      : item.origin !== "historical" && item.canonical !== "done" && item.canonical !== "cancelled",
  );
}

export function liveOpenWorkCount(items: TaskTruthSnapshot[]): number {
  return items.filter((item) => item.counts_as_live_open).length;
}

/**
 * Resolve a historical Work identity through the live stable-origin legend.
 * Persisted numeric ids are deliberately ignored because ids are recycled on
 * restore and could otherwise focus an unrelated pane.
 */
export function resolveWorkFocusTarget(
  snapshot: TaarofStateSnapshot | null,
  identity: WorkRecordSnapshot["identity"],
): WorkFocusTarget | null {
  if (!snapshot?.work || identity.session !== snapshot.work.session) {
    return null;
  }
  const current = snapshot.work.legend.find(
    (item) =>
      item.origin_state === "live" &&
      item.workspace_origin.length > 0 &&
      item.tab_origin === identity.tab_origin &&
      item.pane_origin === identity.pane_origin,
  );
  return current
    ? {
        workspaceId: current.workspace_id,
        tabId: current.tab_id,
        paneId: current.pane_id,
      }
    : null;
}

export interface MonitorFilterOption {
  id: MonitorFilter;
  label: string;
  count: number;
}

export interface StringArrayStorage {
  getItem(storageKey: string): string | null;
  setItem(storageKey: string, value: string): void;
}

export function readStoredStringArray(
  storage: Pick<StringArrayStorage, "getItem">,
  storageKey: string,
): string[] {
  try {
    const raw = storage.getItem(storageKey);
    const parsed = raw ? (JSON.parse(raw) as unknown) : null;
    return Array.isArray(parsed) ? parsed.filter((value) => typeof value === "string") : [];
  } catch {
    return [];
  }
}

export function writeStoredStringArray(
  storage: Pick<StringArrayStorage, "setItem">,
  storageKey: string,
  values: string[],
) {
  storage.setItem(storageKey, JSON.stringify(values));
}

export function readStoredMonitorOrder(storage: Pick<StringArrayStorage, "getItem">): string[] {
  return readStoredStringArray(storage, MONITOR_ORDER_STORAGE_KEY);
}

export function writeStoredMonitorOrder(
  storage: Pick<StringArrayStorage, "setItem">,
  keys: string[],
) {
  writeStoredStringArray(storage, MONITOR_ORDER_STORAGE_KEY, keys);
}

export function readStoredWatchedKeys(storage: Pick<StringArrayStorage, "getItem">): string[] {
  return readStoredStringArray(storage, MONITOR_WATCH_STORAGE_KEY);
}

export function writeStoredWatchedKeys(
  storage: Pick<StringArrayStorage, "setItem">,
  keys: string[],
) {
  writeStoredStringArray(storage, MONITOR_WATCH_STORAGE_KEY, keys);
}

export function panePriority(
  snapshot: TaarofStateSnapshot,
  workspace: WorkspaceSnapshot,
  tab: TabSnapshot,
  pane: PaneSnapshot,
): number {
  let score = 0;
  const agent = paneAgent(tab, pane.pane_id);
  const attention = pane.attention ?? agent?.attention ?? null;
  const hasCanonicalAttention = pane.attention !== undefined || agent?.attention !== undefined;

  if (workspace.id === snapshot.active_workspace) {
    score += 120;
  }
  if (tab.tab_id === snapshot.active_tab) {
    score += 180;
  }
  if (tab.tab_id === workspace.active_tab) {
    score += 80;
  }
  if (attention || (!hasCanonicalAttention && tab.needs_attention)) {
    score += 220;
  }
  if (agent?.state === "working" || (!tab.agents?.length && tab.agent_running)) {
    score += 190;
  }
  if (!hasCanonicalAttention && tab.agent_activity?.state === "waiting-input") {
    score += 180;
  }
  if (!hasCanonicalAttention && tab.agent_activity?.state === "errored") {
    score += 170;
  }
  if (tab.agent_pane_id === pane.pane_id) {
    score += 140;
  }
  if (pane.has_child_process) {
    score += 110;
  }
  if (pane.shell_running) {
    score += 30;
  }
  if (pane.attach_supported) {
    score += 10;
  }

  return score;
}

export function buildPaneTargets(snapshot: TaarofStateSnapshot | null): PaneTarget[] {
  if (!snapshot) {
    return [];
  }

  const targets: PaneTarget[] = [];
  let order = 0;

  snapshot.workspaces.forEach((workspace) => {
    workspace.tabs.forEach((tab) => {
      tab.panes.forEach((pane) => {
        targets.push({
          key: `${workspace.id}:${tab.tab_id}:${pane.pane_id}`,
          sessionName: snapshot.session_name,
          workspace,
          tab,
          pane,
          order,
          priority: panePriority(snapshot, workspace, tab, pane),
        });
        order += 1;
      });
    });
  });

  return targets.sort((left, right) => {
    if (right.priority !== left.priority) {
      return right.priority - left.priority;
    }
    return left.order - right.order;
  });
}

export function applyManualOrder(targets: PaneTarget[], orderedKeys: string[]): PaneTarget[] {
  const order = new Map(orderedKeys.map((key, index) => [key, index]));
  return [...targets].sort((left, right) => {
    const leftManual = order.get(left.key);
    const rightManual = order.get(right.key);

    if (leftManual !== undefined || rightManual !== undefined) {
      if (leftManual === undefined) {
        return 1;
      }
      if (rightManual === undefined) {
        return -1;
      }
      return leftManual - rightManual;
    }

    if (right.priority !== left.priority) {
      return right.priority - left.priority;
    }
    return left.order - right.order;
  });
}

export function isAttentionTarget(target: PaneTarget): boolean {
  const agent = paneAgent(target.tab, target.pane.pane_id);
  if (target.pane.attention || agent?.attention) {
    return true;
  }
  if (target.pane.attention !== undefined || agent?.attention !== undefined) {
    return false;
  }
  return (
    target.tab.needs_attention ||
    target.tab.agent_activity?.state === "waiting-input" ||
    target.tab.agent_activity?.state === "errored"
  );
}

export function isLiveTarget(target: PaneTarget): boolean {
  const agent = paneAgent(target.tab, target.pane.pane_id);
  if (agent?.state) {
    return agent.state === "working";
  }
  if (target.tab.agents?.length) {
    return false;
  }
  return target.tab.agent_running || target.tab.agent_activity?.state === "running";
}

export function isBusyTarget(target: PaneTarget): boolean {
  return target.pane.has_child_process === true;
}

export function isDoneTarget(target: PaneTarget): boolean {
  return (
    !isAttentionTarget(target) &&
    !isLiveTarget(target) &&
    target.pane.has_child_process === false
  );
}

export function matchesMonitorFilter(target: PaneTarget, filter: MonitorContentFilter): boolean {
  switch (filter) {
    case "attention":
      return isAttentionTarget(target);
    case "live":
      return isLiveTarget(target);
    case "busy":
      return isBusyTarget(target);
    case "done":
      return isDoneTarget(target);
    case "signal":
      return isAttentionTarget(target) || isLiveTarget(target) || isBusyTarget(target);
    default:
      return true;
  }
}

export function matchesActiveFilter(
  target: PaneTarget,
  filter: MonitorFilter,
  watchedKeys: Set<string>,
): boolean {
  if (filter === "watch") {
    return watchedKeys.has(target.key);
  }

  return matchesMonitorFilter(target, filter);
}

export function buildFilterOptions(
  targets: PaneTarget[],
  watchedKeys: Set<string>,
): MonitorFilterOption[] {
  return [
    {
      id: "signal",
      label: "Signal",
      count: targets.filter((target) => matchesMonitorFilter(target, "signal")).length,
    },
    {
      id: "watch",
      label: "Watch",
      count: targets.filter((target) => watchedKeys.has(target.key)).length,
    },
    { id: "attention", label: "Attention", count: targets.filter(isAttentionTarget).length },
    { id: "live", label: "Live", count: targets.filter(isLiveTarget).length },
    { id: "busy", label: "Busy", count: targets.filter(isBusyTarget).length },
    { id: "done", label: "Done", count: targets.filter(isDoneTarget).length },
    { id: "all", label: "All", count: targets.length },
  ];
}


/**
 * The agent instance running in a specific pane of a tab, if any. Reads the
 * per-pane `agents[]` the native app now emits, so the browser Monitor can
 * distinguish two same-kind agents (codex #1 vs codex #2) the same way the
 * desktop sidebar does.
 */
export function paneAgent(tab: TabSnapshot, paneId: number): TabAgentSnapshot | null {
  return tab.agents?.find((agent) => agent.pane_id === paneId) ?? null;
}

export function paneAttention(target: PaneTarget): AttentionEvidenceSnapshot | null {
  return (
    target.pane.attention ??
    paneAgent(target.tab, target.pane.pane_id)?.attention ??
    null
  );
}

/**
 * Human-facing label for a pane's agent, preferring the disambiguated
 * `instance_label` (e.g. `codex #2`) and falling back to the bare agent name.
 * Returns null when the pane has no agent.
 */
export function paneAgentLabel(tab: TabSnapshot, paneId: number): string | null {
  const agent = paneAgent(tab, paneId);
  if (!agent) {
    return null;
  }
  return agent.instance_label ?? agent.agent_name ?? null;
}

/**
 * Per-pane agent detail for a tab, in pane order. Each entry pairs the
 * disambiguated label with its pane and current activity state text, mirroring
 * the native tab-row tooltip. Used to show multi-agent tabs distinctly.
 */
/**
 * Neutral fallback badge for an agent whose `badge` field is absent (older
 * payloads) or whose source name is empty. Mirrors the native generic
 * fallback in `crate::agents::agent_badge`: uppercased/truncated source label,
 * `\u2022` glyph, `agent-generic` color token.
 */
export function fallbackAgentBadge(agentName: string | null | undefined): AgentBadgeSnapshot {
  const source = (agentName ?? "").trim();
  const shortLabel =
    source.length === 0
      ? "AGENT"
      : source.replace(/\s+/g, "").slice(0, 4).toUpperCase();
  return {
    name: source.length === 0 ? "agent" : source.toLowerCase(),
    short_label: shortLabel,
    glyph: "\u2022",
    color_token: "agent-generic",
    known: false,
  };
}

/**
 * Resolve the stable identity badge for a pane's agent, consumed straight from
 * the payload the native app emits (single source of truth — no parallel TS
 * table). Falls back to a generic badge when the payload predates badges.
 * Returns null when the pane has no agent.
 */
export function paneAgentBadge(tab: TabSnapshot, paneId: number): AgentBadgeSnapshot | null {
  const agent = paneAgent(tab, paneId);
  if (!agent) {
    return null;
  }
  return agent.badge ?? fallbackAgentBadge(agent.agent_name);
}

export function tabAgentInstances(
  tab: TabSnapshot,
): Array<{
  paneId: number;
  label: string;
  state: string | null;
  badge: AgentBadgeSnapshot;
}> {
  return (tab.agents ?? [])
    .filter((agent) => Boolean(agent.instance_label ?? agent.agent_name))
    .map((agent) => ({
      paneId: agent.pane_id,
      label: (agent.instance_label ?? agent.agent_name) as string,
      state: agent.activity?.text ?? agent.activity?.state ?? null,
      badge: agent.badge ?? fallbackAgentBadge(agent.agent_name),
    }));
}

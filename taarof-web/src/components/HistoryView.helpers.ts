import type {
  HistoryRecord,
  HistoryStatusSnapshot,
  TaarofStateSnapshot,
} from "../types.js";

export interface HistoryNavigation {
  pane?: { workspaceId: number; tabId: number; paneId: number; checkedAtUnixMs: number };
  task?: { taskId: string; workspaceId: number; tabId: number; paneId: number; checkedAtUnixMs: number };
  pullRequest?: { url: string; checkedAtUnixMs: number };
  paneNote?: string;
  taskNote?: string;
  pullRequestNote?: string;
}

export interface HistoryRowProjection {
  record: HistoryRecord;
  historicalLabel: "Historical observation";
  typeLabel: string;
  sourceLabel: string;
  authorityLabel: string;
  verificationLabel: string;
  observedAtUnixMs: number;
  snippet: string;
  navigation: HistoryNavigation;
}

export function projectHistoryRow(
  record: HistoryRecord,
  snapshot: TaarofStateSnapshot | null,
): HistoryRowProjection {
  return {
    record,
    historicalLabel: "Historical observation",
    typeLabel: `${record.record_type} / ${record.subtype}`,
    sourceLabel:
      record.source_space && record.source_seq !== undefined
        ? `${record.source_space} #${record.source_seq}`
        : record.source_space ??
          (record.source_seq !== undefined ? `sequence #${record.source_seq}` : "local history"),
    authorityLabel: record.authority ?? "observational",
    verificationLabel: record.verification ?? "unverified",
    observedAtUnixMs: record.ts_unix_ms,
    snippet: record.summary ?? record.subtype,
    navigation: resolveHistoryNavigation(record, snapshot),
  };
}

export function resolveHistoryNavigation(
  record: HistoryRecord,
  snapshot: TaarofStateSnapshot | null,
): HistoryNavigation {
  const result: HistoryNavigation = {};
  const liveLegend = (snapshot?.work?.legend ?? []).filter(
    (item) => item.origin_state === "live",
  );
  if (record.pane_origin) {
    const matches = liveLegend.filter(
      (item) =>
        item.pane_origin === record.pane_origin &&
        (!record.tab_origin || item.tab_origin === record.tab_origin),
    );
    if (matches.length === 1 && snapshot) {
      const [target] = matches;
      result.pane = {
        workspaceId: target.workspace_id,
        tabId: target.tab_id,
        paneId: target.pane_id,
        checkedAtUnixMs: snapshot.generated_at_unix_ms,
      };
    } else {
      result.paneNote = matches.length === 0 ? "Pane no longer present" : "Pane identity is ambiguous";
    }
  }

  if (record.task_id) {
    const matches = liveLegend.filter(
      (item) =>
        item.task_id === record.task_id &&
        (!record.workspace_origin || item.workspace_origin === record.workspace_origin),
    );
    const matchingTruth = snapshot?.work?.truth?.filter(
      (item) =>
        item.task_id === record.task_id &&
        item.origin === "live" &&
        item.canonical !== "absent" &&
        item.canonical !== "unknown" &&
        matches.some((target) => target.pane_origin === item.pane_origin),
    ) ?? [];
    if (matches.length === 1 && matchingTruth.length === 1 && snapshot) {
      const [target] = matches;
      const [truth] = matchingTruth;
      result.task = {
        taskId: record.task_id,
        workspaceId: target.workspace_id,
        tabId: target.tab_id,
        paneId: target.pane_id,
        checkedAtUnixMs: truth.last_checked_unix_ms ?? snapshot.generated_at_unix_ms,
      };
    } else {
      result.taskNote =
        matches.length === 0 || matchingTruth.length === 0
          ? "Task no longer present in a live workspace"
          : "Task identity is ambiguous";
    }
  }

  const prNumber = historyPullRequestNumber(record);
  if (record.repository && prNumber !== null) {
    const matches = (snapshot?.work?.all_entries ?? [])
      .filter(
        (entry) =>
          entry.reconciliation.status === "verified" &&
          entry.reconciliation.origin_status === "live" &&
          entry.record.pull_request?.repository.toLowerCase() === record.repository?.toLowerCase() &&
          entry.record.pull_request?.number === prNumber,
      )
      .map((entry) => ({
        url: entry.record.pull_request?.url ?? "",
        checkedAtUnixMs:
          entry.reconciliation.checked_at_unix_ms ?? snapshot?.generated_at_unix_ms ?? 0,
      }));
    const unique = matches.reduce<typeof matches>((current, candidate) => {
      const existing = current.find((other) => other.url === candidate.url);
      if (!existing) {
        current.push(candidate);
      } else if (candidate.checkedAtUnixMs > existing.checkedAtUnixMs) {
        existing.checkedAtUnixMs = candidate.checkedAtUnixMs;
      }
      return current;
    }, []);
    if (unique.length === 1) {
      result.pullRequest = unique[0];
    } else {
      result.pullRequestNote =
        unique.length === 0
          ? "Pull request no longer present in verified live state"
          : "Pull request identity is ambiguous";
    }
  }
  return result;
}

function historyPullRequestNumber(record: HistoryRecord): number | null {
  const pullRequest = record.attrs?.pull_request;
  if (!pullRequest || typeof pullRequest !== "object" || Array.isArray(pullRequest)) {
    return null;
  }
  const number = (pullRequest as Record<string, unknown>).number;
  return typeof number === "number" && Number.isSafeInteger(number) && number > 0 ? number : null;
}

export type HistoryRenderState =
  | "loading"
  | "empty"
  | "ready"
  | "stale"
  | "unavailable"
  | "failed";

export function historyRenderState(input: {
  loading: boolean;
  records: HistoryRecord[];
  error: string | null;
  historyStatus?: HistoryStatusSnapshot;
}): HistoryRenderState {
  if (input.loading) return "loading";
  if (input.historyStatus && !input.historyStatus.available) return "unavailable";
  if (input.error) return "failed";
  if (input.records.length === 0) return "empty";
  if (
    input.historyStatus &&
    (input.historyStatus.state !== "ok" || input.historyStatus.maintenance?.last_result === "failed")
  ) {
    return "stale";
  }
  return "ready";
}

export function sanitizedHistoryExport(records: HistoryRecord[]): string {
  return JSON.stringify(records, null, 2);
}

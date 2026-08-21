import {
  historyRenderState,
  projectHistoryRow,
  resolveHistoryNavigation,
  sanitizedHistoryExport,
} from "./HistoryView.helpers.js";
import type { HistoryRecord, TaarofStateSnapshot } from "../types.js";

function assert(condition: unknown, message: string) {
  if (!condition) throw new Error(message);
}
function test(name: string, fn: () => void) {
  try { fn(); console.log(`PASS ${name}`); } catch (error) { console.error(`FAIL ${name}`); throw error; }
}

const record: HistoryRecord = {
  id: 1,
  ts_unix_ms: 10,
  record_type: "work",
  subtype: "started",
  source_space: "work",
  source_seq: 7,
  session: "test",
  workspace_origin: "workspace-a",
  tab_origin: "tab-a",
  pane_origin: "pane-a",
  task_id: "EXAMPLE-135",
  repository: "owner/repo",
  authority: "plan_canonical",
  verification: "canonical_file",
  summary: "Implementation started",
  attrs: { pull_request: { number: 42 } },
};

const snapshot: TaarofStateSnapshot = {
  schema: "test",
  generated_at_unix_ms: 20,
  session_name: "test",
  active_workspace: 1,
  active_tab: 2,
  workspaces: [],
  work: {
    schema: "test",
    session: "test",
    filter: {},
    palette: {},
    overflow_count: 0,
    restore: { status: "ok", loaded_records: 1, rejected_records: 0 },
    legend: [{
      pane_origin: "pane-a", workspace_origin: "workspace-a", tab_origin: "tab-a",
      marker: "A", color_slot: 0, workspace_id: 1, tab_id: 2, pane_id: 3,
      workspace_name: "ws", tab_name: "tab", task_id: "EXAMPLE-135", origin_state: "live",
    }],
    entries: [],
    all_entries: [{
      record: {
        seq: 7, ts_unix_ms: 10, kind: "pull_request", summary: "PR",
        evidence_source: "github", authority: "github_canonical", verification: "github_api",
        identity: {
          session: "test", workspace_origin: "workspace-a", tab_origin: "tab-a",
          pane_origin: "pane-a", workspace_id: 1, workspace_name: "ws", tab_id: 2,
          tab_name: "tab", pane_id: 3, task_id: "EXAMPLE-135",
        },
        pull_request: { repository: "owner/repo", number: 42, title: "PR", url: "https://github.com/owner/repo/pull/42", state: "open" },
      },
      marker: "A", color_slot: 0,
      reconciliation: { status: "verified", source: "github", reason: "exact", origin_status: "live", checked_at_unix_ms: 21 },
    }],
    truth: [{
      pane_origin: "pane-a", task_id: "EXAMPLE-135", canonical: "in_progress", binding: "bound",
      execution: "running", origin: "live", verification: "verified", verification_source: ".plan",
      last_checked_unix_ms: 22, counts_as_live_open: true,
    }],
  },
};

test("projection keeps historical and current provenance separate", () => {
  const row = projectHistoryRow(record, snapshot);
  assert(row.historicalLabel === "Historical observation", "history label missing");
  assert(row.sourceLabel === "work #7", "source label mismatch");
  assert(row.observedAtUnixMs === 10, "observed time mismatch");
  assert(row.navigation.task?.checkedAtUnixMs === 22, "checked time mismatch");
});

test("navigation requires exact non-ambiguous live identities", () => {
  const exact = resolveHistoryNavigation(record, snapshot);
  assert(exact.pane?.paneId === 3 && exact.task?.taskId === "EXAMPLE-135", "exact navigation missing");
  assert(exact.pullRequest?.url.endsWith("/42"), "exact PR missing");
  const missing = resolveHistoryNavigation(record, { ...snapshot, work: undefined });
  assert(!missing.pane && missing.paneNote?.includes("no longer"), "missing pane should not navigate");
  const duplicate = structuredClone(snapshot);
  duplicate.work?.legend.push({ ...(duplicate.work.legend[0]) });
  assert(resolveHistoryNavigation(record, duplicate).paneNote?.includes("ambiguous"), "ambiguous pane should not navigate");
  const conflictingPullRequest = structuredClone(snapshot);
  conflictingPullRequest.work?.all_entries?.push({
    ...structuredClone(conflictingPullRequest.work.all_entries[0]),
    record: {
      ...structuredClone(conflictingPullRequest.work.all_entries[0].record),
      pull_request: {
        ...structuredClone(conflictingPullRequest.work.all_entries[0].record.pull_request!),
        url: "https://github.com/other/repo/pull/42",
      },
    },
  });
  assert(
    resolveHistoryNavigation(record, conflictingPullRequest).pullRequestNote?.includes("ambiguous"),
    "conflicting exact PR identities should not navigate",
  );
});

test("render state covers loading empty stale unavailable and failed", () => {
  assert(historyRenderState({ loading: true, records: [], error: null }) === "loading", "loading");
  assert(historyRenderState({ loading: false, records: [], error: null }) === "empty", "empty");
  assert(historyRenderState({ loading: false, records: [record], error: null, historyStatus: { available: true, enabled: true, state: "degraded" } }) === "stale", "stale");
  assert(historyRenderState({ loading: false, records: [], error: null, historyStatus: { available: false, enabled: false, state: "disabled" } }) === "unavailable", "unavailable");
  assert(historyRenderState({ loading: false, records: [], error: "boom" }) === "failed", "failed");
});

test("export cannot acquire terminal transcript input token argv or env fields", () => {
  const output = sanitizedHistoryExport([record]);
  for (const forbidden of ["terminal", "transcript", "input", "token", "argv", "env"]) {
    assert(!output.includes(`\"${forbidden}\"`), `${forbidden} leaked`);
  }
});

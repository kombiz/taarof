export type PaneAttachKind = "tmux" | "vte" | "unsupported";

export interface ApiEnvelope<T> {
  ok: boolean;
  data: T;
}

export interface DashboardSnapshot {
  probe_state?: string;
  observed_at_unix_ms?: number | null;
  checked_at_unix_ms?: number | null;
  error?: string | null;
  sessions?: unknown[];
  hosts?: unknown[];
}

export interface AgentActivitySnapshot {
  state?: string;
  text?: string | null;
  source?: string | null;
  origin?: string | null;
}

export interface AgentJobSnapshot {
  workspace_id: number;
  workspace_name: string;
  tab_id: number;
  tab_name: string;
  pane_id?: number | null;
  agent_name?: string | null;
  session_id?: string | null;
  instance_label?: string | null;
  activity?: AgentActivitySnapshot | null;
}

export type RuntimeProbeTruthState =
  | "ok"
  | "absent"
  | "partial"
  | "stale"
  | "failed"
  | "unknown";

export type ProbeState = "ok" | "stale" | "unknown" | "error";

export interface RuntimeProbeComponentSnapshot {
  state: ProbeState;
  observed_at_unix_ms?: number | null;
  checked_at_unix_ms?: number | null;
  age_ms?: number | null;
  error?: string | null;
}

export interface RuntimeProbeSnapshot {
  state: RuntimeProbeTruthState;
  process: RuntimeProbeComponentSnapshot;
  ports: RuntimeProbeComponentSnapshot;
  checked_at_unix_ms?: number | null;
  cache_ttl_ms?: number;
}

export interface TaarofStateSnapshot {
  schema: string;
  generated_at_unix_ms: number;
  session_name: string;
  active_workspace: number | null;
  active_tab: number | null;
  workspaces: WorkspaceSnapshot[];
  dashboard?: DashboardSnapshot;
  agent_jobs?: AgentJobSnapshot[];
  work?: WorkStreamSnapshot;
  history?: HistoryStatusSnapshot;
  runtime_probe?: RuntimeProbeSnapshot;
}

export type HistoryOrder = "asc" | "desc";

export interface HistoryFilters {
  from_ts?: number;
  to_ts?: number;
  record_type?: "event" | "diagnostic" | "work";
  session?: string;
  workspace?: string;
  pane?: string;
  task?: string;
  repository?: string;
  authority?: string;
  verification?: string;
  severity?: "info" | "warn" | "error";
  text?: string;
  order?: HistoryOrder;
  scan_budget?: number;
}

export interface HistoryRecord {
  id: number;
  ts_unix_ms: number;
  record_type: string;
  subtype: string;
  source_space?: string;
  source_seq?: number;
  session: string;
  workspace_origin?: string;
  tab_origin?: string;
  pane_origin?: string;
  task_id?: string;
  repository?: string;
  authority?: string;
  verification?: string;
  level?: string;
  summary?: string;
  attrs?: Record<string, unknown>;
}

export interface HistoryPage {
  schema: string;
  since_id: number | null;
  limit: number;
  next_id: number;
  has_more: boolean;
  scanned: number;
  scan_exhausted: boolean;
  truncated: boolean;
  filters: HistoryFilters;
  records: HistoryRecord[];
}

export interface HistoryStatusSnapshot {
  available: boolean;
  enabled: boolean;
  state: string;
  reason?: string | null;
  last_commit_at_unix_ms?: number | null;
  maintenance?: {
    last_result?: string;
    last_run_at_unix_ms?: number | null;
    last_error?: string | null;
  };
}

export type WorkReconciliationStatus =
  | "pending"
  | "verified"
  | "stale"
  | "unverified";

export interface WorkReconciliationSnapshot {
  status: WorkReconciliationStatus;
  source: string;
  reason: string;
  origin_status: "live" | "historical" | "unknown" | string;
  current_value?: string | null;
  checked_at_unix_ms?: number | null;
}

export interface WorkRecordSnapshot {
  seq: number;
  ts_unix_ms: number;
  kind: string;
  summary: string;
  evidence_source: string;
  authority: string;
  verification: string;
  task_status?: string | null;
  identity: {
    session: string;
    workspace_origin: string;
    tab_origin: string;
    pane_origin: string;
    workspace_id: number;
    workspace_name: string;
    tab_id: number;
    tab_name: string;
    pane_id: number;
    task_id?: string | null;
    task_title?: string | null;
  };
  pull_request?: {
    repository: string;
    number: number;
    title: string;
    url: string;
    state: string;
  } | null;
}

export interface WorkStreamEntrySnapshot {
  record: WorkRecordSnapshot;
  marker: string;
  color_slot: number | null;
  reconciliation: WorkReconciliationSnapshot;
  truth?: TaskTruthSnapshot;
}

export interface WorkStreamLegendSnapshot {
  pane_origin: string;
  workspace_origin: string;
  tab_origin: string;
  marker: string;
  color_slot: number | null;
  workspace_id: number;
  tab_id: number;
  pane_id: number;
  workspace_name: string;
  tab_name: string;
  agent_name?: string | null;
  task_id?: string | null;
  task_title?: string | null;
  origin_state: "live" | "lazy" | "historical";
}

export type CanonicalTaskState =
  | "todo"
  | "in_progress"
  | "blocked"
  | "done"
  | "cancelled"
  | "absent"
  | "unknown";
export type BindingState = "bound" | "unbound";
export type ExecutionState = "running" | "idle" | "unknown";
export type WorkOriginState = "live" | "lazy" | "historical";

export interface TaskTruthMismatchSnapshot {
  source: string;
  detail: string;
  current_value?: string | null;
}

export interface TaskTruthSnapshot {
  pane_origin: string;
  task_id?: string | null;
  canonical: CanonicalTaskState;
  binding: BindingState;
  execution: ExecutionState;
  origin: WorkOriginState;
  verification: WorkReconciliationStatus;
  verification_source: string;
  mismatch?: TaskTruthMismatchSnapshot | null;
  last_checked_unix_ms?: number | null;
  counts_as_live_open: boolean;
}

export interface WorkCountsSnapshot {
  live_open: number;
  historical: number;
  mismatch: number;
}

export interface WorkStreamSnapshot {
  schema: "taarof.work-stream.v1" | string;
  session: string;
  filter: unknown;
  palette: unknown;
  legend: WorkStreamLegendSnapshot[];
  entries: WorkStreamEntrySnapshot[];
  /** Complete chronology for clients with their own live/history control. */
  all_entries?: WorkStreamEntrySnapshot[];
  truth?: TaskTruthSnapshot[];
  counts?: WorkCountsSnapshot;
  overflow_count: number;
  restore: {
    status: string;
    detail?: string | null;
    loaded_records: number;
    rejected_records: number;
  };
}

export interface WorkspaceSnapshot {
  id: number;
  name: string;
  active_tab: number;
  collapsed: boolean;
  repo_root: string | null;
  working_tree_path?: string | null;
  branch_name: string | null;
  tool_version_chip?: string | null;
  is_worktree: boolean;
  linked_issue?: number | null;
  run_status?: string;
  tmux_backed: boolean;
  host_config_name: string | null;
  host_status?: {
    state?: string;
    detail?: string | null;
  } | null;
  tab_count: number;
  tabs: TabSnapshot[];
}

/**
 * Stable agent identity badge, mirrored from the native `crate::agents::agent_badge`
 * table via the HTTP API. The web renders these fields directly (no parallel TS
 * table) so every surface shows the same identity chip. See
 * docs/agent-state-model.md.
 */
export interface AgentBadgeSnapshot {
  /** Normalised agent kind, or the raw source for unknown kinds. */
  name: string;
  /** Short uppercase chip label, e.g. `CLD`, `CDX`. */
  short_label: string;
  /** Single short glyph (letter/emoji) for the compact chip. */
  glyph: string;
  /** Stable color token, e.g. `agent-claude`; `agent-generic` for unknown kinds. */
  color_token: string;
  /** True when the kind was recognised; false for the generic fallback. */
  known: boolean;
}

/**
 * The canonical resolved agent state, decided once by the native app's state
 * machine. Read this instead of re-deriving a state from `activity`, or web
 * surfaces will disagree with the desktop sidebar.
 */
export type AgentLifecycleSnapshot =
  | "idle"
  | "working"
  | "waiting_input"
  | "done"
  | "errored";

export interface TabAgentSnapshot {
  pane_id: number;
  agent_name: string | null;
  session_id?: string | null;
  /** Stable per-kind label, e.g. `codex #2`, disambiguating same-kind agents. */
  instance_label?: string | null;
  /** 1-based index within this kind, in pane order. */
  kind_index?: number | null;
  /** Stable identity badge resolved by the native app. */
  badge?: AgentBadgeSnapshot | null;
  activity?: AgentActivitySnapshot | null;
  /** Canonical resolved lifecycle for this pane. */
  state?: AgentLifecycleSnapshot | null;
  /** Uppercase badge text matching the native sidebar, e.g. `WORKING`. */
  state_label?: string | null;
}

export interface TabSnapshot {
  tab_id: number;
  name: string;
  kind: string;
  focused_pane: number;
  agent_running: boolean;
  agent_name: string | null;
  agent_session_id?: string | null;
  agent_pane_id?: number | null;
  agent_activity?: AgentActivitySnapshot | null;
  /** Per-pane agents in pane order; multiple entries = multi-agent tab. */
  agents?: TabAgentSnapshot[];
  needs_attention: boolean;
  notification_msg: string | null;
  workspace_action?: string | null;
  discovery_cwd?: string | null;
  listening_ports: number[];
  ports_updated_at_unix_ms?: number | null;
  ports_label?: string;
  panes: PaneSnapshot[];
}

export interface PaneSnapshot {
  pane_id: number;
  shell_running: boolean;
  /** Local child-process state; null across SSH or remote-tmux boundaries. */
  has_child_process: boolean | null;
  remote_shell: boolean;
  cwd: string | null;
  cwd_host: string | null;
  tmux_session: string | null;
  tmux_host: string | null;
  location_updated_at_unix_ms?: number | null;
  probe_updated_at_unix_ms?: number | null;
  tmux_probe?: {
    width?: number | null;
    height?: number | null;
    current_command?: string | null;
  } | null;
  attach_supported?: boolean;
  attach_kind?: PaneAttachKind;
  // Server-advertised transport capability (api.rs): "raw_pty" for broker-owned
  // panes (raw pty/ws attach available), "legacy_snapshot" otherwise.
  pty_capability?: "raw_pty" | "legacy_snapshot";
  cols?: number;
  rows?: number;
  transcript?: PaneTranscriptSnapshot | null;
}

export interface RecentFileSnapshot {
  path: string;
  op: "write" | "edit" | string;
  at_unix_ms: number;
}

export interface PaneTranscriptSnapshot {
  last_message?: string | null;
  files_touched?: string[];
  recent_files?: RecentFileSnapshot[];
  message_count?: number;
  updated_at_unix_ms?: number | null;
  recent_tool_calls?: Array<{ tool?: string; target?: string }>;
  session_id?: string | null;
}

export interface PaneAttachSnapshotFrame {
  type: "snapshot";
  pane_id: number;
  cols: number;
  rows: number;
  encoding: "base64";
  payload: string;
}

export interface PaneAttachReplaceFrame {
  type: "replace";
  pane_id: number;
  cols: number;
  rows: number;
  encoding: "utf8";
  payload: string;
}

export interface PaneControlDeltaFrame {
  type: "delta";
  pane_id: number;
  cols: number;
  rows: number;
  encoding: "utf8";
  payload: string;
}

export interface PaneAttachErrorFrame {
  type: "error";
  pane_id: number;
  error: string;
}

export interface PaneControlResizeFrame {
  type: "resize";
  pane_id: number;
  cols: number;
  rows: number;
  supported: boolean;
  message?: string;
}

export interface LiveAgentBinding {
  agent: string;
  session_id?: string | null;
  cwd?: string | null;
  workspace_id: number;
  workspace_name: string;
  tab_id: number;
  tab_name: string;
  pane_id: number;
}

export interface AgentSessionProviderStatus {
  name: string;
  ok: boolean;
  history_available: boolean;
  warning?: string | null;
  error?: string | null;
  session_count: number;
}

/**
 * Per-host status for remote agent-session discovery. Additive: the snapshot
 * omits `remote_hosts` entirely when no remote host is configured or live.
 */
export interface RemoteAgentHostStatus {
  host: string;
  ssh_target: string;
  ok: boolean;
  /** Degraded, not merely cached: failed probe, no probe yet, or a stale round. */
  stale: boolean;
  error?: string | null;
  session_count: number;
  /** Record lines present on the host but not represented in `sessions`. */
  dropped_lines: number;
  /** Files whose per-file byte budget trimmed the sample early. */
  truncated_files: number;
  warning?: string | null;
  observed_at_unix_ms?: number | null;
}

export interface AgentSessionRecord {
  agent: string;
  session_id: string;
  title: string;
  cwd: string;
  /** Present only on sessions discovered on a remote host. */
  host?: string | null;
  repo_root?: string | null;
  started_at_unix_ms?: number | null;
  updated_at_unix_ms: number;
  status: string;
  live_binding?: LiveAgentBinding | null;
  resume_command?: string | null;
  resume_unavailable_reason?: string | null;
}

export interface AgentSessionsSnapshot {
  schema: string;
  generated_at_unix_ms: number;
  providers: AgentSessionProviderStatus[];
  sessions: AgentSessionRecord[];
  remote_hosts?: RemoteAgentHostStatus[];
}

export type PaneAttachTextFrame =
  | PaneAttachSnapshotFrame
  | PaneAttachReplaceFrame
  | PaneControlDeltaFrame
  | PaneAttachErrorFrame
  | PaneControlResizeFrame;

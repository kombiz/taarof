import {
  MONITOR_ORDER_STORAGE_KEY,
  MONITOR_WATCH_STORAGE_KEY,
  applyManualOrder,
  buildFilterOptions,
  buildPaneTargets,
  isAttentionTarget,
  isBusyTarget,
  isDoneTarget,
  isLiveTarget,
  matchesActiveFilter,
  matchesMonitorFilter,
  fallbackAgentBadge,
  paneAgent,
  paneAgentBadge,
  paneAgentLabel,
  panePriority,
  tabAgentInstances,
  readStoredMonitorOrder,
  readStoredStringArray,
  readStoredWatchedKeys,
  resolveWorkFocusTarget,
  liveOpenWorkCount,
  visibleWorkTruth,
  workTruthLabels,
  writeStoredWatchedKeys,
  type PaneTarget,
  type StringArrayStorage,
} from "./monitorBoard.js";
import type { PaneSnapshot, TaarofStateSnapshot, TabSnapshot, TaskTruthSnapshot, WorkspaceSnapshot } from "./types.js";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(message);
  }
}

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

function pane(paneId: number, overrides: Partial<PaneSnapshot> = {}): PaneSnapshot {
  return {
    pane_id: paneId,
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

function tab(
  tabId: number,
  panes: PaneSnapshot[],
  overrides: Partial<TabSnapshot> = {},
): TabSnapshot {
  return {
    tab_id: tabId,
    name: `tab-${tabId}`,
    kind: "agent",
    focused_pane: panes[0]?.pane_id ?? 0,
    agent_running: false,
    agent_name: null,
    agent_session_id: null,
    agent_pane_id: null,
    agent_activity: null,
    needs_attention: false,
    notification_msg: null,
    listening_ports: [],
    panes,
    ...overrides,
  };
}

function workspace(
  workspaceId: number,
  tabs: TabSnapshot[],
  overrides: Partial<WorkspaceSnapshot> = {},
): WorkspaceSnapshot {
  return {
    id: workspaceId,
    name: `workspace-${workspaceId}`,
    active_tab: tabs[0]?.tab_id ?? 0,
    collapsed: false,
    repo_root: "/repo",
    branch_name: "main",
    is_worktree: false,
    tmux_backed: true,
    host_config_name: null,
    tab_count: tabs.length,
    tabs,
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
    workspaces: [],
    ...overrides,
  };
}

function truth(overrides: Partial<TaskTruthSnapshot> = {}): TaskTruthSnapshot {
  return {
    pane_origin: "pane-1",
    task_id: "EXAMPLE-131",
    canonical: "todo",
    binding: "unbound",
    execution: "idle",
    origin: "live",
    verification: "verified",
    verification_source: "plan",
    mismatch: null,
    last_checked_unix_ms: 9_000,
    counts_as_live_open: true,
    ...overrides,
  };
}

test("work truth labels keep open, binding, execution, origin, verification, and checked time separate", () => {
  const label = (item: TaskTruthSnapshot) => workTruthLabels(item, 10_000).join(" · ");
  assertEqual(
    label(truth()),
    "Todo · Unbound · Idle · Live · Verified (plan) · Checked just now",
  );
  assert(label(truth({ binding: "bound" })).includes("Bound · Idle"), "bound idle is explicit");
  assert(label(truth({ binding: "bound", execution: "running" })).includes("Bound · Running"), "bound running is explicit");
  assert(label(truth({ task_id: null, execution: "running" })).includes("Unbound · Running"), "unbound agent stays running");
  assert(label(truth({ canonical: "done", origin: "historical", execution: "unknown" })).includes("Done · Unbound · Unknown · Historical"), "historical done is explicit");
  assert(label(truth({ origin: "historical", execution: "unknown", counts_as_live_open: false })).includes("Todo · Unbound · Unknown · Historical"), "historical open remains historical");
  assert(label(truth({ verification: "stale" })).includes("Stale (plan)"), "stale source is explicit");
});

test("work truth checked age uses the same minute hour and day buckets as native", () => {
  assert(workTruthLabels(truth({ last_checked_unix_ms: 0 }), 60_000).includes("Checked 1m ago"), "minute bucket");
  assert(workTruthLabels(truth({ last_checked_unix_ms: 0 }), 3_600_000).includes("Checked 1h ago"), "hour bucket");
  assert(workTruthLabels(truth({ last_checked_unix_ms: 0 }), 86_400_000).includes("Checked 1d ago"), "day bucket");
});

test("work truth history gating excludes done and historical from live open", () => {
  const items = [
    truth(),
    truth({ pane_origin: "pane-2", canonical: "done", counts_as_live_open: false }),
    truth({ pane_origin: "pane-3", origin: "historical", execution: "unknown", counts_as_live_open: false }),
  ];
  assertEqual(liveOpenWorkCount(items), 1);
  assertDeepEqual(visibleWorkTruth(items, false).map((item) => item.pane_origin), ["pane-1"]);
  assertDeepEqual(visibleWorkTruth(items, true).map((item) => item.pane_origin), ["pane-2", "pane-3"]);
});

test("work truth labels source PR task mismatch", () => {
  const label = workTruthLabels(truth({
    verification: "stale",
    verification_source: "github",
    mismatch: { source: "github", detail: "open PR vs todo", current_value: "open" },
  }), 10_000).join(" · ");
  assert(label.includes("GitHub mismatch: open PR vs todo"), "mismatch names its authority");
});

function targetByKey(targets: PaneTarget[], key: string): PaneTarget {
  const target = targets.find((candidate) => candidate.key === key);
  assert(target, `missing target ${key}`);
  return target;
}

class MemoryStringArrayStorage implements StringArrayStorage {
  private readonly values = new Map<string, string>();

  constructor(entries: Array<[string, string]> = []) {
    entries.forEach(([key, value]) => this.values.set(key, value));
  }

  getItem(storageKey: string): string | null {
    return this.values.get(storageKey) ?? null;
  }

  setItem(storageKey: string, value: string): void {
    this.values.set(storageKey, value);
  }
}

test("panePriority scores attention/running/active panes above idle panes", () => {
  const idlePane = pane(1, { attach_supported: false, shell_running: false });
  const signalPane = pane(2, { has_child_process: true });
  const idleTab = tab(10, [idlePane]);
  const signalTab = tab(20, [signalPane], {
    agent_activity: { state: "waiting-input" },
    agent_pane_id: 2,
    agent_running: true,
    needs_attention: true,
  });
  const activeWorkspace = workspace(1, [idleTab, signalTab], { active_tab: 20 });
  const state = snapshot({
    active_tab: 20,
    active_workspace: 1,
    workspaces: [activeWorkspace],
  });

  assert(
    panePriority(state, activeWorkspace, signalTab, signalPane) >
      panePriority(state, activeWorkspace, idleTab, idlePane),
    "signal pane should score above an idle pane",
  );
  assertEqual(buildPaneTargets(state)[0].key, "1:20:2");
});

test("filter predicates classify attention, live, busy, done, and watch panes", () => {
  const attentionTab = tab(10, [pane(1, { has_child_process: false })], {
    needs_attention: true,
  });
  const liveTab = tab(20, [pane(2, { has_child_process: false })], {
    agent_activity: { state: "running" },
  });
  const busyTab = tab(30, [pane(3, { has_child_process: true })]);
  const doneTab = tab(40, [pane(4, { has_child_process: false })]);
  const targets = buildPaneTargets(
    snapshot({
      workspaces: [workspace(1, [attentionTab, liveTab, busyTab, doneTab])],
    }),
  );
  const attention = targetByKey(targets, "1:10:1");
  const live = targetByKey(targets, "1:20:2");
  const busy = targetByKey(targets, "1:30:3");
  const done = targetByKey(targets, "1:40:4");
  const watchedKeys = new Set([done.key]);
  const options = buildFilterOptions(targets, watchedKeys);

  assertEqual(isAttentionTarget(attention), true);
  assertEqual(isLiveTarget(live), true);
  assertEqual(isBusyTarget(busy), true);
  assertEqual(isDoneTarget(done), true);
  assertEqual(matchesMonitorFilter(done, "signal"), false);
  assertEqual(matchesMonitorFilter(attention, "signal"), true);
  assertEqual(matchesMonitorFilter(live, "signal"), true);
  assertEqual(matchesMonitorFilter(busy, "signal"), true);
  assertEqual(matchesActiveFilter(done, "watch", watchedKeys), true);
  assertEqual(options.find((option) => option.id === "watch")?.count, 1);
  assertEqual(options.find((option) => option.id === "signal")?.count, 3);
});

test("per-pane canonical attention keeps a waiting sibling distinct from a running sibling", () => {
  const siblingTab = tab(60, [pane(1), pane(2)], {
    agent_running: true,
    agent_activity: { state: "running", text: "working" },
    agents: [
      {
        pane_id: 1,
        agent_name: "codex",
        state: "working",
        attention: null,
      },
      {
        pane_id: 2,
        agent_name: "claude",
        state: "waiting_input",
        attention: {
          reason: "waiting_input",
          provider: "claude",
          provenance: "termprop",
          authority: "provider_explicit",
          freshness: "fresh",
          last_verified_unix_ms: 1_800_000_000_000,
        },
      },
    ],
  });
  siblingTab.panes[1].attention = siblingTab.agents?.[1].attention;
  const targets = buildPaneTargets(
    snapshot({ workspaces: [workspace(1, [siblingTab])] }),
  );
  const running = targetByKey(targets, "1:60:1");
  const waiting = targetByKey(targets, "1:60:2");

  assertEqual(isLiveTarget(running), true);
  assertEqual(isAttentionTarget(running), false);
  assertEqual(isLiveTarget(waiting), false);
  assertEqual(isAttentionTarget(waiting), true);
  assertEqual(matchesMonitorFilter(waiting, "attention"), true);
  assert(
    waiting.priority > running.priority,
    "the exact waiting pane should rank above its running sibling",
  );
});

test("remote tmux panes remain neither busy nor done when SSH probing is missed", () => {
  const remoteTab = tab(50, [pane(5, {
    has_child_process: null,
    remote_shell: false,
    cwd_host: "devbox",
    tmux_host: "devbox",
  })]);
  const [remote] = buildPaneTargets(
    snapshot({ workspaces: [workspace(1, [remoteTab])] }),
  );

  assertEqual(isBusyTarget(remote), false);
  assertEqual(isDoneTarget(remote), false);
  assertEqual(matchesMonitorFilter(remote, "busy"), false);
  assertEqual(matchesMonitorFilter(remote, "done"), false);
});

test("applyManualOrder honors stored order then falls back to priority", () => {
  const targets = buildPaneTargets(
    snapshot({
      workspaces: [
        workspace(1, [
          tab(1, [
            pane(1),
            pane(2, { has_child_process: true }),
            pane(3),
          ]),
        ]),
      ],
    }),
  );
  const ordered = applyManualOrder(targets, ["1:1:3"]);

  assertEqual(ordered[0].key, "1:1:3");
  assertDeepEqual(
    ordered.slice(1).map((target) => target.key),
    ["1:1:2", "1:1:1"],
  );
});

test("localStorage string-array adapters ignore bad values and write JSON arrays", () => {
  const storage = new MemoryStringArrayStorage([
    [MONITOR_ORDER_STORAGE_KEY, JSON.stringify(["one", 2, "two", null])],
    ["invalid", "{"],
  ]);

  assertDeepEqual(readStoredMonitorOrder(storage), ["one", "two"]);
  assertDeepEqual(readStoredStringArray(storage, "missing"), []);
  assertDeepEqual(readStoredStringArray(storage, "invalid"), []);

  writeStoredWatchedKeys(storage, ["1:1:1", "1:1:2"]);

  assertEqual(storage.getItem(MONITOR_WATCH_STORAGE_KEY), "[\"1:1:1\",\"1:1:2\"]");
  assertDeepEqual(readStoredWatchedKeys(storage), ["1:1:1", "1:1:2"]);
});


test("paneAgentLabel disambiguates same-kind agents and preserves per-pane detail", () => {
  // A tab with two codex agents (panes 4, 5) and one claude (pane 6),
  // matching the native app's agents[] payload.
  const multiAgentTab = tab(1, [pane(4), pane(5), pane(6)], {
    agents: [
      {
        pane_id: 4,
        agent_name: "codex",
        session_id: "c1",
        instance_label: "codex #1",
        kind_index: 1,
        activity: { state: "running", text: "running" },
      },
      {
        pane_id: 5,
        agent_name: "codex",
        session_id: "c2",
        instance_label: "codex #2",
        kind_index: 2,
        activity: { state: "waiting-input", text: "waiting for input" },
      },
      {
        pane_id: 6,
        agent_name: "claude",
        session_id: "cl",
        instance_label: "claude",
        kind_index: 1,
        badge: {
          name: "claude",
          short_label: "CLD",
          glyph: "\u2726",
          color_token: "agent-claude",
          known: true,
        },
        activity: null,
      },
    ],
  });

  // Same-kind agents are distinguishable by their disambiguated labels.
  assertEqual(paneAgentLabel(multiAgentTab, 4), "codex #1");
  assertEqual(paneAgentLabel(multiAgentTab, 5), "codex #2");
  assertEqual(paneAgentLabel(multiAgentTab, 6), "claude");

  // Pane attribution is preserved on lookup.
  assertEqual(paneAgent(multiAgentTab, 5)?.session_id, "c2");

  // The rollup keeps every pane's detail (label + pane + activity state).
  // The rollup carries each pane's badge too: codex entries omit `badge` in the
  // payload so they fall back to the resolved codex badge, while the claude
  // entry's payload badge is consumed straight through.
  assertDeepEqual(tabAgentInstances(multiAgentTab), [
    {
      paneId: 4,
      label: "codex #1",
      state: "running",
      badge: fallbackAgentBadge("codex"),
    },
    {
      paneId: 5,
      label: "codex #2",
      state: "waiting for input",
      badge: fallbackAgentBadge("codex"),
    },
    {
      paneId: 6,
      label: "claude",
      state: null,
      badge: {
        name: "claude",
        short_label: "CLD",
        glyph: "\u2726",
        color_token: "agent-claude",
        known: true,
      },
    },
  ]);

  // paneAgentBadge consumes the payload badge directly for the claude pane.
  assertEqual(paneAgentBadge(multiAgentTab, 6)?.color_token, "agent-claude");
  assertEqual(paneAgentBadge(multiAgentTab, 6)?.known, true);
  // Panes without a payload badge fall back to a generic-but-sourced badge.
  assertEqual(paneAgentBadge(multiAgentTab, 4)?.short_label, "CODE");
  assertEqual(paneAgentBadge(multiAgentTab, 4)?.color_token, "agent-generic");
});

test("paneAgentLabel falls back to agent_name and returns null without agents", () => {
  const bareTab = tab(2, [pane(1)], {
    agents: [{ pane_id: 1, agent_name: "aider", session_id: null }],
  });
  assertEqual(paneAgentLabel(bareTab, 1), "aider");

  const noAgentTab = tab(3, [pane(1)]);
  assertEqual(paneAgentLabel(noAgentTab, 1), null);
  assertDeepEqual(tabAgentInstances(noAgentTab), []);
});

test("fallbackAgentBadge mirrors the native generic fallback", () => {
  // Empty / whitespace source -> neutral AGENT badge.
  assertDeepEqual(fallbackAgentBadge(""), {
    name: "agent",
    short_label: "AGENT",
    glyph: "\u2022",
    color_token: "agent-generic",
    known: false,
  });
  assertDeepEqual(fallbackAgentBadge("   "), {
    name: "agent",
    short_label: "AGENT",
    glyph: "\u2022",
    color_token: "agent-generic",
    known: false,
  });

  // Named source -> uppercased, whitespace-stripped, truncated to 4 chars.
  const mystery = fallbackAgentBadge("MysteryBot");
  assertEqual(mystery.short_label, "MYST");
  assertEqual(mystery.name, "mysterybot");
  assertEqual(mystery.color_token, "agent-generic");
  assertEqual(mystery.known, false);
});

test("paneAgentBadge returns null for a pane with no agent", () => {
  const noAgentTab = tab(4, [pane(1)]);
  assertEqual(paneAgentBadge(noAgentTab, 1), null);
});

test("resolveWorkFocusTarget ignores recycled persisted ids and uses live stable origins", () => {
  const state = snapshot({
    session_name: "named",
    work: {
      schema: "taarof.work-stream.v1",
      session: "named",
      filter: "all",
      palette: {},
      overflow_count: 0,
      restore: { status: "restored", loaded_records: 1, rejected_records: 0 },
      entries: [],
      legend: [
        {
          pane_origin: "pane-stable",
          workspace_origin: "workspace-stable",
          tab_origin: "tab-stable",
          marker: "P1",
          color_slot: 0,
          workspace_id: 9,
          tab_id: 90,
          pane_id: 900,
          workspace_name: "current",
          tab_name: "current",
          origin_state: "live",
        },
      ],
    },
  });
  const identity = {
    session: "named",
    workspace_origin: "workspace-stable",
    tab_origin: "tab-stable",
    pane_origin: "pane-stable",
    // These persisted ids have since been recycled by another pane.
    workspace_id: 1,
    workspace_name: "old",
    tab_id: 2,
    tab_name: "old",
    pane_id: 3,
  };
  assertDeepEqual(resolveWorkFocusTarget(state, identity), {
    workspaceId: 9,
    tabId: 90,
    paneId: 900,
  });
  assertEqual(
    resolveWorkFocusTarget(state, { ...identity, pane_origin: "recycled-other-pane" }),
    null,
  );
});

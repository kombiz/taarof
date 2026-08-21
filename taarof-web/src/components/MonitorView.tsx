import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  normalizePanePreviewText,
  type PaneAttachPhase,
} from "../paneAttachFrames";
import {
  buildPaneIdentity,
  formatPaneJournalText,
  pruneExpiredPaneJournals,
} from "../monitorPaneJournal";
import type { TaarofStateSnapshot } from "../types";
import {
  applyManualOrder,
  buildFilterOptions,
  buildPaneTargets,
  isAttentionTarget,
  isBusyTarget,
  isLiveTarget,
  liveOpenWorkCount,
  matchesActiveFilter,
  matchesMonitorFilter,
  paneAgentBadge,
  paneAgentLabel,
  readStoredMonitorOrder,
  readStoredWatchedKeys,
  resolveWorkFocusTarget,
  tabAgentInstances,
  visibleWorkTruth,
  workTruthLabels,
  writeStoredMonitorOrder,
  writeStoredWatchedKeys,
  type MonitorFilter,
  type PaneTarget,
} from "../monitorBoard";
import {
  activityLabel,
  formatJournalTime,
  paneStatusLabel,
  phaseLabel,
  previewFromTerminalText,
  renderFrameText,
} from "./MonitorView.helpers";
import { usePaneAttach } from "../usePaneAttach";
import { usePaneJournal, type PaneJournalFrame } from "../usePaneJournal";

const MAX_LIVE_PREVIEWS = 12;
const MAX_WATCH_SIGNAL_PREVIEWS = 6;

type MonitorPhase = PaneAttachPhase;

interface MonitorViewProps {
  isLoading: boolean;
  token: string;
  snapshot: TaarofStateSnapshot | null;
  onSelectLiveTarget: (workspaceId: number, tabId: number, paneId: number | null) => void;
}

interface MonitorPaneCardProps {
  actionLabel?: string;
  canMoveDown: boolean;
  canMoveUp: boolean;
  isLive: boolean;
  isWatched: boolean;
  onMoveDown: () => void;
  onMoveUp: () => void;
  onToggleWatch: () => void;
  showMoveControls?: boolean;
  target: PaneTarget;
  token: string;
  onOpen: () => void;
}

function MonitorPaneCard({
  actionLabel = "Watch",
  canMoveDown,
  canMoveUp,
  isLive,
  isWatched,
  onMoveDown,
  onMoveUp,
  onToggleWatch,
  showMoveControls = true,
  target,
  token,
  onOpen,
}: MonitorPaneCardProps) {
  const [phase, setPhase] = useState<MonitorPhase>(isLive ? "connecting" : "queued");
  const [terminalText, setTerminalText] = useState("");
  const terminalTextRef = useRef("");
  const frameSequenceRef = useRef(0);
  const [latestJournalFrame, setLatestJournalFrame] = useState<PaneJournalFrame | null>(null);
  const [sizeLabel, setSizeLabel] = useState(
    target.pane.cols && target.pane.rows ? `${target.pane.cols}x${target.pane.rows}` : "--",
  );
  const identity = useMemo(
    () =>
      buildPaneIdentity({
        sessionName: target.sessionName,
        workspaceId: target.workspace.id,
        workspaceName: target.workspace.name,
        tabId: target.tab.tab_id,
        tabName: target.tab.name,
        paneId: target.pane.pane_id,
      }),
    [
      target.pane.pane_id,
      target.sessionName,
      target.tab.name,
      target.tab.tab_id,
      target.workspace.id,
      target.workspace.name,
    ],
  );
  const journal = usePaneJournal(window.localStorage, identity, latestJournalFrame);

  const queueJournalFrame = useCallback(
    (frame: Omit<PaneJournalFrame, "seenAtUnixMs" | "sequence">) => {
      frameSequenceRef.current += 1;
      setLatestJournalFrame({
        ...frame,
        seenAtUnixMs: Date.now(),
        sequence: frameSequenceRef.current,
      });
    },
    [],
  );

  useEffect(() => {
    if (!isLive || !target.pane.attach_supported) {
      setPhase("queued");
      terminalTextRef.current = "";
      setTerminalText("");
      setSizeLabel(
        target.pane.cols && target.pane.rows
          ? `${target.pane.cols}x${target.pane.rows}`
          : "--",
      );
    }
  }, [isLive, target.pane.attach_supported, target.pane.cols, target.pane.rows]);

  usePaneAttach(target.tab.tab_id, target.pane.pane_id, token, {
    enabled: isLive && Boolean(target.pane.attach_supported),
    onConnecting: () => {
      setPhase("connecting");
      terminalTextRef.current = "";
      setTerminalText("");
    },
    onSnapshot: (frame) => {
      const nextText = renderFrameText(frame);
      terminalTextRef.current = nextText;
      setTerminalText(nextText);
      queueJournalFrame({ source: "snapshot", text: nextText, cols: frame.cols, rows: frame.rows });
      setSizeLabel(`${frame.cols}x${frame.rows}`);
      setPhase("live");
    },
    onReplace: (frame) => {
      const nextText = renderFrameText(frame);
      terminalTextRef.current = nextText;
      setTerminalText(nextText);
      queueJournalFrame({ source: "replace", text: nextText, cols: frame.cols, rows: frame.rows });
      setSizeLabel(`${frame.cols}x${frame.rows}`);
      setPhase("live");
    },
    onErrorFrame: (frame) => {
      terminalTextRef.current = frame.error;
      setTerminalText(frame.error);
      queueJournalFrame({ source: "error", text: frame.error });
      setPhase("error");
    },
    onRawText: (text) => {
      const nextText = normalizePanePreviewText(`${terminalTextRef.current}\n${text}`)
        .trimEnd()
        .slice(-12000);
      terminalTextRef.current = nextText;
      setTerminalText(nextText);
      queueJournalFrame({ source: "raw", text: nextText });
      setPhase("live");
    },
    onClose: () => {
      setPhase((current) => (current === "error" ? current : "closed"));
    },
    onSocketError: () => {
      setPhase("error");
      terminalTextRef.current = "preview socket failed";
      setTerminalText("preview socket failed");
    },
  });

  const status = paneStatusLabel(target);
  const cwd = target.pane.cwd ?? target.tab.discovery_cwd ?? "unknown cwd";
  const agentLabel = paneAgentLabel(target.tab, target.pane.pane_id);
  const paneBadge = paneAgentBadge(target.tab, target.pane.pane_id);
  const tabAgents = tabAgentInstances(target.tab);
  const isMultiAgentTab = tabAgents.length > 1;
  const compactPreview = previewFromTerminalText(terminalText);
  const journalText = formatPaneJournalText(journal);
  const latestObservation = journal.entries[journal.entries.length - 1] ?? null;
  const previewText =
    compactPreview ||
    (isLive
      ? "waiting for terminal frame"
      : target.pane.attach_supported
        ? "preview queued"
        : "preview unavailable");
  const textCapture = journalText || terminalText || "No text captured";

  const cardClassName = [
    "monitor-card",
    isLive ? "monitor-card--live" : "",
    showMoveControls ? "" : "monitor-card--candidate",
  ]
    .filter(Boolean)
    .join(" ");

  return (
    <article className={cardClassName}>
      <div className="monitor-card__header">
        <div>
          <div className="monitor-card__workspace">{target.workspace.name}</div>
          <h3>{target.tab.name}</h3>
        </div>
        <div className="monitor-card__badges">
          {isWatched ? <span className="monitor-card__watch-badge">watch</span> : null}
          <span className={`monitor-card__status monitor-card__status--${status}`}>
            {status}
          </span>
          <span className={`monitor-card__phase monitor-card__phase--${phase}`}>
            {phaseLabel(phase)}
          </span>
        </div>
      </div>

      <button className="monitor-card__preview-button" onClick={onOpen} type="button">
        <pre className="monitor-card__preview">{previewText}</pre>
      </button>

      <details className="monitor-card__text-capture">
        <summary>
          <span>{identity.displayName}</span>
          <strong>{journal.totalTextBytes.toLocaleString()} bytes</strong>
        </summary>
        <div className="monitor-card__journal-meta">
          <span>{journal.entries.length} observations</span>
          <span>latest {formatJournalTime(latestObservation?.seenAtUnixMs ?? null)}</span>
          {latestObservation && latestObservation.tabNameAtCapture !== target.tab.name ? (
            <span>captured as {latestObservation.tabNameAtCapture}</span>
          ) : null}
        </div>
        <pre>{textCapture}</pre>
      </details>

      <div className="monitor-card__meta">
        <span>pane {target.pane.pane_id}</span>
        {paneBadge ? (
          <span
            className={`monitor-card__agent-badge monitor-card__agent-badge--${paneBadge.color_token}`}
            title={paneBadge.name}
          >
            <span className="monitor-card__agent-badge-glyph" aria-hidden="true">
              {paneBadge.glyph}
            </span>
            {paneBadge.short_label}
          </span>
        ) : null}
        {agentLabel ? <span className="monitor-card__agent">{agentLabel}</span> : null}
        <span>{sizeLabel}</span>
        <span>{target.pane.attach_kind ?? "unknown"}</span>
      </div>
      <div className="monitor-card__detail">
        <span>{cwd}</span>
        <span>{activityLabel(target)}</span>
      </div>
      {isMultiAgentTab ? (
        <ul className="monitor-card__agents" aria-label="Agents in this tab">
          {tabAgents.map((agent) => (
            <li
              key={agent.paneId}
              className={
                agent.paneId === target.pane.pane_id
                  ? "monitor-card__agent-row monitor-card__agent-row--current"
                  : "monitor-card__agent-row"
              }
            >
              <span
                className={`monitor-card__agent-badge monitor-card__agent-badge--${agent.badge.color_token}`}
                title={agent.badge.name}
              >
                <span className="monitor-card__agent-badge-glyph" aria-hidden="true">
                  {agent.badge.glyph}
                </span>
                {agent.badge.short_label}
              </span>
              <span className="monitor-card__agent-label">{agent.label}</span>
              <span className="monitor-card__agent-pane">pane {agent.paneId}</span>
              {agent.state ? (
                <span className="monitor-card__agent-state">{agent.state}</span>
              ) : null}
            </li>
          ))}
        </ul>
      ) : null}
      <div className="monitor-card__actions" aria-label={`Monitor actions for ${target.tab.name}`}>
        <button
          className={
            isWatched
              ? "monitor-card__watch-action monitor-card__watch-action--active"
              : "monitor-card__watch-action"
          }
          onClick={onToggleWatch}
          type="button"
        >
          {isWatched ? "Watching" : actionLabel}
        </button>
        {showMoveControls ? (
          <>
            <button disabled={!canMoveUp} onClick={onMoveUp} type="button">
              Up
            </button>
            <button disabled={!canMoveDown} onClick={onMoveDown} type="button">
              Down
            </button>
          </>
        ) : null}
      </div>
    </article>
  );
}

export function MonitorView({
  isLoading,
  token,
  snapshot,
  onSelectLiveTarget,
}: MonitorViewProps) {
  const [filter, setFilter] = useState<MonitorFilter>("all");
  const [showWorkHistory, setShowWorkHistory] = useState(false);
  const [orderedKeys, setOrderedKeys] = useState<string[]>(() =>
    readStoredMonitorOrder(window.localStorage),
  );
  const [watchedKeys, setWatchedKeys] = useState<string[]>(() =>
    readStoredWatchedKeys(window.localStorage),
  );
  const work = snapshot?.work;
  const workTruth = work?.truth ?? [];
  const workEntries = work?.all_entries ?? work?.entries ?? [];
  const visibleTruth = useMemo(
    () => visibleWorkTruth(workTruth, showWorkHistory),
    [showWorkHistory, workTruth],
  );
  const visibleTruthOrigins = useMemo(
    () => new Set(visibleTruth.map((item) => item.pane_origin)),
    [visibleTruth],
  );
  const hasWork = Boolean(work && (workEntries.length > 0 || work.legend.length > 0));

  useEffect(() => {
    pruneExpiredPaneJournals(window.localStorage);
  }, []);

  const baseTargets = useMemo(() => buildPaneTargets(snapshot), [snapshot]);
  const targets = useMemo(
    () => applyManualOrder(baseTargets, orderedKeys),
    [baseTargets, orderedKeys],
  );
  const watchedKeySet = useMemo(() => new Set(watchedKeys), [watchedKeys]);
  const visibleTargets = useMemo(
    () =>
      targets.filter((target) => matchesActiveFilter(target, filter, watchedKeySet)),
    [filter, targets, watchedKeySet],
  );
  const filterOptions = useMemo(
    () => buildFilterOptions(targets, watchedKeySet),
    [targets, watchedKeySet],
  );
  const activeFilterLabel =
    filterOptions.find((option) => option.id === filter)?.label ?? "Signal";
  const livePreviewKeys = useMemo(() => {
    const attachable = visibleTargets.filter((target) => target.pane.attach_supported);
    const watched = attachable.filter((target) => watchedKeySet.has(target.key));
    const unwatched = attachable.filter((target) => !watchedKeySet.has(target.key));
    return new Set(
      [...watched, ...unwatched]
        .slice(0, MAX_LIVE_PREVIEWS)
        .map((target) => target.key),
    );
  }, [visibleTargets, watchedKeySet]);
  const busyCount = targets.filter(isBusyTarget).length;
  const liveCount = targets.filter(isLiveTarget).length;
  const attentionCount = targets.filter(isAttentionTarget).length;
  const watchCount = targets.filter((target) => watchedKeySet.has(target.key)).length;
  const watchSignalTargets = useMemo(
    () =>
      targets.filter(
        (target) =>
          !watchedKeySet.has(target.key) && matchesMonitorFilter(target, "signal"),
      ),
    [targets, watchedKeySet],
  );
  const visibleWatchSignalTargets = watchSignalTargets.slice(0, MAX_WATCH_SIGNAL_PREVIEWS);
  const hiddenWatchSignalCount = Math.max(
    0,
    watchSignalTargets.length - visibleWatchSignalTargets.length,
  );
  const showWatchSignalPreviews = filter !== "signal" && visibleWatchSignalTargets.length > 0;
  const watchSignalPreviewKeys = useMemo(
    () =>
      new Set(
        visibleWatchSignalTargets
          .filter((target) => target.pane.attach_supported)
          .slice(0, MAX_WATCH_SIGNAL_PREVIEWS)
          .map((target) => target.key),
      ),
    [visibleWatchSignalTargets],
  );

  function commitOrder(nextKeys: string[]) {
    setOrderedKeys(nextKeys);
    writeStoredMonitorOrder(window.localStorage, nextKeys);
  }

  function commitWatchedKeys(nextKeys: string[]) {
    setWatchedKeys(nextKeys);
    writeStoredWatchedKeys(window.localStorage, nextKeys);
  }

  function toggleWatchedTarget(targetKey: string) {
    if (watchedKeySet.has(targetKey)) {
      commitWatchedKeys(watchedKeys.filter((key) => key !== targetKey));
      return;
    }

    commitWatchedKeys([...watchedKeys, targetKey]);
  }

  function moveTarget(targetKey: string, direction: -1 | 1) {
    const visibleKeys = visibleTargets.map((target) => target.key);
    const visibleIndex = visibleKeys.indexOf(targetKey);
    const swapKey = visibleKeys[visibleIndex + direction];
    if (!swapKey) {
      return;
    }

    const currentKeys = targets.map((target) => target.key);
    const fromIndex = currentKeys.indexOf(targetKey);
    const toIndex = currentKeys.indexOf(swapKey);

    if (fromIndex < 0 || toIndex < 0 || toIndex >= currentKeys.length) {
      return;
    }

    const nextKeys = [...currentKeys];
    nextKeys[fromIndex] = swapKey;
    nextKeys[toIndex] = targetKey;
    commitOrder(nextKeys);
  }

  if (isLoading && targets.length === 0 && !hasWork) {
    return (
      <section className="monitor-view monitor-view--empty">
        <div className="monitor-view__label">Global monitor</div>
        <h2>Loading pane map</h2>
      </section>
    );
  }

  if (targets.length === 0 && !hasWork) {
    return (
      <section className="monitor-view monitor-view--empty">
        <div className="monitor-view__label">Global monitor</div>
        <h2>No panes available</h2>
      </section>
    );
  }

  return (
    <section className="monitor-view">
      <div className="monitor-view__header">
        <div>
          <div className="monitor-view__label">Terminal board</div>
          <h2>{activeFilterLabel} board</h2>
        </div>
        <div className="monitor-view__summary">
          <span>{visibleTargets.length} shown</span>
          <span>{livePreviewKeys.size} live previews</span>
          <span>{watchCount} watched</span>
          <span>{attentionCount} attention</span>
          <span>{liveCount} live</span>
          <span>{busyCount} busy</span>
        </div>
      </div>

      <div className="monitor-view__controls" role="tablist" aria-label="Monitor filter">
        {filterOptions.map((option) => (
          <button
            aria-selected={filter === option.id}
            className={
              filter === option.id
                ? "monitor-view__filter monitor-view__filter--selected"
                : "monitor-view__filter"
            }
            key={option.id}
            onClick={() => setFilter(option.id)}
            role="tab"
            type="button"
          >
            <span>{option.label}</span>
            <strong>{option.count}</strong>
          </button>
        ))}
      </div>

      {work ? (
        <section className="monitor-work" aria-label="Session work history">
          <div className="monitor-view__section-header">
            <div>
              <span>Session chronology</span>
              <h3>Work</h3>
            </div>
            <div className="monitor-work__counts">
              <span>
                {work.counts?.live_open ?? liveOpenWorkCount(workTruth)} live/open ·{" "}
                {work.counts?.historical ?? 0} historical
                {(work.counts?.mismatch ?? 0) > 0 ? ` · ${work.counts?.mismatch} mismatch` : ""}
              </span>
              <button
                aria-pressed={showWorkHistory}
                onClick={() => setShowWorkHistory((current) => !current)}
                type="button"
              >
                {showWorkHistory ? "Show live/open" : "Show history"}
              </button>
            </div>
          </div>
          {work.restore.status === "degraded" || work.restore.status === "persistence_unavailable" ? (
            <div className="monitor-work__warning" role="status">
              {work.restore.detail ?? "Restored work history is incomplete"}
            </div>
          ) : null}
          <div className="monitor-work__legend" aria-label="Work pane legend">
            {work.legend
              .map((item) => {
              const truth = workTruth.find((candidate) => candidate.pane_origin === item.pane_origin);
              return (
              <span data-color-slot={item.color_slot ?? "overflow"} key={item.pane_origin}>
                <strong>{item.marker}</strong> {item.tab_name} pane {item.pane_id} ·{" "}
                {item.agent_name ?? "No active agent"} · {truth?.task_id ?? item.task_id ?? "No task"}
                {truth ? (
                  <span className="monitor-work__truth-labels">
                    {workTruthLabels(truth).map((label, index, labels) => {
                      const isMismatch = truth.mismatch != null && index === labels.length - 1;
                      return (
                      <span className={isMismatch ? "monitor-work__truth-chip monitor-work__truth-chip--mismatch" : "monitor-work__truth-chip"} key={`${index}:${label}`}>
                        {label}
                      </span>
                      );
                    })}
                  </span>
                ) : ` · ${item.origin_state}`}
              </span>
              );
            })}
          </div>
          <div className="monitor-work__feed">
            {workEntries
              .filter((entry) => entry.truth
                ? visibleWorkTruth([entry.truth], showWorkHistory).length > 0
                : workTruth.length === 0 || visibleTruthOrigins.has(entry.record.identity.pane_origin))
              .map((entry) => {
              const identity = entry.record.identity;
              const focusTarget = resolveWorkFocusTarget(snapshot, identity);
              const truth = entry.truth ?? workTruth.find((candidate) => candidate.pane_origin === identity.pane_origin);
              return (
                <button
                  className={`monitor-work__entry monitor-work__entry--${entry.reconciliation.status}`}
                  data-color-slot={entry.color_slot ?? "overflow"}
                  disabled={!focusTarget}
                  key={entry.record.seq}
                  onClick={() => {
                    if (focusTarget) {
                      onSelectLiveTarget(
                        focusTarget.workspaceId,
                        focusTarget.tabId,
                        focusTarget.paneId,
                      );
                    }
                  }}
                  title={`${entry.reconciliation.source}: ${entry.reconciliation.reason}`}
                  type="button"
                >
                  <span>
                    <strong>{entry.marker}</strong> {entry.record.summary}
                  </span>
                  <small>
                    {truth?.task_id ?? identity.task_id ?? "No task"}
                    {entry.record.pull_request ? ` · PR #${entry.record.pull_request.number}` : ""}
                    {truth ? ` · ${workTruthLabels(truth).join(" · ")}` : ` · ${entry.reconciliation.status}`}
                    {` · #${entry.record.seq}`}
                  </small>
                </button>
              );
            })}
          </div>
        </section>
      ) : null}

      {showWatchSignalPreviews ? (
        <section className="monitor-view__signal-previews" aria-label="Unwatched signal previews">
          <div className="monitor-view__section-header">
            <div>
              <span>Signal to watch</span>
              <h3>New terminal screens</h3>
            </div>
            <button onClick={() => setFilter("signal")} type="button">
              {hiddenWatchSignalCount > 0
                ? `Review all ${watchSignalTargets.length}`
                : "Review signal board"}
            </button>
          </div>
          <div className="monitor-view__signal-grid">
            {visibleWatchSignalTargets.map((target) => (
              <MonitorPaneCard
                actionLabel="Add to watch"
                canMoveDown={false}
                canMoveUp={false}
                isLive={watchSignalPreviewKeys.has(target.key)}
                isWatched={false}
                key={`signal-${target.key}`}
                onMoveDown={() => undefined}
                onMoveUp={() => undefined}
                onToggleWatch={() => toggleWatchedTarget(target.key)}
                onOpen={() =>
                  onSelectLiveTarget(
                    target.workspace.id,
                    target.tab.tab_id,
                    target.pane.pane_id,
                  )
                }
                showMoveControls={false}
                target={target}
                token={token}
              />
            ))}
          </div>
        </section>
      ) : null}

      <div className="monitor-view__grid">
        {visibleTargets.length === 0 ? (
          <div className="monitor-view__empty-filter">
            {filter === "watch"
              ? "No terminals are on the watch board yet."
              : "No terminals match this filter right now."}
          </div>
        ) : null}
        {visibleTargets.map((target, targetIndex) => {
          return (
          <MonitorPaneCard
            canMoveDown={targetIndex < visibleTargets.length - 1}
            canMoveUp={targetIndex > 0}
            isLive={livePreviewKeys.has(target.key)}
            isWatched={watchedKeySet.has(target.key)}
            key={target.key}
            onMoveDown={() => moveTarget(target.key, 1)}
            onMoveUp={() => moveTarget(target.key, -1)}
            onToggleWatch={() => toggleWatchedTarget(target.key)}
            onOpen={() =>
              onSelectLiveTarget(
                target.workspace.id,
                target.tab.tab_id,
                target.pane.pane_id,
              )
            }
            target={target}
            token={token}
          />
          );
        })}
      </div>
    </section>
  );
}

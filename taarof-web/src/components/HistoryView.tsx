import { useEffect, useMemo, useRef, useState } from "react";
import { fetchHistory } from "../api";
import type { HistoryFilters, HistoryRecord, TaarofStateSnapshot } from "../types";
import {
  historyRenderState,
  projectHistoryRow,
  sanitizedHistoryExport,
} from "./HistoryView.helpers";

interface HistoryViewProps {
  token: string;
  snapshot: TaarofStateSnapshot | null;
  onSelectLiveTarget: (workspaceId: number, tabId: number, paneId: number | null) => void;
}

const INITIAL_FILTERS: HistoryFilters = { order: "desc" };

export function HistoryView({ token, snapshot, onSelectLiveTarget }: HistoryViewProps) {
  const [filters, setFilters] = useState<HistoryFilters>(INITIAL_FILTERS);
  const [records, setRecords] = useState<HistoryRecord[]>([]);
  const [nextId, setNextId] = useState<number | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [scanned, setScanned] = useState(0);
  const [scanExhausted, setScanExhausted] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestGeneration = useRef(0);
  const activeRequest = useRef<AbortController | null>(null);
  const queryKey = JSON.stringify(filters);

  useEffect(() => {
    activeRequest.current?.abort();
    activeRequest.current = null;
    const generation = ++requestGeneration.current;
    setRecords([]);
    setNextId(null);
    setHasMore(false);
    setScanned(0);
    setScanExhausted(false);
    if (snapshot?.history && !snapshot.history.available) {
      setLoading(false);
      setError(null);
      return;
    }
    const controller = new AbortController();
    activeRequest.current = controller;
    setLoading(true);
    setError(null);
    fetchHistory(token, { ...filters, limit: 100 }, controller.signal)
      .then((page) => {
        if (generation !== requestGeneration.current) return;
        setRecords(page.records);
        setNextId(page.next_id);
        setHasMore(page.has_more);
        setScanned(page.scanned);
        setScanExhausted(page.scan_exhausted);
      })
      .catch((reason: unknown) => {
        if (controller.signal.aborted || generation !== requestGeneration.current) return;
        setError(reason instanceof Error ? reason.message : "History request failed");
      })
      .finally(() => {
        if (generation === requestGeneration.current) setLoading(false);
      });
    // Abort whichever request is currently active on unmount, including one
    // started by loadMore after it replaced this effect's controller.
    return () => activeRequest.current?.abort();
  }, [queryKey, snapshot?.history?.available, token]);

  async function loadMore() {
    if (nextId === null || loading) return;
    activeRequest.current?.abort();
    const generation = ++requestGeneration.current;
    const controller = new AbortController();
    activeRequest.current = controller;
    setLoading(true);
    setError(null);
    try {
      const page = await fetchHistory(
        token,
        { ...filters, since_id: nextId, limit: 100 },
        controller.signal,
      );
      if (generation !== requestGeneration.current) return;
      setRecords((current) => [...current, ...page.records]);
      setNextId(page.next_id);
      setHasMore(page.has_more);
      setScanned((current) => current + page.scanned);
      setScanExhausted(page.scan_exhausted);
    } catch (reason) {
      if (!controller.signal.aborted && generation === requestGeneration.current) {
        setError(reason instanceof Error ? reason.message : "History request failed");
      }
    } finally {
      if (generation === requestGeneration.current) setLoading(false);
    }
  }

  const rows = useMemo(
    () => records.map((record) => projectHistoryRow(record, snapshot)),
    [records, snapshot],
  );
  const renderState = historyRenderState({
    loading,
    records,
    error,
    historyStatus: snapshot?.history,
  });
  const statusMessage =
    renderState === "unavailable"
      ? `History unavailable: ${snapshot?.history?.reason ?? snapshot?.history?.state ?? "disabled"}`
      : renderState === "failed"
        ? `History failed: ${error}`
        : renderState === "loading" && records.length === 0
          ? "Searching durable history…"
          : renderState === "empty"
            ? scanExhausted
              ? "No matches in this scan window. Load more to continue."
              : "No durable history matches these filters."
            : `${records.length} historical observations · ${scanned} candidates scanned${
                renderState === "stale" ? " · live verification is stale" : ""
              }${scanExhausted ? " · scan budget reached" : ""}`;

  function updateFilters(update: (current: HistoryFilters) => HistoryFilters) {
    // Invalidate synchronously so an already-queued response cannot render
    // between this input event and the effect that starts its replacement.
    activeRequest.current?.abort();
    requestGeneration.current += 1;
    setFilters(update);
  }

  function setTextFilter(name: keyof HistoryFilters, value: string) {
    updateFilters((current) => ({ ...current, [name]: value || undefined }));
  }

  return (
    <section className="history-view">
      <header className="history-view__header">
        <div>
          <div className="history-view__eyebrow">Durable metadata</div>
          <h2>History</h2>
          <p>Sanitized event, diagnostic, and Work observations across restarts.</p>
        </div>
        <button
          disabled={records.length === 0}
          onClick={() => navigator.clipboard.writeText(sanitizedHistoryExport(records))}
          type="button"
        >
          Copy page export
        </button>
      </header>

      <div className="history-view__filters">
        <input
          aria-label="Search history"
          onChange={(event) => setTextFilter("text", event.target.value)}
          placeholder="Search sanitized summaries and event types"
          type="search"
          value={filters.text ?? ""}
        />
        <select aria-label="Record type" onChange={(event) => setTextFilter("record_type", event.target.value)} value={filters.record_type ?? ""}>
          <option value="">All types</option><option value="event">Events</option><option value="diagnostic">Diagnostics</option><option value="work">Work</option>
        </select>
        <select aria-label="Severity" onChange={(event) => setTextFilter("severity", event.target.value)} value={filters.severity ?? ""}>
          <option value="">All severities</option><option value="info">Info</option><option value="warn">Warn</option><option value="error">Error</option>
        </select>
        <select aria-label="Order" onChange={(event) => setTextFilter("order", event.target.value)} value={filters.order ?? "desc"}>
          <option value="desc">Newest first</option><option value="asc">Oldest first</option>
        </select>
        {(["session", "workspace", "pane", "task", "repository", "authority", "verification"] as const).map((name) => (
          <input key={name} aria-label={name} onChange={(event) => setTextFilter(name, event.target.value)} placeholder={name} value={filters[name] ?? ""} />
        ))}
        <input aria-label="From Unix milliseconds" min="0" onChange={(event) => updateFilters((current) => ({ ...current, from_ts: event.target.value ? Number(event.target.value) : undefined }))} placeholder="from Unix ms" step="1" type="number" value={filters.from_ts ?? ""} />
        <input aria-label="To Unix milliseconds" min="0" onChange={(event) => updateFilters((current) => ({ ...current, to_ts: event.target.value ? Number(event.target.value) : undefined }))} placeholder="to Unix ms" step="1" type="number" value={filters.to_ts ?? ""} />
      </div>

      <p className={`history-view__status history-view__status--${renderState}`} role={renderState === "failed" || renderState === "unavailable" ? "alert" : "status"}>
        {statusMessage}
      </p>

      <div className="history-view__results">
        {rows.map((row) => (
          <article className="history-card" key={row.record.id}>
            <div className="history-card__title">
              <span className="history-card__historical">{row.historicalLabel}</span>
              <strong>{row.typeLabel}</strong>
              {row.record.level ? <span className={`history-card__severity history-card__severity--${row.record.level}`}>{row.record.level}</span> : null}
            </div>
            <p className="history-card__snippet">{row.snippet}</p>
            <p className="history-card__provenance">
              Source {row.sourceLabel} · authority {row.authorityLabel} · verification {row.verificationLabel} · observed at {new Date(row.observedAtUnixMs).toLocaleString()}
            </p>
            <div className="history-card__actions">
              {row.navigation.pane ? <button onClick={() => onSelectLiveTarget(row.navigation.pane!.workspaceId, row.navigation.pane!.tabId, row.navigation.pane!.paneId)} type="button">Focus pane · checked {new Date(row.navigation.pane.checkedAtUnixMs).toLocaleString()}</button> : row.navigation.paneNote ? <span>{row.navigation.paneNote}</span> : null}
              {row.navigation.task ? <button onClick={() => onSelectLiveTarget(row.navigation.task!.workspaceId, row.navigation.task!.tabId, row.navigation.task!.paneId)} type="button">Open {row.navigation.task.taskId} · checked {new Date(row.navigation.task.checkedAtUnixMs).toLocaleString()}</button> : row.navigation.taskNote ? <span>{row.navigation.taskNote}</span> : null}
              {row.navigation.pullRequest ? <a href={row.navigation.pullRequest.url} rel="noreferrer" target="_blank">Open PR · checked {new Date(row.navigation.pullRequest.checkedAtUnixMs).toLocaleString()}</a> : row.navigation.pullRequestNote ? <span>{row.navigation.pullRequestNote}</span> : null}
            </div>
          </article>
        ))}
      </div>
      {hasMore ? <button className="history-view__load-more" disabled={loading} onClick={loadMore} type="button">{loading ? "Loading…" : "Load more"}</button> : null}
    </section>
  );
}

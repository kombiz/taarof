import { useEffect, useMemo, useRef, useState } from "react";
import {
  findLiveBindingTab,
  livePreviewPaneIds,
} from "../agentSessionPanes";
import type {
  AgentJobSnapshot,
  AgentSessionRecord,
  AgentSessionsSnapshot,
  TaarofStateSnapshot,
} from "../types";
import {
  formatRelativeTime,
  formatTimestamp,
  projectLabel,
  readSessionHashKey,
  sessionKey,
  sessionLink,
  shortSessionId,
  writeSessionHashKey,
} from "./AgentsView.helpers";
import { SessionPanePreview } from "./SessionPanePreview";

interface AgentsViewProps {
  isLoading: boolean;
  errorMessage: string | null;
  snapshot: AgentSessionsSnapshot | null;
  taarofSnapshot: TaarofStateSnapshot | null;
  token: string;
  runningJobs: AgentJobSnapshot[];
  onSelectLiveTarget: (workspaceId: number, tabId: number, paneId: number | null) => void;
}

function fallbackCopyText(text: string): boolean {
  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.opacity = "0";
  textarea.style.pointerEvents = "none";
  document.body.appendChild(textarea);
  textarea.select();
  textarea.setSelectionRange(0, text.length);

  try {
    return document.execCommand("copy");
  } finally {
    document.body.removeChild(textarea);
  }
}

async function copyText(text: string): Promise<void> {
  if (window.navigator.clipboard?.writeText) {
    await window.navigator.clipboard.writeText(text);
    return;
  }

  if (!fallbackCopyText(text)) {
    throw new Error("Clipboard copy is unavailable in this browser.");
  }
}

export function AgentsView({
  isLoading,
  errorMessage,
  snapshot,
  taarofSnapshot,
  token,
  runningJobs,
  onSelectLiveTarget,
}: AgentsViewProps) {
  const [selectedSessionKey, setSelectedSessionKey] = useState<string | null>(null);
  const [copiedKey, setCopiedKey] = useState<string | null>(null);
  const [copyError, setCopyError] = useState<string | null>(null);
  const [areTabPanesVisible, setAreTabPanesVisible] = useState(false);
  const clearCopiedTimerRef = useRef<number | null>(null);

  useEffect(() => {
    return () => {
      if (clearCopiedTimerRef.current !== null) {
        window.clearTimeout(clearCopiedTimerRef.current);
      }
    };
  }, []);

  useEffect(() => {
    const sessions = snapshot?.sessions ?? [];
    if (sessions.length === 0) {
      if (selectedSessionKey !== null) {
        setSelectedSessionKey(null);
      }
      return;
    }

    if (
      selectedSessionKey !== null &&
      sessions.some((session) => sessionKey(session) === selectedSessionKey)
    ) {
      return;
    }

    const hashKey = readSessionHashKey();
    if (hashKey && sessions.some((session) => sessionKey(session) === hashKey)) {
      setSelectedSessionKey(hashKey);
      return;
    }

    setSelectedSessionKey(sessionKey(sessions[0]));
  }, [selectedSessionKey, snapshot]);

  const selectedSession = useMemo(() => {
    if (!snapshot || snapshot.sessions.length === 0) {
      return null;
    }

    return (
      snapshot.sessions.find((session) => sessionKey(session) === selectedSessionKey) ??
      snapshot.sessions[0]
    );
  }, [selectedSessionKey, snapshot]);

  const liveBindingTab = useMemo(() => {
    return findLiveBindingTab(taarofSnapshot, selectedSession?.live_binding);
  }, [selectedSession, taarofSnapshot]);
  const livePreviewPaneIdSet = useMemo(
    () => new Set(liveBindingTab ? livePreviewPaneIds(liveBindingTab.tab.panes) : []),
    [liveBindingTab],
  );

  useEffect(() => {
    setAreTabPanesVisible(false);
  }, [selectedSessionKey]);

  function handleSelectSession(session: AgentSessionRecord) {
    const key = sessionKey(session);
    setSelectedSessionKey(key);
    writeSessionHashKey(key);
  }

  async function handleCopyValue(session: AgentSessionRecord, kind: string, text?: string | null) {
    if (!text) {
      return;
    }

    const key = `${sessionKey(session)}:${kind}`;

    try {
      await copyText(text);
      setCopiedKey(key);
      setCopyError(null);
      if (clearCopiedTimerRef.current !== null) {
        window.clearTimeout(clearCopiedTimerRef.current);
      }
      clearCopiedTimerRef.current = window.setTimeout(() => {
        setCopiedKey((current) => (current === key ? null : current));
      }, 2200);
    } catch (error) {
      setCopiedKey(null);
      setCopyError(error instanceof Error ? error.message : "Failed to copy session detail.");
    }
  }

  return (
    <section className="agents-view">
      <div className="agents-view__label">Agents panel</div>
      <div className="agents-view__header">
        <div>
          <h2>Read-only session history</h2>
          <p>
            Browser viewer reconnection only follows an exact pane in taarof&apos;s current snapshot.
            It does not attach a native process. Resume commands are copy-only and never execute
            in the browser.
          </p>
        </div>
        <div className="agents-view__header-meta">
          <span>{runningJobs.length} live</span>
          <span>{snapshot?.sessions.length ?? 0} recent</span>
          <span>{snapshot?.providers.length ?? 0} providers</span>
        </div>
      </div>

      {errorMessage ? <div className="agents-view__notice">{errorMessage}</div> : null}
      {isLoading ? (
        <div className="agents-view__notice agents-view__notice--soft">
          Refreshing agent history from local session stores.
        </div>
      ) : null}
      {copyError ? <div className="agents-view__notice">{copyError}</div> : null}

      <section className="agents-view__section">
        <div className="agents-view__section-heading">
          <h3>Running now</h3>
          <p>Live agent tabs detected in the current taarof session.</p>
        </div>
        {runningJobs.length > 0 ? (
          <div className="agents-view__jobs">
            {runningJobs.map((job) => (
              <article className="agents-view__job-card" key={`${job.workspace_id}:${job.tab_id}`}>
                <div className="agents-view__job-topline">
                  <strong>{job.agent_name ?? "agent"}</strong>
                  <span className="agents-view__status agents-view__status--active">active</span>
                </div>
                <div className="agents-view__job-path">
                  {job.workspace_name} / {job.tab_name}
                </div>
                <p className="agents-view__job-activity">
                  {job.activity?.text ?? job.activity?.state ?? "Live agent activity available."}
                </p>
                <div className="agents-view__job-meta">
                  <span>tab {job.tab_id}</span>
                  {job.pane_id != null ? <span>pane {job.pane_id}</span> : null}
                  {job.session_id ? <code>{job.session_id}</code> : null}
                </div>
                <div className="agents-view__actions">
                  <button
                    onClick={() =>
                      onSelectLiveTarget(job.workspace_id, job.tab_id, job.pane_id ?? null)
                    }
                    type="button"
                  >
                    Reconnect browser viewer
                  </button>
                </div>
              </article>
            ))}
          </div>
        ) : (
          <div className="agents-view__empty">
            No active agent jobs are visible in the current taarof snapshot.
          </div>
        )}
      </section>

      <section className="agents-view__section">
        <div className="agents-view__section-heading">
          <h3>Provider status</h3>
          <p>History is discovered from local session stores on this host.</p>
        </div>
        {snapshot?.providers.length ? (
          <div className="agents-view__providers">
            {snapshot.providers.map((provider) => (
              <article className="agents-view__provider-card" key={provider.name}>
                <div className="agents-view__job-topline">
                  <strong>{provider.name}</strong>
                  <span
                    className={
                      provider.ok
                        ? "agents-view__status agents-view__status--ready"
                        : "agents-view__status agents-view__status--error"
                    }
                  >
                    {provider.ok ? "ok" : "error"}
                  </span>
                </div>
                <p className="agents-view__provider-summary">
                  {provider.history_available
                    ? `${provider.session_count} recent session${
                        provider.session_count === 1 ? "" : "s"
                      }`
                    : "history unavailable"}
                </p>
                {provider.warning ? (
                  <p className="agents-view__provider-message">{provider.warning}</p>
                ) : null}
                {provider.error ? (
                  <p className="agents-view__provider-message agents-view__provider-message--error">
                    {provider.error}
                  </p>
                ) : null}
              </article>
            ))}
          </div>
        ) : (
          <div className="agents-view__empty">
            {isLoading ? "Loading provider status." : "No provider history data is available yet."}
          </div>
        )}
      </section>

      <section className="agents-view__section">
        <div className="agents-view__section-heading">
          <h3>Recent sessions</h3>
          <p>
            Historical sessions keep provider identity separate from exact current browser-viewer
            targets.
          </p>
        </div>
        {snapshot?.sessions.length ? (
          <div className="agents-view__sessions-layout">
            <div className="agents-view__session-list">
              {snapshot.sessions.map((session) => {
                const key = sessionKey(session);
                const isSelected = key === sessionKey(selectedSession ?? session);
                return (
                  <button
                    aria-current={isSelected ? "true" : undefined}
                    className={
                      isSelected
                        ? "agents-view__session-row agents-view__session-row--selected"
                        : "agents-view__session-row"
                    }
                    key={key}
                    onClick={() => handleSelectSession(session)}
                    type="button"
                  >
                    <div className="agents-view__job-topline">
                      <strong className="agents-view__session-title">{session.title}</strong>
                      <span
                        className={
                          session.status === "active"
                            ? "agents-view__status agents-view__status--active"
                            : "agents-view__status"
                        }
                      >
                        {session.status}
                      </span>
                    </div>
                    <div className="agents-view__session-subtitle">
                      {session.agent} · {projectLabel(session)}
                      {session.host ? ` · ${session.host}` : ""}
                    </div>
                    <div className="agents-view__session-path">
                      {session.repo_root ?? session.cwd}
                    </div>
                    <div className="agents-view__session-meta">
                      <span>{formatRelativeTime(session.updated_at_unix_ms)}</span>
                      <code>{shortSessionId(session.session_id)}</code>
                      {session.live_binding ? <span>live in taarof</span> : null}
                    </div>
                  </button>
                );
              })}
            </div>

            {selectedSession ? (
              <article className="agents-view__detail-card">
                <div className="agents-view__job-topline">
                  <div>
                    <h4>{selectedSession.title}</h4>
                    <p>
                      {selectedSession.agent} · {projectLabel(selectedSession)} ·{" "}
                      {selectedSession.host ? `${selectedSession.host} · ` : ""}
                      {formatRelativeTime(selectedSession.updated_at_unix_ms)}
                    </p>
                  </div>
                  <span
                    className={
                      selectedSession.status === "active"
                        ? "agents-view__status agents-view__status--active"
                        : "agents-view__status"
                    }
                  >
                    {selectedSession.status}
                  </span>
                </div>

                <div className="agents-view__detail-grid">
                  <article>
                    <span className="agents-view__meta-label">Session id</span>
                    <code>{selectedSession.session_id}</code>
                  </article>
                  <article>
                    <span className="agents-view__meta-label">Provider</span>
                    <strong>{selectedSession.agent}</strong>
                  </article>
                  <article>
                    <span className="agents-view__meta-label">Updated</span>
                    <strong>{formatTimestamp(selectedSession.updated_at_unix_ms)}</strong>
                  </article>
                  <article>
                    <span className="agents-view__meta-label">Started</span>
                    <strong>{formatTimestamp(selectedSession.started_at_unix_ms)}</strong>
                  </article>
                  <article>
                    <span className="agents-view__meta-label">Repository</span>
                    <strong>{selectedSession.repo_root ?? "unknown"}</strong>
                  </article>
                  <article className="agents-view__detail-grid-wide">
                    <span className="agents-view__meta-label">cwd</span>
                    <strong>{selectedSession.cwd}</strong>
                  </article>
                </div>

                <div className="agents-view__actions agents-view__actions--secondary">
                  <button
                    onClick={() =>
                      void handleCopyValue(
                        selectedSession,
                        "id",
                        selectedSession.session_id,
                      )
                    }
                    type="button"
                  >
                    {copiedKey === `${sessionKey(selectedSession)}:id`
                      ? "Copied"
                      : "Copy session id"}
                  </button>
                  <button
                    onClick={() =>
                      void handleCopyValue(
                        selectedSession,
                        "link",
                        sessionLink(sessionKey(selectedSession)),
                      )
                    }
                    type="button"
                  >
                    {copiedKey === `${sessionKey(selectedSession)}:link`
                      ? "Copied"
                      : "Copy session link"}
                  </button>
                </div>

                {selectedSession.live_binding ? (
                  <div className="agents-view__binding">
                    <div>
                      <span className="agents-view__meta-label">Live taarof tab</span>
                      <strong>
                        {selectedSession.live_binding.workspace_name} /{" "}
                        {selectedSession.live_binding.tab_name}
                      </strong>
                    </div>
                    <div className="agents-view__binding-actions">
                      {liveBindingTab ? (
                        <>
                          <button
                            onClick={() =>
                              onSelectLiveTarget(
                                liveBindingTab.workspace.id,
                                liveBindingTab.tab.tab_id,
                                selectedSession.live_binding!.pane_id,
                              )
                            }
                            type="button"
                          >
                            Reconnect browser viewer
                          </button>
                          <button
                            aria-expanded={areTabPanesVisible}
                            onClick={() => setAreTabPanesVisible((current) => !current)}
                            type="button"
                          >
                            {areTabPanesVisible ? "Hide tab panes" : "Show tab panes"}
                          </button>
                        </>
                      ) : (
                        <span className="agents-view__empty">
                          Browser viewer reconnect unavailable: the exact current pane is missing.
                        </span>
                      )}
                    </div>
                  </div>
                ) : (
                  <div className="agents-view__empty">
                    Browser viewer reconnect unavailable: this history record has no exact current
                    pane binding.
                  </div>
                )}

                {areTabPanesVisible && liveBindingTab ? (
                  <section className="agents-view__pane-viewer">
                    <div className="agents-view__pane-viewer-header">
                      <div>
                        <h3>Tab panes</h3>
                        <p>
                          {liveBindingTab.workspace.name} / {liveBindingTab.tab.name} ·{" "}
                          {liveBindingTab.tab.panes.length} pane
                          {liveBindingTab.tab.panes.length === 1 ? "" : "s"} ·{" "}
                          {livePreviewPaneIdSet.size} live preview
                          {livePreviewPaneIdSet.size === 1 ? "" : "s"}
                        </p>
                      </div>
                      <button onClick={() => setAreTabPanesVisible(false)} type="button">
                        Hide
                      </button>
                    </div>
                    {liveBindingTab.tab.panes.length > 0 ? (
                      <div className="agents-view__pane-grid">
                        {liveBindingTab.tab.panes.map((pane) => (
                          <SessionPanePreview
                            isLivePreviewEnabled={livePreviewPaneIdSet.has(pane.pane_id)}
                            key={`${liveBindingTab.tab.tab_id}:${pane.pane_id}`}
                            pane={pane}
                            tab={liveBindingTab.tab}
                            token={token}
                          />
                        ))}
                      </div>
                    ) : (
                      <div className="agents-view__empty">
                        This live tab has no visible panes in the current snapshot.
                      </div>
                    )}
                  </section>
                ) : null}

                <div className="agents-view__command-card">
                  <div className="agents-view__section-heading agents-view__section-heading--tight">
                    <h3>Resume agent conversation</h3>
                    <p>
                      Copy-only provider command for this saved session identity. The browser does
                      not run it or infer queue, steer, interrupt, or rewind support.
                    </p>
                  </div>
                  {selectedSession.resume_command ? (
                    <>
                      <pre className="agents-view__command">
                        <code>{selectedSession.resume_command}</code>
                      </pre>
                      <div className="agents-view__actions">
                        <button
                          onClick={() =>
                            void handleCopyValue(
                              selectedSession,
                              "resume",
                              selectedSession.resume_command,
                            )
                          }
                          type="button"
                        >
                          {copiedKey === `${sessionKey(selectedSession)}:resume`
                            ? "Copied"
                            : "Copy resume command"}
                        </button>
                      </div>
                    </>
                  ) : (
                    <div className="agents-view__empty">
                      {selectedSession.resume_unavailable_reason ??
                        "Resume agent conversation unavailable: no exact provider resume authority was validated on this host."}
                    </div>
                  )}
                </div>

                <div className="agents-view__command-card">
                  <div className="agents-view__section-heading agents-view__section-heading--tight">
                    <h3>Reopen workspace layout</h3>
                    <p>
                      Desktop-owned saved arrangement. This browser mirrors the current desktop
                      snapshot and cannot open or replace saved layout state.
                    </p>
                  </div>
                  <div className="agents-view__empty">
                    Reopen workspace layout unavailable in the browser; use the native application.
                  </div>
                </div>
              </article>
            ) : null}
          </div>
        ) : (
          <div className="agents-view__empty">
            {isLoading
              ? "Loading recent agent sessions."
              : "No recent agent sessions were discovered on this host."}
          </div>
        )}
      </section>
    </section>
  );
}

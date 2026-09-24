import {
  Component,
  Suspense,
  useCallback,
  lazy,
  startTransition,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import {
  ApiUnauthorizedError,
  buildEventsWebSocketUrl,
  fetchAgentSessions,
  fetchEvents,
  fetchRuntimeIdentity,
  fetchState,
} from "./api";
import {
  bootstrapTokenFromUrl,
  clearStoredToken,
  getStoredToken,
  storeToken,
} from "./auth";
import { AgentsView } from "./components/AgentsView";
import { selectPane, selectStage, selectTab, viewModeFromHash } from "./components/PaneStage.helpers";
import { Sidebar } from "./components/Sidebar";
import { TokenGate } from "./components/TokenGate";
import {
  EventRecoveryController,
  type EventConnectionStatus,
} from "./eventRecovery";
import {
  DatasetRefreshScheduler,
  RefreshRequestTimeoutError,
  classifyEventRefreshDomains,
  runGuardedRefresh,
} from "./refreshScheduler";
import type {
  AgentSessionsSnapshot,
  DashboardSnapshot,
  TaarofStateSnapshot,
} from "./types";

type ViewMode = "panes" | "monitor" | "agents" | "history";

const LazyMonitorView = lazy(() =>
  import("./components/MonitorView").then(({ MonitorView }) => ({
    default: MonitorView,
  })),
);
const LazyPaneView = lazy(() =>
  import("./components/PaneView").then(({ PaneView }) => ({
    default: PaneView,
  })),
);
const LazyHistoryView = lazy(() =>
  import("./components/HistoryView").then(({ HistoryView }) => ({
    default: HistoryView,
  })),
);

interface LazyTerminalViewBoundaryProps {
  children: ReactNode;
  fallback: ReactNode;
  resetKey: string;
}

interface LazyTerminalViewBoundaryState {
  hasError: boolean;
}

class LazyTerminalViewBoundary extends Component<
  LazyTerminalViewBoundaryProps,
  LazyTerminalViewBoundaryState
> {
  state: LazyTerminalViewBoundaryState = { hasError: false };

  static getDerivedStateFromError(): LazyTerminalViewBoundaryState {
    return { hasError: true };
  }

  componentDidUpdate(previousProps: LazyTerminalViewBoundaryProps) {
    if (previousProps.resetKey !== this.props.resetKey && this.state.hasError) {
      this.setState({ hasError: false });
    }
  }

  render() {
    return this.state.hasError ? this.props.fallback : this.props.children;
  }
}

function MonitorViewLoadingFallback() {
  return (
    <section className="monitor-view monitor-view--empty" aria-busy="true">
      <div className="monitor-view__label">Terminal board</div>
      <h2>Loading terminal board</h2>
    </section>
  );
}

function MonitorViewErrorFallback() {
  return (
    <section className="monitor-view monitor-view--empty" role="alert">
      <div className="monitor-view__label">Terminal board</div>
      <h2>Terminal board failed to load</h2>
    </section>
  );
}

function PaneViewLoadingFallback() {
  return (
    <section className="pane-view pane-view--empty" aria-busy="true">
      <div className="pane-view__label">Pane surface</div>
      <h2>Loading pane surface</h2>
      <p>Preparing the selected terminal surface.</p>
    </section>
  );
}

function PaneViewErrorFallback() {
  return (
    <section className="pane-view pane-view--empty" role="alert">
      <div className="pane-view__label">Pane surface</div>
      <h2>Pane surface failed to load</h2>
      <p>Refresh the browser client and try opening the pane again.</p>
    </section>
  );
}

function HistoryViewLoadingFallback() {
  return <section className="history-view history-view--empty" aria-busy="true"><h2>Loading History</h2></section>;
}

function HistoryViewErrorFallback() {
  return <section className="history-view history-view--empty" role="alert"><h2>History view failed to load</h2></section>;
}

function initialViewMode(): ViewMode {
  return viewModeFromHash(window.location.hash);
}

function selectedDashboard(snapshot: TaarofStateSnapshot | null): DashboardSnapshot | null {
  return snapshot?.dashboard ?? null;
}

function eventStatusLabel(status: EventConnectionStatus) {
  const connection = `${status.connection[0].toUpperCase()}${status.connection.slice(1)}`;
  if (status.freshness === "verified") return `${connection} · verified`;
  if (status.last_verified_at_unix_ms === null) return `${connection} · stale · never verified`;
  return `${connection} · stale · last verified ${new Date(
    status.last_verified_at_unix_ms,
  ).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })}`;
}

export function App() {
  const [token, setToken] = useState<string | null>(() => {
    return bootstrapTokenFromUrl() ?? getStoredToken();
  });
  const [tokenError, setTokenError] = useState<string | null>(null);
  const [snapshot, setSnapshot] = useState<TaarofStateSnapshot | null>(null);
  const [agentSessions, setAgentSessions] = useState<AgentSessionsSnapshot | null>(null);
  const [selectedWorkspaceId, setSelectedWorkspaceId] = useState<number | null>(null);
  const [selectedTabId, setSelectedTabId] = useState<number | null>(null);
  const [selectedPaneId, setSelectedPaneId] = useState<number | null>(null);
  const [viewMode, setViewMode] = useState<ViewMode>(() => initialViewMode());
  const [isLoading, setIsLoading] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [areAgentSessionsLoading, setAreAgentSessionsLoading] = useState(false);
  const [agentSessionsError, setAgentSessionsError] = useState<string | null>(null);
  const [paneRefreshGeneration, setPaneRefreshGeneration] = useState(0);
  const [isNavOpen, setIsNavOpen] = useState(false);
  const [eventStatus, setEventStatus] = useState<EventConnectionStatus>({
    connection: "disconnected",
    freshness: "stale",
    cursor: null,
    last_verified_at_unix_ms: null,
    attempt: 0,
  });
  const navigationTriggerRef = useRef<HTMLButtonElement | null>(null);
  const refreshGenerationRef = useRef(0);
  const stateRefreshRef = useRef<DatasetRefreshScheduler | null>(null);
  const agentSessionsRefreshRef = useRef<DatasetRefreshScheduler | null>(null);
  const eventRecoveryRef = useRef<EventRecoveryController | null>(null);

  const selectedStage = selectStage(snapshot, {
    workspaceId: selectedWorkspaceId,
    tabId: selectedTabId,
    paneId: selectedPaneId,
  });
  const { workspace: selectedWorkspace, tab: selectedTab, pane: selectedPane } = selectedStage;
  const dashboard = selectedDashboard(snapshot);
  const agentJobs = snapshot?.agent_jobs ?? [];

  function advanceRefreshGeneration() {
    refreshGenerationRef.current += 1;
    stateRefreshRef.current?.dispose();
    agentSessionsRefreshRef.current?.dispose();
    stateRefreshRef.current = null;
    agentSessionsRefreshRef.current = null;
    return refreshGenerationRef.current;
  }

  function handleUnauthorized(message: string) {
    eventRecoveryRef.current?.stop();
    eventRecoveryRef.current = null;
    advanceRefreshGeneration();
    clearStoredToken();
    setSnapshot(null);
    setAgentSessions(null);
    setToken(null);
    setTokenError(message);
    setIsLoading(false);
    setAreAgentSessionsLoading(false);
    setLoadError(null);
    setAgentSessionsError(null);
    setSelectedWorkspaceId(null);
    setSelectedTabId(null);
    setSelectedPaneId(null);
    setIsNavOpen(false);
    setViewMode("panes");
  }

  useEffect(() => {
    let disposed = false;
    let generation = advanceRefreshGeneration();

    if (!token) {
      setSnapshot(null);
      setAgentSessions(null);
      setSelectedWorkspaceId(null);
      setSelectedTabId(null);
      setSelectedPaneId(null);
      setViewMode("panes");
      setIsLoading(false);
      setAreAgentSessionsLoading(false);
      setLoadError(null);
      setAgentSessionsError(null);
      setIsNavOpen(false);
      setEventStatus({
        connection: "disconnected",
        freshness: "stale",
        cursor: null,
        last_verified_at_unix_ms: null,
        attempt: 0,
      });
      return () => {
        disposed = true;
        if (refreshGenerationRef.current === generation) refreshGenerationRef.current += 1;
      };
    }

    const installRefreshSchedulers = () => {
      generation = advanceRefreshGeneration();
      const installedGeneration = generation;
      const isCurrent = () =>
        !disposed && refreshGenerationRef.current === installedGeneration;
      const stateRefresh = new DatasetRefreshScheduler((signal) =>
        runGuardedRefresh({
          signal,
          isCurrent,
          load: () => fetchState(token, signal),
          enqueueWrite: (write) => startTransition(write),
          onLoading: setIsLoading,
          onSuccess: (nextSnapshot) => {
            setSnapshot(nextSnapshot);
            setPaneRefreshGeneration((current) => current + 1);
            eventRecoveryRef.current?.markSnapshotVerified();
          },
          onError: (error) => {
            setLoadError(
              error === null
                ? null
                : error instanceof RefreshRequestTimeoutError
                  ? "Refreshing taarof state timed out. Displaying the last snapshot."
                  : error instanceof Error
                    ? error.message
                    : "Failed to load taarof state.",
            );
          },
          isUnauthorized: (error) => error instanceof ApiUnauthorizedError,
          onUnauthorized: () => {
            handleUnauthorized("The taarof token was rejected. Paste a current token.");
          },
        }),
      );
      const agentSessionsRefresh = new DatasetRefreshScheduler((signal) =>
        runGuardedRefresh({
          signal,
          isCurrent,
          load: () => fetchAgentSessions(token, signal),
          enqueueWrite: (write) => startTransition(write),
          onLoading: setAreAgentSessionsLoading,
          onSuccess: setAgentSessions,
          onError: (error) => {
            setAgentSessionsError(
              error === null
                ? null
                : error instanceof RefreshRequestTimeoutError
                  ? "Refreshing recent agent sessions timed out. Displaying the last catalog."
                  : error instanceof Error
                    ? error.message
                    : "Failed to load recent agent sessions.",
            );
          },
          isUnauthorized: (error) => error instanceof ApiUnauthorizedError,
          onUnauthorized: () => {
            handleUnauthorized("The taarof token was rejected. Paste a current token.");
          },
        }),
      );
      stateRefreshRef.current = stateRefresh;
      agentSessionsRefreshRef.current = agentSessionsRefresh;
      return { stateRefresh, agentSessionsRefresh, isCurrent };
    };

    let schedulers = installRefreshSchedulers();
    schedulers.stateRefresh.invalidate({ immediate: true });
    schedulers.agentSessionsRefresh.invalidate({ immediate: true });

    const recoverSnapshot = async (signal: AbortSignal) => {
      const recoveryGeneration = generation;
      const ownsGeneration = () =>
        !disposed && refreshGenerationRef.current === recoveryGeneration;
      const isCurrent = () =>
        ownsGeneration() && !signal.aborted;
      if (!isCurrent()) throw signal.reason ?? new DOMException("Recovery replaced.", "AbortError");
      setIsLoading(true);
      setAreAgentSessionsLoading(true);
      setLoadError(null);
      setAgentSessionsError(null);
      try {
        const [stateResult, sessionsResult] = await Promise.allSettled([
          fetchState(token, signal),
          fetchAgentSessions(token, signal),
        ]);
        if (!isCurrent()) {
          throw signal.reason ?? new DOMException("Recovery replaced.", "AbortError");
        }
        if (stateResult.status === "rejected" || sessionsResult.status === "rejected") {
          if (stateResult.status === "rejected" && !(stateResult.reason instanceof ApiUnauthorizedError)) {
            setLoadError(
              stateResult.reason instanceof Error
                ? stateResult.reason.message
                : "Failed to recover taarof state.",
            );
          }
          if (
            sessionsResult.status === "rejected" &&
            !(sessionsResult.reason instanceof ApiUnauthorizedError)
          ) {
            setAgentSessionsError(
              sessionsResult.reason instanceof Error
                ? sessionsResult.reason.message
                : "Failed to recover recent agent sessions.",
            );
          }
          if (stateResult.status === "rejected") throw stateResult.reason;
          if (sessionsResult.status === "rejected") throw sessionsResult.reason;
        }
        startTransition(() => {
          if (!isCurrent()) return;
          setSnapshot(stateResult.value);
          setAgentSessions(sessionsResult.value);
          setPaneRefreshGeneration((current) => current + 1);
        });
      } finally {
        if (ownsGeneration()) {
          setIsLoading(false);
          setAreAgentSessionsLoading(false);
        }
      }
    };

    const eventRecovery = new EventRecoveryController({
      createSocket: () => {
        const socket = new WebSocket(buildEventsWebSocketUrl(token));
        return {
          addEventListener: (type, listener) =>
            socket.addEventListener(type, listener as EventListener),
          removeEventListener: (type, listener) =>
            socket.removeEventListener(type, listener as EventListener),
          close: () => socket.close(),
        };
      },
      fetchRuntimeIdentity: async (signal) =>
        (await fetchRuntimeIdentity(token, signal)).runtime_id,
      fetchEvents: (cursor, signal) => fetchEvents(token, cursor, signal),
      recoverSnapshot,
      onEvent: (data, source) => {
        if (source === "recovery") return;
        for (const domain of classifyEventRefreshDomains(data)) {
          if (domain === "state") stateRefreshRef.current?.invalidate();
          else agentSessionsRefreshRef.current?.invalidate();
        }
      },
      onRecoveryBoundary: () => {
        schedulers = installRefreshSchedulers();
        setIsLoading(false);
        setAreAgentSessionsLoading(false);
      },
      onRuntimeReset: () => {
        schedulers = installRefreshSchedulers();
        setIsLoading(false);
        setAreAgentSessionsLoading(false);
      },
      onUnauthorized: () => {
        handleUnauthorized("The taarof token was rejected. Paste a current token.");
      },
      onStatus: (status) => {
        if (!disposed) setEventStatus(status);
      },
      isUnauthorized: (error) => error instanceof ApiUnauthorizedError,
    });
    eventRecoveryRef.current = eventRecovery;
    eventRecovery.start();

    return () => {
      disposed = true;
      if (eventRecoveryRef.current === eventRecovery) eventRecoveryRef.current = null;
      eventRecovery.stop();
      schedulers.stateRefresh.dispose();
      schedulers.agentSessionsRefresh.dispose();
      if (stateRefreshRef.current === schedulers.stateRefresh) stateRefreshRef.current = null;
      if (agentSessionsRefreshRef.current === schedulers.agentSessionsRefresh) {
        agentSessionsRefreshRef.current = null;
      }
      if (refreshGenerationRef.current === generation) refreshGenerationRef.current += 1;
    };
  }, [token]);

  const closeNavigation = useCallback(() => {
    setIsNavOpen(false);
    window.requestAnimationFrame(() => navigationTriggerRef.current?.focus());
  }, []);

  useEffect(() => {
    if (!isNavOpen) {
      return;
    }

    function handleKeyDown(event: KeyboardEvent) {
      if (event.key === "Escape") {
        closeNavigation();
      }
    }

    window.addEventListener("keydown", handleKeyDown);
    return () => {
      window.removeEventListener("keydown", handleKeyDown);
    };
  }, [closeNavigation, isNavOpen]);

  useEffect(() => {
    if (!snapshot) {
      return;
    }

    if (selectedWorkspace?.id !== selectedWorkspaceId) {
      setSelectedWorkspaceId(selectedWorkspace?.id ?? null);
    }
    if (selectedTab?.tab_id !== selectedTabId) {
      setSelectedTabId(selectedTab?.tab_id ?? null);
    }
    if (selectedPane?.pane_id !== selectedPaneId) {
      setSelectedPaneId(selectedPane?.pane_id ?? null);
    }
  }, [
    selectedPane,
    selectedPaneId,
    selectedTab,
    selectedTabId,
    selectedWorkspace,
    selectedWorkspaceId,
    snapshot,
  ]);

  function handleTokenSubmit(nextToken: string) {
    if (!nextToken) {
      setTokenError("A bearer token is required.");
      return;
    }

    storeToken(nextToken);
    eventRecoveryRef.current?.stop();
    eventRecoveryRef.current = null;
    advanceRefreshGeneration();
    setToken(nextToken);
    setTokenError(null);
  }

  function handleResetToken() {
    eventRecoveryRef.current?.stop();
    eventRecoveryRef.current = null;
    advanceRefreshGeneration();
    clearStoredToken();
    setSnapshot(null);
    setToken(null);
    setTokenError(null);
    setAgentSessions(null);
    setLoadError(null);
    setAgentSessionsError(null);
    setSelectedWorkspaceId(null);
    setSelectedTabId(null);
    setSelectedPaneId(null);
    setIsNavOpen(false);
    setViewMode("panes");
  }

  function handleSelectWorkspace(workspaceId: number) {
    setSelectedWorkspaceId(workspaceId);
    setIsNavOpen(false);
    const workspace = snapshot?.workspaces.find((candidate) => candidate.id === workspaceId);
    const nextTab = selectTab(workspace ?? null, null);

    setSelectedTabId(nextTab?.tab_id ?? null);
    setSelectedPaneId(selectPane(nextTab, null)?.pane_id ?? null);
  }

  function handleSelectTab(workspaceId: number, tabId: number) {
    setSelectedWorkspaceId(workspaceId);
    setSelectedTabId(tabId);
    setIsNavOpen(false);
    const workspace = snapshot?.workspaces.find((candidate) => candidate.id === workspaceId);
    const tab = workspace?.tabs.find((candidate) => candidate.tab_id === tabId);
    setSelectedPaneId(selectPane(tab ?? null, null)?.pane_id ?? null);
  }

  function handleSelectPane(paneId: number) {
    setSelectedPaneId(paneId);
  }

  function handleRefresh() {
    stateRefreshRef.current?.invalidate({ immediate: true });
    agentSessionsRefreshRef.current?.invalidate({ immediate: true });
    setPaneRefreshGeneration((current) => current + 1);
  }

  function handleSelectLiveTarget(
    workspaceId: number,
    tabId: number,
    paneId: number | null,
  ) {
    setViewMode("panes");
    setSelectedWorkspaceId(workspaceId);
    setSelectedTabId(tabId);
    setIsNavOpen(false);
    const workspace = snapshot?.workspaces.find((candidate) => candidate.id === workspaceId);
    const tab = workspace?.tabs.find((candidate) => candidate.tab_id === tabId);
    setSelectedPaneId(selectPane(tab ?? null, paneId)?.pane_id ?? null);
  }

  const introMessage =
    viewMode === "history"
      ? snapshot?.history && !snapshot.history.available
        ? snapshot.history.reason ?? `History is ${snapshot.history.state}.`
        : "Search sanitized durable observations across restarts."
      : viewMode === "agents"
      ? areAgentSessionsLoading && !agentSessions
        ? "Loading recent agent sessions."
        : agentSessionsError
          ? agentSessionsError
          : agentSessions
            ? `${agentJobs.length} live agent job(s) and ${agentSessions.sessions.length} recent session(s) are available.`
            : "Waiting for the first agent-session snapshot from taarof."
      : viewMode === "monitor"
        ? isLoading && !snapshot
          ? "Loading the global pane monitor."
          : loadError
            ? loadError
            : snapshot
              ? `${snapshot.workspaces.length} workspace(s), ${snapshot.workspaces.reduce(
                  (count, workspace) => count + workspace.tabs.length,
                  0,
                )} tab(s), and ${snapshot.workspaces.reduce(
                  (count, workspace) =>
                    count +
                    workspace.tabs.reduce(
                      (paneCount, tab) => paneCount + tab.panes.length,
                      0,
                    ),
                  0,
                )} pane(s) are visible in the monitor.`
              : "Waiting for the first successful snapshot from taarof."
      : isLoading
        ? "Loading the current taarof workspace snapshot."
        : loadError
          ? loadError
          : snapshot
            ? `${snapshot.workspaces.length} workspace(s) available in this session.`
            : "Waiting for the first successful snapshot from taarof.";

  const mobileContextTitle =
    viewMode === "history"
      ? "History"
      : viewMode === "agents"
      ? "Agents panel"
      : viewMode === "monitor"
        ? "Terminal board"
      : selectedWorkspace?.name ?? "No workspace selected";
  const mobileContextMessage =
    viewMode === "history"
      ? "Durable events · diagnostics · Work"
      : viewMode === "agents"
      ? `${agentJobs.length} running now · ${agentSessions?.sessions.length ?? 0} recent sessions`
      : viewMode === "monitor"
        ? `${snapshot?.workspaces.length ?? 0} workspaces · ${
            snapshot?.workspaces.reduce(
              (count, workspace) =>
                count +
                workspace.tabs.reduce((paneCount, tab) => paneCount + tab.panes.length, 0),
              0,
            ) ?? 0
          } panes`
      : selectedTab
        ? `${selectedTab.name} · visible panes stay mounted in one stage`
        : "Open the navigator to switch workspaces and tabs.";

  if (!token) {
    return <TokenGate errorMessage={tokenError} onSubmit={handleTokenSubmit} />;
  }

  return (
    <div className={isNavOpen ? "app-shell app-shell--nav-open" : "app-shell"}>
      <a className="skip-link" href="#workspace-stage">Skip to workspace stage</a>
      <Sidebar
        snapshot={snapshot}
        selectedWorkspaceId={selectedWorkspace?.id ?? null}
        selectedTabId={selectedTab?.tab_id ?? null}
        onSelectWorkspace={handleSelectWorkspace}
        onSelectTab={handleSelectTab}
        isDrawerOpen={isNavOpen}
        onClose={closeNavigation}
      />
      <button
        aria-label="Close workspace navigator"
        className="app-shell__backdrop"
        onClick={closeNavigation}
        type="button"
      />
      <main className="main-shell" id="workspace-stage" tabIndex={-1}>
        <header className="main-shell__header">
          <div className="main-shell__header-main">
            <button
              aria-expanded={isNavOpen}
              aria-label="Open workspace navigator"
              className="main-shell__menu-toggle"
              onClick={() => setIsNavOpen(true)}
              ref={navigationTriggerRef}
              type="button"
            >
              Browse
            </button>
            <div>
              <div className="main-shell__eyebrow">taarof web</div>
              <h1>{snapshot?.session_name ?? "Loading session state"}</h1>
            </div>
          </div>
          <div className="main-shell__actions">
            <div className="main-shell__mode-switch" aria-label="View mode">
              <button
                aria-pressed={viewMode === "panes"}
                className={
                  viewMode === "panes"
                    ? "main-shell__mode-button main-shell__mode-button--selected"
                    : "main-shell__mode-button"
                }
                onClick={() => setViewMode("panes")}
                type="button"
              >
                Workspace
              </button>
              <button
                aria-pressed={viewMode === "monitor"}
                className={
                  viewMode === "monitor"
                    ? "main-shell__mode-button main-shell__mode-button--selected"
                    : "main-shell__mode-button"
                }
                onClick={() => setViewMode("monitor")}
                type="button"
              >
                Monitor
              </button>
              <button
                aria-pressed={viewMode === "agents"}
                className={
                  viewMode === "agents"
                    ? "main-shell__mode-button main-shell__mode-button--selected"
                    : "main-shell__mode-button"
                }
                onClick={() => setViewMode("agents")}
                type="button"
              >
                Agents
              </button>
              <button
                aria-pressed={viewMode === "history"}
                className={viewMode === "history" ? "main-shell__mode-button main-shell__mode-button--selected" : "main-shell__mode-button"}
                onClick={() => setViewMode("history")}
                type="button"
              >
                History
              </button>
            </div>
            <span
              className={`main-shell__connection-state main-shell__connection-state--${eventStatus.connection}`}
              data-connection={eventStatus.connection}
              data-freshness={eventStatus.freshness}
              role="status"
            >
              {eventStatusLabel(eventStatus)}
            </span>
            <span className="main-shell__token-state">token cached</span>
            <button onClick={handleRefresh} type="button">
              Refresh
            </button>
            <button onClick={handleResetToken} type="button">
              Forget token
            </button>
          </div>
        </header>

        <section className="main-shell__mobile-context">
          <div className="main-shell__mobile-context-copy">
            <span className="main-shell__eyebrow">Phone view</span>
            <strong>{mobileContextTitle}</strong>
            <p>{mobileContextMessage}</p>
          </div>
          <button onClick={() => setIsNavOpen(true)} type="button">
            {viewMode === "agents" || viewMode === "monitor" || viewMode === "history" ? "Workspaces" : "Switch"}
          </button>
        </section>

        <section className="main-shell__intro" aria-live="polite">
          <p>{introMessage}</p>
          <p className="main-shell__mobile-note">
            {viewMode === "history"
              ? "History is read-only, searches only sanitized fields, and separates observed-at from live checked-at state."
              : viewMode === "agents"
              ? "Agents view stays read-only, uses the same bearer token, and only offers copyable resume commands plus jump-to-live navigation."
              : viewMode === "monitor"
                ? "Terminals view shows every pane in one board and opens a bounded live-preview pool for attachable terminals."
              : "Workspace stage keeps visible panes in observe mode, still relies on the taarof bearer token, and unlocks local control only for the selected pane."}
          </p>
        </section>

        {viewMode === "history" ? (
          <LazyTerminalViewBoundary fallback={<HistoryViewErrorFallback />} resetKey="history">
            <Suspense fallback={<HistoryViewLoadingFallback />}>
              <LazyHistoryView
                onSelectLiveTarget={handleSelectLiveTarget}
                snapshot={snapshot}
                token={token}
              />
            </Suspense>
          </LazyTerminalViewBoundary>
        ) : viewMode === "agents" ? (
          <AgentsView
            errorMessage={agentSessionsError}
            isLoading={areAgentSessionsLoading}
            onSelectLiveTarget={handleSelectLiveTarget}
            runningJobs={agentJobs}
            snapshot={agentSessions}
            taarofSnapshot={snapshot}
            token={token}
          />
        ) : viewMode === "monitor" ? (
          <LazyTerminalViewBoundary
            fallback={<MonitorViewErrorFallback />}
            resetKey="monitor"
          >
            <Suspense fallback={<MonitorViewLoadingFallback />}>
              <LazyMonitorView
                isLoading={isLoading}
                onSelectLiveTarget={handleSelectLiveTarget}
                snapshot={snapshot}
                token={token}
              />
            </Suspense>
          </LazyTerminalViewBoundary>
        ) : (
          <LazyTerminalViewBoundary fallback={<PaneViewErrorFallback />} resetKey="panes">
            <Suspense fallback={<PaneViewLoadingFallback />}>
              <LazyPaneView
                isLoading={isLoading}
                token={token}
                dashboard={dashboard}
                workspace={selectedWorkspace}
                tab={selectedTab}
                selectedPaneId={selectedPane?.pane_id ?? null}
                onSelectPane={handleSelectPane}
                onUnauthorized={handleUnauthorized}
                refreshGeneration={paneRefreshGeneration}
              />
            </Suspense>
          </LazyTerminalViewBoundary>
        )}
      </main>
    </div>
  );
}

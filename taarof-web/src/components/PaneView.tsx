import { Component, useEffect, useRef, type ReactNode } from "react";
import {
  classifyStageLayout,
  hasTerminalStageAttachmentTarget,
  paneStreamKey,
  projectStagePanes,
} from "./PaneStage.helpers";
import { paneTileBoundaryNeedsReset } from "./PaneTerminal.lifecycle";
import { PaneTerminal } from "./PaneTerminal";
import type { DashboardSnapshot, TabSnapshot, WorkspaceSnapshot } from "../types";

interface PaneViewProps {
  isLoading: boolean;
  token: string;
  dashboard: DashboardSnapshot | null;
  workspace: WorkspaceSnapshot | null;
  tab: TabSnapshot | null;
  selectedPaneId: number | null;
  onSelectPane: (paneId: number) => void;
  onUnauthorized: (message: string) => void;
  refreshGeneration: number;
}

class PaneTileBoundary extends Component<
  { children: ReactNode; paneId: number; refreshGeneration: number },
  { failed: boolean }
> {
  state = { failed: false };

  static getDerivedStateFromError() {
    return { failed: true };
  }

  componentDidUpdate(previous: { paneId: number; refreshGeneration: number }) {
    if (
      this.state.failed &&
      paneTileBoundaryNeedsReset(previous, this.props)
    ) {
      this.setState({ failed: false });
    }
  }

  render() {
    return this.state.failed ? (
      <section className="pane-stage__tile pane-stage__tile--failed" role="alert">
        <h2>Pane {this.props.paneId} failed to render</h2>
        <p>Other panes remain available. Refresh the workspace stage to retry this pane.</p>
      </section>
    ) : this.props.children;
  }
}

/** A bounded, tab-scoped terminal stage. Each tile owns its own transport lifecycle. */
export function PaneView({
  isLoading,
  token,
  dashboard,
  workspace,
  tab,
  selectedPaneId,
  onSelectPane,
  onUnauthorized,
  refreshGeneration,
}: PaneViewProps) {
  const previousStageRef = useRef<{ tabId: number | null; paneIds: number[] }>({
    tabId: null,
    paneIds: [],
  });
  const previousPaneIds = previousStageRef.current.tabId === tab?.tab_id
    ? previousStageRef.current.paneIds
    : [];
  const projection = projectStagePanes(tab?.panes ?? [], selectedPaneId, previousPaneIds);

  useEffect(() => {
    previousStageRef.current = {
      tabId: tab?.tab_id ?? null,
      paneIds: projection.ordered.map((candidate) => candidate.pane_id),
    };
  }, [projection.ordered, tab?.tab_id]);

  if (!workspace || !tab) {
    return (
      <section className="pane-view pane-view--empty" aria-labelledby="pane-stage-title">
        <div className="pane-view__label">Workspace stage</div>
        <h2 id="pane-stage-title">{isLoading ? "Loading workspace stage" : "No tab selected"}</h2>
        <p>{isLoading ? "Waiting for the current taarof snapshot to arrive." : "Choose a workspace and tab to open its stage."}</p>
      </section>
    );
  }

  if (tab.kind !== "terminal") {
    return (
      <section className="pane-view pane-view--empty" aria-labelledby="pane-stage-title">
        <div className="pane-view__label">Workspace stage</div>
        <h2 id="pane-stage-title">{tab.name} is not a terminal tab</h2>
        <p>This {tab.kind} tab has no browser terminal stage. Use Monitor for cross-workspace terminal observation.</p>
        {tab.kind === "dashboard" ? <p>Dashboard reports: {dashboard?.sessions?.length ?? 0} sessions and {dashboard?.hosts?.length ?? 0} hosts.</p> : null}
      </section>
    );
  }

  if (!hasTerminalStageAttachmentTarget(tab)) {
    return (
      <section className="pane-view pane-view--empty" aria-labelledby="pane-stage-title">
        <div className="pane-view__label">Workspace stage</div>
        <h2 id="pane-stage-title">{tab.name} has no panes</h2>
        <p>There is no terminal to attach to in the current snapshot.</p>
      </section>
    );
  }

  const layout = classifyStageLayout(projection.visible.length);
  return (
    <section className="pane-stage" aria-labelledby="pane-stage-title">
      <header className="pane-stage__header">
        <div>
          <div className="pane-view__label">Workspace stage</div>
          <h2 id="pane-stage-title">{workspace.name} · {tab.name}</h2>
          <p>{projection.visible.length} visible pane{projection.visible.length === 1 ? "" : "s"}; select a tile to unlock its control.</p>
        </div>
        {projection.hidden.length > 0 ? (
          <details className="pane-stage__overflow">
            <summary>{projection.hidden.length} hidden pane{projection.hidden.length === 1 ? "" : "s"}</summary>
            <div aria-label="Hidden panes">
              {projection.hidden.map((candidate) => (
                <button key={paneStreamKey(tab.tab_id, candidate.pane_id)} onClick={() => onSelectPane(candidate.pane_id)} type="button">
                  Pane {candidate.pane_id} · {candidate.attach_kind ?? "unknown"}
                </button>
              ))}
            </div>
          </details>
        ) : null}
      </header>
      <div className={`pane-stage__grid pane-stage__grid--${layout}`}>
        {projection.visible.map((candidate) => (
          <PaneTileBoundary
            key={paneStreamKey(tab.tab_id, candidate.pane_id)}
            paneId={candidate.pane_id}
            refreshGeneration={refreshGeneration}
          >
            <PaneTerminal
              dashboard={dashboard}
              isLoading={isLoading}
              isSelected={candidate.pane_id === selectedPaneId}
              onSelectPane={onSelectPane}
              onUnauthorized={onUnauthorized}
              pane={candidate}
              selectedPaneId={selectedPaneId}
              tab={tab}
              token={token}
              workspace={workspace}
            />
          </PaneTileBoundary>
        ))}
      </div>
    </section>
  );
}

import type { TaarofStateSnapshot } from "../types";
import { runtimeProbePresentation } from "../runtimeProbe";

interface SidebarProps {
  snapshot: TaarofStateSnapshot | null;
  selectedWorkspaceId: number | null;
  selectedTabId: number | null;
  onSelectWorkspace: (workspaceId: number) => void;
  onSelectTab: (workspaceId: number, tabId: number) => void;
  isDrawerOpen: boolean;
  onClose: () => void;
}

export function Sidebar({
  snapshot,
  selectedWorkspaceId,
  selectedTabId,
  onSelectWorkspace,
  onSelectTab,
  isDrawerOpen,
  onClose,
}: SidebarProps) {
  const selectedWorkspace =
    snapshot?.workspaces.find((workspace) => workspace.id === selectedWorkspaceId) ?? null;
  const runtimeProbe = runtimeProbePresentation(snapshot?.runtime_probe);

  return (
    <aside className={isDrawerOpen ? "sidebar sidebar--drawer-open" : "sidebar"}>
      <div className="sidebar__header">
        <div>
          <span className="sidebar__eyebrow">taarof navigator</span>
          <h2>{snapshot?.session_name ?? "Scaffold shell"}</h2>
        </div>
        <button
          aria-label="Close navigator"
          className="sidebar__mobile-close"
          onClick={onClose}
          type="button"
        >
          Done
        </button>
      </div>

      {runtimeProbe ? (
        <div
          className={`sidebar__runtime-probe sidebar__runtime-probe--${runtimeProbe.tone}`}
          role={runtimeProbe.tone === "error" ? "alert" : "status"}
          title={runtimeProbe.detail}
        >
          <strong>{runtimeProbe.label}</strong>
          <span>{runtimeProbe.detail}</span>
        </div>
      ) : null}

      {snapshot ? (
        <div className="sidebar__sections">
          <section className="sidebar__section">
            <div className="sidebar__section-title">Workspaces</div>
            <div className="sidebar__workspaces">
              {snapshot.workspaces.map((workspace) => (
                <button
                  className={
                    workspace.id === selectedWorkspaceId
                      ? "sidebar__workspace sidebar__workspace--selected"
                      : "sidebar__workspace"
                  }
                  key={workspace.id}
                  onClick={() => onSelectWorkspace(workspace.id)}
                  type="button"
                >
                  <span className="sidebar__workspace-main">
                    <span className="sidebar__workspace-title-row">
                      <strong>{workspace.name}</strong>
                      {workspace.tool_version_chip ? (
                        <span className="workspace-tool-chip workspace-tool-chip--sidebar">
                          {workspace.tool_version_chip}
                        </span>
                      ) : null}
                    </span>
                    <span className="sidebar__workspace-path">
                      {workspace.branch_name
                        ? `${workspace.branch_name} · ${workspace.repo_root ?? "no repo"}`
                        : workspace.repo_root ?? "No repository"}
                    </span>
                  </span>
                  <span className="sidebar__meta">{workspace.tab_count} tabs</span>
                </button>
              ))}
            </div>
          </section>

          <section className="sidebar__section">
            <div className="sidebar__section-title">
              {selectedWorkspace ? `Tabs in ${selectedWorkspace.name}` : "Tabs"}
            </div>
            {selectedWorkspace ? (
              <div className="sidebar__tabs">
                {selectedWorkspace.tabs.map((tab) => (
                  <button
                    className={
                      tab.tab_id === selectedTabId
                        ? "sidebar__tab sidebar__tab--selected"
                        : "sidebar__tab"
                    }
                    key={tab.tab_id}
                    onClick={() => onSelectTab(selectedWorkspace.id, tab.tab_id)}
                    type="button"
                  >
                    <span className="sidebar__tab-main">
                      <strong>{tab.name}</strong>
                      <span className="sidebar__tab-meta">
                        {tab.kind}
                        {tab.listening_ports.length > 0
                          ? ` · ${tab.listening_ports.length} port${
                              tab.listening_ports.length === 1 ? "" : "s"
                            }`
                          : ""}
                      </span>
                    </span>
                    <span className="sidebar__tab-badges">
                      {tab.kind === "dashboard" ? (
                        <span className="sidebar__badge sidebar__badge--neutral">
                          dashboard
                        </span>
                      ) : null}
                      {tab.agent_name ? (
                        <span
                          className={
                            tab.agent_running
                              ? "sidebar__badge sidebar__badge--active"
                              : "sidebar__badge sidebar__badge--neutral"
                          }
                        >
                          {tab.agent_running ? "agent" : tab.agent_name}
                        </span>
                      ) : null}
                      {tab.needs_attention ? (
                        <span className="sidebar__badge">alert</span>
                      ) : null}
                    </span>
                  </button>
                ))}
              </div>
            ) : (
              <div className="sidebar__empty">
                <p>Select a workspace to inspect its tabs.</p>
              </div>
            )}
          </section>

          <section className="sidebar__section sidebar__section--footnote">
            <p>
              Read-only local browsing over the existing taarof bearer token,
              responsive one-pane mobile view, and tmux pane attach WebSocket.
            </p>
          </section>
        </div>
      ) : (
        <div className="sidebar__empty">
          <p>No workspace snapshot has loaded yet.</p>
          <p>Check the token, the local HTTP server, and the current session.</p>
        </div>
      )}
    </aside>
  );
}

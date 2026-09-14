//! App-level session persistence and shutdown handling.
//!
//! This module owns restoring the default workspace, serializing UI session
//! state, and reacting to process shutdown signals. It must not own window
//! construction or long-running runtime polling.

use adw::prelude::*;

use std::cell::Cell;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use crate::{git, session, sidebar, terminal, workspace, AppState, RuntimeHandle};

pub(crate) fn workspace_git_discovery_path(saved_ws: &session::SavedWorkspace) -> Option<&str> {
    saved_ws
        .working_tree_path
        .as_deref()
        .or(saved_ws.repo_root.as_deref())
}

pub(crate) fn normalize_persisted_path(path: Option<&str>) -> Option<String> {
    path.map(str::trim)
        .filter(|path| !path.is_empty())
        .map(git::normalize_path)
}

/// Create the default workspace with a single shell tab when no prior session
/// could be restored.
pub(crate) fn restore_default_workspace(
    runtime: &RuntimeHandle,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    let state = runtime.shared_state();
    let (ws_id, ws_name, ws_origin) = {
        let st = state.borrow();
        let ws = st.workspaces.first().unwrap();
        (ws.id, ws.name.clone(), ws.work_origin.clone())
    };
    // The default shell is the launch boundary. Git discovery follows it on a
    // worker so a slow filesystem or Git config never delays the first usable
    // tab; the GTK apply phase validates that this original workspace remains.
    sidebar::add_workspace_header(tab_list, &state, term_stack, ws_id, &ws_name);

    let tab_id = terminal::create_terminal(runtime, term_stack, "Shell", None, None);
    sidebar::add_tab_row(tab_list, &state, term_stack, tab_id, "Shell", true);
    terminal::wire_tab_terminals(&state, term_stack, tab_list, window, tab_id);

    let state_for_apply = state.clone();
    let tab_list_for_apply = tab_list.clone();
    let request_key = format!("discover:default-workspace:{ws_id}");
    let submission =
        git::spawn_async(
            request_key,
            || git::discover("."),
            move |git_info| {
                let workspace_still_exists =
                    state_for_apply.borrow().workspaces.iter().any(|workspace| {
                        workspace.id == ws_id && workspace.work_origin == ws_origin
                    });
                if !workspace_still_exists {
                    return;
                }
                let workspace_name = crate::workspace_name_from_git_info(&git_info);
                let header_name = {
                    let mut st = state_for_apply.borrow_mut();
                    let Some(workspace) = st.workspaces.iter_mut().find(|workspace| {
                        workspace.id == ws_id && workspace.work_origin == ws_origin
                    }) else {
                        return;
                    };
                    crate::apply_git_info_to_workspace(workspace, &git_info);
                    if let Some(name) = workspace_name.as_ref() {
                        workspace.name = name.clone();
                    }
                    workspace.name.clone()
                };
                sidebar::set_workspace_header_label(
                    &tab_list_for_apply,
                    &state_for_apply,
                    ws_id,
                    &crate::workspace_header_label(&header_name, &git_info),
                );
            },
        );
    if matches!(submission, git::GitAsyncSubmission::Saturated) {
        crate::show_toast(
            "Git metadata refresh is busy; the default workspace will retry on the next refresh",
        );
    }
}

/// Build the serializable v2 session payload from the canonical `AppState`.
///
/// This is the pure core of session persistence: it derives every saved
/// workspace and tab straight from `AppState`, so persisted state always
/// agrees with the live runtime (including the results of tab moves between
/// workspaces). Kept free of GTK handles so it can be exercised headlessly;
/// `save_session` layers the window geometry and session-name label on top.
#[allow(deprecated)] // VTE 0.78 deprecated current_directory_uri; termprop migration is future scope
pub(crate) fn build_session_state_v2(
    st: &AppState,
    session_name: Option<String>,
    window_width: i32,
    window_height: i32,
) -> session::SessionStateV2 {
    let process_truth_fresh = crate::runtime_probe::runtime_process_truth_is_fresh(st);
    let workspaces: Vec<session::SavedWorkspace> = st
        .workspaces
        .iter()
        .map(|ws| {
            let tabs: Vec<session::SavedTab> = ws
                .tabs
                .iter()
                .filter(|tab| tab.kind == workspace::TabKind::Terminal)
                .map(|tab| {
                    // Lazy-restored tabs (EXAMPLE-84) have not been built yet: their
                    // live `panes` is `PaneNode::Empty`, so serialize the stashed
                    // saved layout verbatim to round-trip splits/tmux/ssh/cwd
                    // without ever spawning them.
                    let (pane_tree, cwd) = if let Some(pending) =
                        st.pending_tab_restores.get(&tab.id)
                    {
                        (pending.saved.clone(), pending.cwd.clone())
                    } else {
                        let pane_tree = terminal::save_pane_tree_for_tab(
                            tab,
                            |pane_id| {
                                st.headless_pane(tab.id, pane_id)
                                    .map_or((None, None), |pane| {
                                        (pane.current_task.clone(), Some(pane.work_origin.clone()))
                                    })
                            },
                            |pane_id| {
                                let status = st.runtime_probe.as_ref().and_then(|snapshot| {
                                    snapshot.pane_agents.get(&(tab.id, pane_id))
                                });
                                let detected_agent_running = process_truth_fresh
                                    && status.is_some_and(|status| {
                                        status.running && status.agent_name.is_some()
                                    });
                                let cwd = tab.panes.leaf(pane_id).and_then(|leaf| leaf.saved_cwd());
                                (
                                    detected_agent_running,
                                    saved_agent_session(status, cwd.as_deref(), None),
                                )
                            },
                        );
                        let cwd = tab
                            .panes
                            .leaf(tab.focused_pane_id)
                            .or_else(|| tab.panes.leaves().into_iter().next())
                            .and_then(|leaf| leaf.saved_cwd());
                        (pane_tree, cwd)
                    };
                    session::SavedTab {
                        name: tab.name.clone(),
                        work_origin: Some(tab.work_origin.clone()),
                        cwd,
                        panes: Some(pane_tree),
                        discovery_cwd: normalize_persisted_path(tab.discovery_cwd.as_deref()),
                    }
                })
                .collect();

            let active_tab_index = ws
                .tabs
                .iter()
                .filter(|t| t.kind == workspace::TabKind::Terminal)
                .position(|t| t.id == ws.active_tab)
                .unwrap_or(0);

            session::SavedWorkspace {
                id: ws.id,
                work_origin: Some(ws.work_origin.clone()),
                name: ws.name.clone(),
                collapsed: ws.collapsed,
                repo_root: normalize_persisted_path(ws.repo_root.as_deref()),
                is_worktree: ws.is_worktree,
                working_tree_path: normalize_persisted_path(ws.working_tree_path.as_deref()),
                branch_name: ws.branch_name.clone(),
                linked_issue: ws.linked_issue.clone(),
                tabs,
                active_tab_index,
                tmux_backed: ws.tmux_backed,
                host_config_name: ws.host_config_name.clone(),
            }
        })
        .collect();

    let active_workspace_index = st
        .workspaces
        .iter()
        .position(|w| w.id == st.active_workspace)
        .unwrap_or(0);

    session::SessionStateV2 {
        version: 2,
        session_namespace: crate::instance::session_storage_key(),
        session_identity: crate::instance::session_name(),
        session_name,
        workspaces,
        active_workspace_index,
        window_width,
        window_height,
        detached_sessions: st
            .detached_sessions
            .iter()
            .map(|ds| ds.to_saved())
            .collect(),
        background_section_collapsed: Some(st.background_section_collapsed),
    }
}

/// Capture the live GTK state briefly, leaving provider-history lookup and
/// serialization to the ordered session writer after the `AppState` borrow is
/// released.
#[allow(deprecated)] // VTE 0.78 deprecated current_directory_uri; termprop migration is future scope
fn build_session_capture_v2(
    st: &AppState,
    session_name: Option<String>,
    window_width: i32,
    window_height: i32,
) -> session::SessionCapture {
    let state = build_session_state_v2(st, session_name, window_width, window_height);
    let process_truth_fresh = crate::runtime_probe::runtime_process_truth_is_fresh(st);
    let mut agent_lookups = Vec::new();
    for workspace in &st.workspaces {
        for tab in workspace
            .tabs
            .iter()
            .filter(|tab| tab.kind == workspace::TabKind::Terminal)
        {
            for leaf in tab.panes.leaves() {
                if leaf.tmux_backing.is_some() {
                    continue;
                }
                let Some(status) = process_truth_fresh
                    .then(|| {
                        st.runtime_probe
                            .as_ref()
                            .and_then(|snapshot| snapshot.pane_agents.get(&(tab.id, leaf.pane_id)))
                            .filter(|status| {
                                status.running
                                    && status.agent_name.is_some()
                                    && status.session_id.is_none()
                            })
                    })
                    .flatten()
                else {
                    continue;
                };
                let (Some(cwd), Some(agent_name)) =
                    (leaf.saved_cwd(), status.agent_name.as_deref())
                else {
                    continue;
                };
                agent_lookups.push(session::AgentSessionLookup::new(
                    workspace.id,
                    tab.work_origin.clone(),
                    leaf.work_origin.clone(),
                    crate::agent_sessions::normalize_agent_name(agent_name),
                    cwd,
                ));
            }
        }
    }
    // The catalog captures home-directory-derived roots on GTK; the writer
    // receives that ready catalog and performs only provider filesystem scans.
    session::SessionCapture::with_agent_lookups(
        state,
        agent_lookups,
        crate::agent_sessions::default_catalog(),
    )
}

fn saved_agent_session(
    status: Option<&crate::agents::AgentStatus>,
    cwd: Option<&str>,
    discovery: Option<&crate::agent_sessions::AgentSessionDiscovery>,
) -> Option<session::SavedAgentSession> {
    let status = status.filter(|status| status.running)?;
    let agent_name = status.agent_name.as_deref()?;
    let normalized_agent = crate::agent_sessions::normalize_agent_name(agent_name);
    if let Some(session_id) = status.session_id.as_ref() {
        return Some(session::SavedAgentSession {
            agent_name: normalized_agent,
            session_id: session_id.clone(),
            source: session::SavedAgentSessionSource::Argv,
        });
    }

    let record =
        crate::agent_sessions::most_recent_discovered_session(discovery?, &normalized_agent, cwd?)?;
    Some(session::SavedAgentSession {
        agent_name: normalized_agent,
        session_id: record.session_id.clone(),
        source: session::SavedAgentSessionSource::TranscriptRecency,
    })
}

/// Capture session state without doing serialization, provider-history work,
/// filesystem I/O, or subprocess work on the GTK main context.
fn capture_session(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
) -> session::SessionCapture {
    let session_name = sidebar::session_label(tab_list).map(|label| label.text().to_string());
    let window_width = window.width();
    let window_height = window.height();
    let st = state.borrow();
    build_session_capture_v2(&st, session_name, window_width, window_height)
}

/// Capture and coalesce an autosave without blocking GTK on provider-history
/// discovery, serialization, or filesystem I/O.
pub(crate) fn save_session_async(
    writer: &session::SessionWriter,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
) {
    if let Some(error) = writer.take_unreported_error() {
        crate::show_error_toast(&format!("Session autosave failed: {error}"));
    }
    writer.schedule_autosave(capture_session(state, tab_list, window));
}

pub(crate) fn save_session_once(
    saved: &Cell<bool>,
    writer: &session::SessionWriter,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
) {
    if saved.replace(true) {
        return;
    }
    if let Err(error) = writer.shutdown(capture_session(state, tab_list, window)) {
        let message = format!("Could not save final session state: {error}");
        eprintln!("taarof: {message}");
        crate::show_error_toast(&message);
    }
}

#[allow(clippy::too_many_arguments)] // Shutdown wiring intentionally passes the live GTK handles explicitly from one setup site.
pub(crate) fn install_signal_cleanup(
    app: &adw::Application,
    writer: &Rc<session::SessionWriter>,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    session_saved: &Rc<Cell<bool>>,
    socket_cleaned: &Rc<Cell<bool>>,
    socket_path: Option<&Path>,
    auto_save_source: &Rc<RefCell<Option<glib::SourceId>>>,
) {
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    for signum in [libc::SIGINT, libc::SIGTERM] {
        if let Err(e) = signal_hook::flag::register(signum, shutdown_requested.clone()) {
            eprintln!("taarof: failed to register signal handler for {signum}: {e}");
        }
    }

    let app = app.clone();
    let writer = writer.clone();
    let state = state.clone();
    let tab_list = tab_list.clone();
    let window = window.clone();
    let session_saved = session_saved.clone();
    let socket_cleaned = socket_cleaned.clone();
    let socket_path = socket_path.map(Path::to_path_buf);
    let auto_save_source = auto_save_source.clone();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if shutdown_requested.swap(false, Ordering::SeqCst) {
            // Cancel the periodic auto-save so it cannot race with the final
            // shutdown save below.
            if let Some(source_id) = auto_save_source.borrow_mut().take() {
                source_id.remove();
            }
            save_session_once(&session_saved, &writer, &state, &tab_list, &window);
            crate::flush_history(&state);
            crate::cleanup_socket_once(&socket_cleaned, socket_path.as_deref());
            app.quit();
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

#[cfg(test)]
mod tests {
    use super::{build_session_state_v2, saved_agent_session};
    use crate::session::{
        SavedAgentSession, SavedAgentSessionSource, SavedPaneNode, SessionStateV2,
    };
    use crate::tracking::TrackingData;
    use crate::{seed_headless_terminal_tab, AppState, HeadlessPaneSeed};

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("taarof-{label}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("temp dir should be creatable");
        dir
    }

    fn sample_tracking() -> TrackingData {
        let root = temp_dir("kmux60-tracking");
        std::fs::create_dir_all(root.join(".plan")).expect(".plan dir should be creatable");
        std::fs::write(
            root.join(".plan/features.json"),
            r#"{"features":[
                {"id":"F1","title":"one","status":"done","effort":"medium"},
                {"id":"F2","title":"two","status":"queued","effort":"large"}
            ]}"#,
        )
        .expect("features.json should be writable");
        let data = TrackingData::load(&root).expect("tracking should load");
        let _ = std::fs::remove_dir_all(&root);
        data
    }

    fn discovered_session(
        agent: &str,
        session_id: &str,
        cwd: &str,
        updated_at_unix_ms: u64,
    ) -> crate::agent_sessions::AgentSessionRecord {
        crate::agent_sessions::AgentSessionRecord {
            agent: agent.into(),
            session_id: session_id.into(),
            title: "session".into(),
            cwd: cwd.into(),
            host: None,
            repo_root: None,
            started_at_unix_ms: None,
            updated_at_unix_ms,
            last_user_message_at_unix_ms: None,
            status: "recent".into(),
            live_binding: None,
            resume_command: None,
            resume_unavailable_reason: None,
        }
    }

    #[test]
    fn capture_prefers_argv_then_falls_back_to_cwd_recency() {
        let discovery = crate::agent_sessions::AgentSessionDiscovery {
            providers: Vec::new(),
            sessions: vec![
                discovered_session("codex", "older", "/repo", 10),
                discovered_session("codex", "newer", "/repo", 20),
            ],
            remote_hosts: Vec::new(),
        };
        let argv_status = crate::agents::AgentStatus {
            agent_name: Some("codex-cli".into()),
            session_id: Some("argv-session".into()),
            running: true,
        };
        assert_eq!(
            saved_agent_session(Some(&argv_status), Some("/repo"), None),
            Some(SavedAgentSession {
                agent_name: "codex".into(),
                session_id: "argv-session".into(),
                source: SavedAgentSessionSource::Argv,
            })
        );

        let fallback_status = crate::agents::AgentStatus {
            session_id: None,
            ..argv_status
        };
        assert_eq!(
            saved_agent_session(Some(&fallback_status), Some("/repo"), Some(&discovery)),
            Some(SavedAgentSession {
                agent_name: "codex".into(),
                session_id: "newer".into(),
                source: SavedAgentSessionSource::TranscriptRecency,
            })
        );
        assert_eq!(saved_agent_session(None, Some("/repo"), None), None);
    }

    #[test]
    fn session_snapshot_wires_runtime_probe_agent_metadata_to_saved_leaf() {
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "agent",
            HeadlessPaneSeed::default(),
        )
        .expect("headless agent pane should seed");

        let without_probe = build_session_state_v2(&state, None, 1200, 800);
        assert!(matches!(
            without_probe.workspaces[0].tabs[0].panes.as_ref(),
            Some(SavedPaneNode::Leaf {
                agent_session: None,
                ..
            })
        ));

        state.runtime_probe = Some(crate::runtime_probe::RuntimeProbeSnapshot {
            probed_at_unix_ms: 1,
            process_observed_at_unix_ms: Some(1),
            ports_observed_at_unix_ms: Some(1),
            tab_pids: std::collections::BTreeMap::new(),
            pane_pids: std::collections::BTreeMap::new(),
            pane_process_states: std::collections::HashMap::new(),
            pane_exact_agents: std::collections::HashMap::new(),
            pane_agents: std::collections::HashMap::from([(
                (tab_id, pane_id),
                crate::agents::AgentStatus {
                    agent_name: Some("codex-cli".into()),
                    session_id: Some("argv-session".into()),
                    running: true,
                },
            )]),
            tab_agents: std::collections::HashMap::new(),
            tab_ports: std::collections::HashMap::new(),
            process_probe: crate::probe::ProbeState::Ok,
            ports_probe: crate::probe::ProbeState::Ok,
            process_error: None,
            ports_error: None,
        });

        let with_probe = build_session_state_v2(&state, None, 1200, 800);
        match with_probe.workspaces[0].tabs[0]
            .panes
            .as_ref()
            .expect("saved pane tree")
        {
            SavedPaneNode::Leaf { agent_session, .. } => assert_eq!(
                agent_session,
                &Some(SavedAgentSession {
                    agent_name: "codex".into(),
                    session_id: "argv-session".into(),
                    source: SavedAgentSessionSource::Argv,
                })
            ),
            other => panic!("expected saved leaf, got {other:?}"),
        }
    }

    /// Build a two-workspace fixture: workspace A holds a realistic terminal tab
    /// (headless tmux pane with cwd, plus agent / discovery / tracking metadata),
    /// workspace B is the move target. Returns `(state, ws_a, ws_b, moved_tab)`.
    fn two_workspace_fixture() -> (AppState, u32, u32, u32) {
        let mut state = AppState::new();
        let ws_a = state.workspaces[0].id;
        let ws_b = state.create_workspace("target", None);

        let (moved_tab, _pane) = seed_headless_terminal_tab(
            &mut state,
            ws_a,
            "runner",
            HeadlessPaneSeed {
                cwd: Some("/tmp/user/project".into()),
                cwd_host: None,
                shell_running: true,
                has_child_process: true,
                remote_shell: false,
                ssh_command: None,
                tmux_session: Some("kmux60".into()),
                tmux_ssh_target: None,
            },
        )
        .expect("headless tab should seed");

        // Give the target workspace a tab of its own so the move is a genuine
        // relocation between populated workspaces rather than into an empty one.
        seed_headless_terminal_tab(
            &mut state,
            ws_b,
            "target-shell",
            HeadlessPaneSeed {
                cwd: Some("/tmp/user/other".into()),
                cwd_host: None,
                shell_running: true,
                has_child_process: false,
                remote_shell: false,
                ssh_command: None,
                tmux_session: None,
                tmux_ssh_target: None,
            },
        )
        .expect("target tab should seed");

        // Decorate the tab that will move with non-trivial metadata.
        {
            let tab = state.workspaces[0]
                .tabs
                .iter_mut()
                .find(|t| t.id == moved_tab)
                .expect("moved tab should exist in source workspace");
            tab.agent_name = Some("claude".into());
            tab.agent_session_id = Some("sess-123".into());
            tab.agent_pane_id = Some(tab.focused_pane_id);
            tab.discovery_cwd = Some("/tmp/user/project".into());
            tab.tracking_data = Some(sample_tracking());
        }

        (state, ws_a, ws_b, moved_tab)
    }

    #[test]
    fn move_tab_workspace_state_survives_session_restore() {
        let (mut state, ws_a, ws_b, moved_tab) = two_workspace_fixture();
        let tracking_before = state.workspaces[0]
            .tabs
            .iter()
            .find(|t| t.id == moved_tab)
            .and_then(|t| t.tracking_data.clone())
            .expect("tracking should be present before move");

        state
            .move_tab_to_workspace(moved_tab, ws_b)
            .expect("move should succeed");

        // The Tab is relocated intact: metadata still lives on the same struct,
        // now under the target workspace.
        let target_ws = state
            .workspaces
            .iter()
            .find(|ws| ws.id == ws_b)
            .expect("target workspace should exist");
        let relocated = target_ws
            .tabs
            .iter()
            .find(|t| t.id == moved_tab)
            .expect("moved tab should be under target workspace");
        assert_eq!(relocated.agent_name.as_deref(), Some("claude"));
        assert_eq!(relocated.agent_session_id.as_deref(), Some("sess-123"));
        assert_eq!(
            relocated.discovery_cwd.as_deref(),
            Some("/tmp/user/project")
        );
        assert_eq!(relocated.tracking_data.as_ref(), Some(&tracking_before));
        assert_eq!(target_ws.active_tab, moved_tab);
        // The source workspace no longer owns it.
        let source_ws = state
            .workspaces
            .iter()
            .find(|ws| ws.id == ws_a)
            .expect("source workspace should exist");
        assert!(source_ws.tabs.iter().all(|t| t.id != moved_tab));

        // Derive the persisted payload from the moved AppState and round-trip it
        // through serde exactly as save_v2 -> load_v2 would on disk.
        let payload = build_session_state_v2(&state, Some("kmux60".into()), 1280, 800);
        let json = serde_json::to_string_pretty(&payload).expect("payload should serialize");
        let restored: SessionStateV2 =
            serde_json::from_str(&json).expect("payload should deserialize");

        let restored_target = restored
            .workspaces
            .iter()
            .find(|ws| ws.id == ws_b)
            .expect("target workspace should persist");
        let restored_source = restored
            .workspaces
            .iter()
            .find(|ws| ws.id == ws_a)
            .expect("source workspace should persist");

        // The moved tab is persisted under the target workspace only.
        assert!(restored_target.tabs.iter().any(|t| t.name == "runner"));
        assert!(restored_source.tabs.iter().all(|t| t.name != "runner"));

        // Active-tab selection points at the relocated tab within the target.
        let restored_tab = restored_target
            .tabs
            .iter()
            .find(|t| t.name == "runner")
            .expect("runner tab should persist under target");
        let active_index = restored_target
            .tabs
            .iter()
            .position(|t| t.name == "runner")
            .expect("runner tab index");
        assert_eq!(restored_target.active_tab_index, active_index);

        // Pane tree, cwd hint, and discovery cwd survive persistence.
        assert_eq!(
            restored_tab.discovery_cwd.as_deref(),
            Some("/tmp/user/project")
        );
        // The pane tree relocates as a single leaf (headless Stub panes serialize
        // to a bare leaf; the point is the tree survives and stays attached to
        // the moved tab under the target workspace).
        assert!(matches!(
            restored_tab
                .panes
                .as_ref()
                .expect("moved tab should persist a pane tree"),
            SavedPaneNode::Leaf { .. }
        ));

        // Active workspace followed the move.
        let active_ws_id = restored.workspaces[restored.active_workspace_index].id;
        assert_eq!(active_ws_id, ws_b);
    }

    #[test]
    fn pane_task_binding_survives_session_snapshot() {
        let root = temp_dir("pane-task-session");
        std::fs::create_dir_all(root.join(".git")).expect(".git dir should be creatable");
        std::fs::create_dir_all(root.join(".plan")).expect(".plan dir should be creatable");
        std::fs::write(
            root.join(".plan/tasks.json"),
            r#"{"tasks":[{"id":"EXAMPLE-110","title":"Bind panes","status":"todo"}]}"#,
        )
        .expect("tasks.json should be writable");

        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (_tab_id, pane_id) = seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "runner",
            HeadlessPaneSeed {
                cwd: Some(root.to_string_lossy().into_owned()),
                ..HeadlessPaneSeed::default()
            },
        )
        .expect("headless tab should seed");
        let tab_id = state.active_tab().expect("active tab should exist").id;

        state
            .bind_pane_to_task(tab_id, pane_id, "EXAMPLE-110")
            .expect("task should bind");

        let payload = build_session_state_v2(&state, Some("tasks".into()), 1280, 800);
        let saved_tab = &payload.workspaces[0].tabs[0];
        assert_eq!(
            payload.workspaces[0].work_origin.as_deref(),
            Some(state.workspaces[0].work_origin.as_str())
        );
        assert_eq!(
            saved_tab.work_origin.as_deref(),
            Some(state.find_tab(tab_id).unwrap().1.work_origin.as_str())
        );
        match saved_tab.panes.as_ref().expect("pane tree should save") {
            SavedPaneNode::Leaf { current_task, .. } => {
                let current_task = current_task
                    .as_ref()
                    .expect("current task should be saved on the pane");
                assert_eq!(current_task.task_id, "EXAMPLE-110");
                assert_eq!(current_task.title, "Bind panes");
            }
            _ => panic!("expected saved leaf"),
        }
    }
}

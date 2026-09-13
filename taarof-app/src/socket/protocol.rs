//! Socket wire protocol: message enum, response type, and dispatch helpers.

use super::*;

pub(super) const SOCKET_MAX_CAPTURE_SCROLLBACK_LINES: u32 = 10_000;

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum SocketActivityState {
    Idle,
    Running,
    #[serde(rename = "waiting-input", alias = "waiting", alias = "needs-input")]
    WaitingInput,
    #[serde(rename = "errored", alias = "error")]
    Errored,
    Done,
}

/// JSON protocol for external notifications and agent activity updates.
/// Usage: echo '{"action":"notify","tab":"Shell 1","message":"Build done"}' | socat - UNIX-CONNECT:$TAAROF_SOCK
///
/// Trust model: this Unix socket is a privileged same-user local control
/// surface. Any local process that can connect can create tabs, run shell
/// commands, inject raw keys, and capture pane text. Runtime-directory
/// ownership and permissions are the access control boundary.
#[derive(Deserialize, Debug)]
#[serde(tag = "action")]
pub(super) enum SocketMessage {
    #[serde(rename = "notify")]
    Notify {
        /// Tab name or numeric ID. If omitted, notifies the first non-active tab.
        tab: Option<String>,
        /// Notification body text.
        message: Option<String>,
    },
    #[serde(rename = "agent-status")]
    AgentStatus {
        /// Tab name, numeric ID, or "current". If omitted, targets the active tab.
        tab: Option<String>,
        /// Stable pane ID within the resolved tab. Required for multi-pane tabs.
        pane: Option<u32>,
        state: SocketActivityState,
        text: Option<String>,
        source: Option<String>,
    },
    /// Submit one prompt to an exact pane and establish an event boundary.
    /// The prompt is never copied into events, diagnostics, or turn state.
    #[serde(rename = "prompt-agent")]
    PromptAgent {
        tab: String,
        pane: u32,
        prompt: String,
    },
    /// Wait for post-boundary agent evidence for a token returned by
    /// `prompt-agent`, then capture bounded logical pane output.
    #[serde(rename = "wait-agent-turn")]
    WaitAgentTurn {
        turn_token: String,
        #[serde(default)]
        timeout_seconds: Option<f64>,
        #[serde(default)]
        scrollback: Option<u32>,
        #[serde(default)]
        max_output_bytes: Option<usize>,
    },
    /// Cancel an active or not-yet-waited turn token.
    #[serde(rename = "cancel-agent-turn")]
    CancelAgentTurn { turn_token: String },
    /// Bootstrap explicit stable reporting context for a manually-bound pane.
    #[serde(rename = "work-context")]
    WorkContext { tab: Option<String>, pane: u32 },
    /// Append an observational milestone to the session work ledger.
    #[serde(rename = "work-report")]
    WorkReport {
        milestone: crate::work_reporting::WorkMilestone,
        note: Option<String>,
        session: String,
        workspace_origin: String,
        tab_origin: String,
        pane_origin: String,
        task_id: String,
        checkout_root: String,
        binding_token: String,
    },
    #[serde(rename = "open-pane")]
    OpenPane {
        tab: Option<String>,
        command: String,
        /// "vertical" | "horizontal" (default handled by caller)
        direction: Option<String>,
        working_dir: Option<String>,
    },
    #[serde(rename = "run-in-pane")]
    RunInPane {
        tab: Option<String>,
        pane: u32,
        command: String,
    },
    #[serde(rename = "close-pane")]
    ClosePane { tab: Option<String>, pane: u32 },
    // ── F010: Full scripting API ──
    #[serde(rename = "create-tab")]
    CreateTab {
        name: Option<String>,
        working_dir: Option<String>,
        command: Option<String>,
    },
    #[serde(rename = "close-tab")]
    CloseTab { tab: String },
    #[serde(rename = "switch-tab")]
    SwitchTab { tab: String },
    #[serde(rename = "rename-tab")]
    RenameTab { tab: String, name: String },
    #[serde(rename = "reorder-tab")]
    ReorderTab { tab: String, index: usize },
    #[serde(rename = "create-workspace")]
    CreateWorkspace { name: Option<String> },
    #[serde(rename = "switch-workspace")]
    SwitchWorkspace { workspace: String },
    #[serde(rename = "rename-workspace")]
    RenameWorkspace { workspace: String, name: String },
    #[serde(rename = "list-tabs")]
    ListTabs,
    #[serde(rename = "send-keys")]
    SendKeys {
        tab: Option<String>,
        pane: u32,
        keys: String,
    },
    #[serde(rename = "resize-pane")]
    ResizePane {
        tab: Option<String>,
        pane: u32,
        cols: u32,
        rows: u32,
    },
    #[serde(rename = "split-pane")]
    SplitPane {
        tab: Option<String>,
        direction: Option<String>,
        command: Option<String>,
        working_dir: Option<String>,
        #[serde(default)]
        idempotency_key: Option<String>,
    },
    #[serde(rename = "get-text")]
    GetText {
        tab: Option<String>,
        pane: u32,
        scrollback: Option<u32>,
        /// Rejoin soft-wrapped rows into logical lines (default on). Set to
        /// `false` for one-line-per-grid-row raw capture.
        logical: Option<bool>,
    },
    #[serde(rename = "create-tmux-tab")]
    CreateTmuxTab {
        name: Option<String>,
        host: Option<String>,
        working_dir: Option<String>,
    },
    #[serde(rename = "open-dashboard")]
    OpenDashboard,
    #[serde(rename = "detach-pane")]
    DetachPane {
        #[serde(default)]
        tab: Option<String>,
        pane: u32,
    },
    #[serde(rename = "attach-session")]
    AttachSession {
        session_name: String,
        #[serde(default)]
        expected_agent: Option<agent_session_core::AttachTarget>,
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        ssh_target: Option<String>,
    },
    #[serde(rename = "list-detached")]
    ListDetached,
    #[serde(rename = "dashboard-state")]
    DashboardState,
    #[serde(rename = "query-state")]
    #[serde(alias = "workspace-state")]
    QueryState,
    #[serde(rename = "query-events")]
    QueryEvents {
        #[serde(default)]
        since_seq: Option<u64>,
        #[serde(default)]
        limit: Option<usize>,
    },
    #[serde(rename = "query-history")]
    QueryHistory {
        #[serde(default)]
        since_id: Option<u64>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(flatten)]
        filters: crate::history::HistoryFilters,
    },
    #[serde(rename = "query-agent-sessions")]
    QueryAgentSessions {
        #[serde(default)]
        schema: crate::agent_sessions::SessionSchema,
    },
    #[serde(rename = "agent-workspace")]
    AgentWorkspace {
        branch: String,
        command: Option<String>,
        repo: Option<String>,
        /// Lifetime of the synchronous response contract. Once it expires,
        /// the caller is removed before GTK apply; a newly-created worktree
        /// is rolled back instead of being opened for an abandoned request.
        #[serde(default)]
        timeout_seconds: Option<f64>,
    },
    // ── Remote-device pairing confirmation channel ──
    /// Ask the gateway to mint a five-minute, one-use pairing offer.
    #[serde(rename = "pairing-offer")]
    PairingOffer,
    /// List devices awaiting the operator's confirmation.
    #[serde(rename = "pairing-pending")]
    PairingPending,
    /// Confirm the pending device on an offer.
    #[serde(rename = "pairing-confirm")]
    PairingConfirm { offer_id: String },
    /// Reject the pending device on an offer.
    #[serde(rename = "pairing-reject")]
    PairingReject { offer_id: String },
    /// Revoke a paired device.
    #[serde(rename = "device-revoke")]
    DeviceRevoke { device_id: String },
}

#[derive(Clone, Serialize)]
pub(super) struct SocketResponse {
    pub(super) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) workspace_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pane_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tab_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) data: Option<serde_json::Value>,
}

impl SocketResponse {
    pub(super) fn ok() -> Self {
        Self {
            ok: true,
            workspace_id: None,
            pane_id: None,
            tab_id: None,
            error: None,
            data: None,
        }
    }

    pub(super) fn ok_with_pane(pane_id: u32) -> Self {
        Self {
            ok: true,
            workspace_id: None,
            pane_id: Some(pane_id),
            tab_id: None,
            error: None,
            data: None,
        }
    }

    pub(super) fn ok_with_tab(tab_id: u32) -> Self {
        Self {
            ok: true,
            workspace_id: None,
            pane_id: None,
            tab_id: Some(tab_id),
            error: None,
            data: None,
        }
    }

    pub(super) fn ok_with_data(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            workspace_id: None,
            pane_id: None,
            tab_id: None,
            error: None,
            data: Some(data),
        }
    }

    pub(super) fn ok_with_target(workspace_id: u32, tab_id: u32, pane_id: u32) -> Self {
        Self {
            ok: true,
            workspace_id: Some(workspace_id),
            pane_id: Some(pane_id),
            tab_id: Some(tab_id),
            error: None,
            data: None,
        }
    }

    pub(super) fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            workspace_id: None,
            pane_id: None,
            tab_id: None,
            error: Some(msg.into()),
            data: None,
        }
    }
}

pub(super) fn bridge_queue_error(
    error: tokio_mpsc::error::TrySendError<(SocketMessage, mpsc::Sender<SocketResponse>)>,
) -> SocketResponse {
    match error {
        tokio_mpsc::error::TrySendError::Full(_) => {
            SocketResponse::err("runtime request queue is full; retry later")
        }
        tokio_mpsc::error::TrySendError::Closed(_) => {
            SocketResponse::err("runtime request queue is closed")
        }
    }
}

pub(super) fn socket_message_is_read_only(msg: &SocketMessage) -> bool {
    matches!(
        msg,
        SocketMessage::ListTabs
            | SocketMessage::WorkContext { .. }
            | SocketMessage::GetText { .. }
            | SocketMessage::ListDetached
            | SocketMessage::DashboardState
            | SocketMessage::QueryState
            | SocketMessage::QueryEvents { .. }
            | SocketMessage::QueryHistory { .. }
            | SocketMessage::QueryAgentSessions { .. }
            | SocketMessage::PairingPending
    )
}

pub(super) fn socket_message_action(msg: &SocketMessage) -> &'static str {
    match msg {
        SocketMessage::Notify { .. } => "notify",
        SocketMessage::AgentStatus { .. } => "agent-status",
        SocketMessage::PromptAgent { .. } => "prompt-agent",
        SocketMessage::WaitAgentTurn { .. } => "wait-agent-turn",
        SocketMessage::CancelAgentTurn { .. } => "cancel-agent-turn",
        SocketMessage::WorkContext { .. } => "work-context",
        SocketMessage::WorkReport { .. } => "work-report",
        SocketMessage::OpenPane { .. } => "open-pane",
        SocketMessage::RunInPane { .. } => "run-in-pane",
        SocketMessage::ClosePane { .. } => "close-pane",
        SocketMessage::CreateTab { .. } => "create-tab",
        SocketMessage::CloseTab { .. } => "close-tab",
        SocketMessage::SwitchTab { .. } => "switch-tab",
        SocketMessage::RenameTab { .. } => "rename-tab",
        SocketMessage::ReorderTab { .. } => "reorder-tab",
        SocketMessage::CreateWorkspace { .. } => "create-workspace",
        SocketMessage::SwitchWorkspace { .. } => "switch-workspace",
        SocketMessage::RenameWorkspace { .. } => "rename-workspace",
        SocketMessage::ListTabs => "list-tabs",
        SocketMessage::SendKeys { .. } => "send-keys",
        SocketMessage::ResizePane { .. } => "resize-pane",
        SocketMessage::SplitPane { .. } => "split-pane",
        SocketMessage::GetText { .. } => "get-text",
        SocketMessage::CreateTmuxTab { .. } => "create-tmux-tab",
        SocketMessage::OpenDashboard => "open-dashboard",
        SocketMessage::DetachPane { .. } => "detach-pane",
        SocketMessage::AttachSession { .. } => "attach-session",
        SocketMessage::ListDetached => "list-detached",
        SocketMessage::DashboardState => "dashboard-state",
        SocketMessage::QueryState => "query-state",
        SocketMessage::QueryEvents { .. } => "query-events",
        SocketMessage::QueryHistory { .. } => "query-history",
        SocketMessage::QueryAgentSessions { .. } => "query-agent-sessions",
        SocketMessage::AgentWorkspace { .. } => "agent-workspace",
        SocketMessage::PairingOffer => "pairing-offer",
        SocketMessage::PairingPending => "pairing-pending",
        SocketMessage::PairingConfirm { .. } => "pairing-confirm",
        SocketMessage::PairingReject { .. } => "pairing-reject",
        SocketMessage::DeviceRevoke { .. } => "device-revoke",
    }
}

#[cfg(test)]
mod agent_session_schema_tests {
    use super::*;
    use crate::agent_sessions::{snapshot_value, AgentSessionsSnapshot, SessionSchema};
    #[test]
    fn socket_agent_sessions_schema_defaults_and_v2_projection() {
        for (request, schema) in [
            (r#"{"action":"query-agent-sessions"}"#, SessionSchema::V1),
            (
                r#"{"action":"query-agent-sessions","schema":"agent.sessions.v2"}"#,
                SessionSchema::V2,
            ),
        ] {
            let message: SocketMessage = serde_json::from_str(request).unwrap();
            assert!(
                matches!(message, SocketMessage::QueryAgentSessions { schema: actual } if actual == schema)
            );
            let value = snapshot_value(
                AgentSessionsSnapshot {
                    schema: "taarof.agent-sessions.v1",
                    generated_at_unix_ms: 42,
                    providers: vec![],
                    sessions: vec![],
                    remote_hosts: vec![],
                },
                schema,
            )
            .unwrap();
            assert_eq!(
                value["schema"],
                if schema == SessionSchema::V1 {
                    "taarof.agent-sessions.v1"
                } else {
                    "agent.sessions.v2"
                }
            );
            assert_eq!(value["generated_at_unix_ms"], 42);
        }
        assert!(serde_json::from_str::<SocketMessage>(
            r#"{"action":"query-agent-sessions","schema":"unknown"}"#
        )
        .is_err());
    }
}

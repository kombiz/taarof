use std::cell::RefCell;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{mpsc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use http_body_util::{BodyExt, Empty};
use hyper::Request;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use serde_json::Value;
use taarof_app::{
    config::{HttpConfig, TmuxCloseBehavior},
    http, plan_restored_spawns, seed_headless_terminal_tab, seed_pending_restore_tab,
    session::{SavedPaneNode, SavedTab, SavedTmuxIdentity, SavedWorkspace, SessionStateV2},
    snapshot_session_state_for_test, socket, AppState, HeadlessPaneSeed, PlannedRestoreSpawn,
};
use tokio_tungstenite::connect_async;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct ScopedEnv {
    _guard: MutexGuard<'static, ()>,
    previous: Vec<(&'static str, Option<OsString>)>,
}

impl ScopedEnv {
    fn set(pairs: &[(&'static str, OsString)]) -> Self {
        let guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let mut previous = Vec::with_capacity(pairs.len());
        for (key, value) in pairs {
            previous.push((*key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        Self {
            _guard: guard,
            previous,
        }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, value) in self.previous.iter().rev() {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn unique_temp_dir(_label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after unix epoch")
        .as_nanos();
    // Keep runtime fixture paths comfortably below Linux sockaddr_un's
    // 107-byte payload limit even when named-session sockets include their
    // fixed 128-bit digest. The nanosecond suffix still makes each directory
    // unique; descriptive labels are intentionally not embedded in the path.
    let dir = std::env::temp_dir().join(format!("trs-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir should be created");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .expect("temp dir permissions should be private");
    dir
}

fn free_loopback_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("ephemeral port bind should work")
        .local_addr()
        .expect("listener should have local addr")
        .port()
}

/// Start an HTTP server on a fresh ephemeral loopback port, retrying when the
/// chosen port is claimed by a sibling test between selection and bind.
///
/// `free_loopback_port` releases its probe listener before the server binds, so
/// under parallel execution another test can grab the same ephemeral port in
/// that window and the bind fails with "Address already in use". Re-probing on
/// failure keeps these smoke tests deterministic; the bound port is returned
/// alongside the server handle so callers can address the running server.
fn start_http_server_on_free_loopback_port<T>(
    mut start: impl FnMut(&HttpConfig) -> Option<T>,
) -> (u16, T) {
    for _ in 0..64 {
        let port = free_loopback_port();
        let config = HttpConfig {
            enabled: true,
            port,
            bind_address: "127.0.0.1".to_string(),
            unsafe_allow_non_loopback: false,
        };
        if let Some(handle) = start(&config) {
            return (port, handle);
        }
    }
    panic!("HTTP server failed to bind a free loopback port after 64 attempts");
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(condition(), "condition was not met within {:?}", timeout);
}

fn connect_unix(path: &Path) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return stream,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("failed to connect to {}: {error}", path.display()),
        }
    }
}

fn socket_request_json(socket_path: &Path, request: Value) -> Value {
    let (response_sender, response_receiver) = mpsc::channel();
    std::thread::spawn({
        let socket_path = socket_path.to_path_buf();
        move || {
            let mut stream = connect_unix(&socket_path);
            stream
                .write_all(
                    serde_json::to_string(&request)
                        .expect("socket request should serialize")
                        .as_bytes(),
                )
                .expect("socket request should be written");
            stream
                .shutdown(Shutdown::Write)
                .expect("socket write half should close");
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .expect("socket response should be readable");
            response_sender
                .send(response)
                .expect("socket response should be forwarded");
        }
    });

    let main_context = glib::MainContext::default();
    let response = {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(response) = response_receiver.try_recv() {
                break response;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for socket response"
            );
            while main_context.pending() {
                main_context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    serde_json::from_str(response.trim()).expect("socket response should be json")
}

fn multi_workspace_restore_roundtrip(label: &str, fixture: Value) -> SessionStateV2 {
    let data_home = unique_temp_dir(label);
    let _env = ScopedEnv::set(&[
        ("XDG_DATA_HOME", data_home.clone().into_os_string()),
        ("TAAROF_SESSION", OsString::from(label)),
    ]);
    let state_dir = data_home.join("taarof");
    std::fs::create_dir_all(&state_dir).expect("session state dir should exist");
    std::fs::write(
        state_dir.join(format!("session-{label}.json")),
        serde_json::to_string_pretty(&fixture).expect("fixture should serialize"),
    )
    .expect("fixture should be written");

    let loaded = taarof_app::session::load_v2().expect("fixture should load");
    taarof_app::session::save_v2(&loaded);
    taarof_app::session::load_v2().expect("round-tripped session should load")
}

fn http_get_json(addr: SocketAddr, path: &str, token: Option<&str>) -> (u16, Value) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should start");
    runtime.block_on(async move {
        let client: Client<HttpConnector, Empty<hyper::body::Bytes>> =
            Client::builder(TokioExecutor::new()).build_http();
        let mut request = Request::builder()
            .uri(format!("http://{addr}{path}"))
            .body(Empty::new())
            .expect("request should build");
        if let Some(token) = token {
            request.headers_mut().insert(
                hyper::header::AUTHORIZATION,
                format!("Bearer {token}")
                    .parse()
                    .expect("authorization header should parse"),
            );
        }

        let response = client
            .request(request)
            .await
            .expect("http request should succeed");
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("response body should collect")
            .to_bytes();
        let payload = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).expect("response body should be json")
        };
        (status, payload)
    })
}

fn http_state_projection(state_snapshot: &Value, projection: http::StateProjection) -> Value {
    match projection {
        http::StateProjection::Workspaces => state_snapshot["workspaces"].clone(),
        http::StateProjection::Tabs => {
            let mut tabs = Vec::new();
            if let Some(workspaces) = state_snapshot["workspaces"].as_array() {
                for workspace in workspaces {
                    let workspace_id = workspace["id"].clone();
                    let workspace_name = workspace["name"].clone();
                    if let Some(workspace_tabs) = workspace["tabs"].as_array() {
                        for tab in workspace_tabs {
                            let mut payload = tab.clone();
                            if let Some(object) = payload.as_object_mut() {
                                object.insert("workspace_id".into(), workspace_id.clone());
                                object.insert("workspace_name".into(), workspace_name.clone());
                            }
                            tabs.push(payload);
                        }
                    }
                }
            }
            Value::Array(tabs)
        }
        http::StateProjection::Panes => {
            let mut panes = Vec::new();
            if let Some(workspaces) = state_snapshot["workspaces"].as_array() {
                for workspace in workspaces {
                    let workspace_id = workspace["id"].clone();
                    if let Some(workspace_tabs) = workspace["tabs"].as_array() {
                        for tab in workspace_tabs {
                            let tab_id = tab["tab_id"].clone();
                            if let Some(tab_panes) = tab["panes"].as_array() {
                                for pane in tab_panes {
                                    let mut payload = pane.clone();
                                    if let Some(object) = payload.as_object_mut() {
                                        object.insert("workspace_id".into(), workspace_id.clone());
                                        object.insert("tab_id".into(), tab_id.clone());
                                    }
                                    panes.push(payload);
                                }
                            }
                        }
                    }
                }
            }
            Value::Array(panes)
        }
    }
}

fn http_get_text(addr: SocketAddr, path: &str, token: Option<&str>) -> (u16, String, String) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should start");
    runtime.block_on(async move {
        let client: Client<HttpConnector, Empty<hyper::body::Bytes>> =
            Client::builder(TokioExecutor::new()).build_http();
        let mut request = Request::builder()
            .uri(format!("http://{addr}{path}"))
            .body(Empty::new())
            .expect("request should build");
        if let Some(token) = token {
            request.headers_mut().insert(
                hyper::header::AUTHORIZATION,
                format!("Bearer {token}")
                    .parse()
                    .expect("authorization header should parse"),
            );
        }

        let response = client
            .request(request)
            .await
            .expect("http request should succeed");
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(hyper::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("response body should collect")
            .to_bytes();
        let text = String::from_utf8(body.to_vec()).expect("response body should be utf-8");
        (status, content_type, text)
    })
}

fn http_request_text(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, String, String) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should start");
    runtime.block_on(async move {
        let client: Client<HttpConnector, Empty<hyper::body::Bytes>> =
            Client::builder(TokioExecutor::new()).build_http();
        let mut builder = Request::builder()
            .method("GET")
            .uri(format!("http://{addr}{path}"))
            .version(hyper::Version::HTTP_11);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(Empty::new()).expect("request should build");

        let response = client
            .request(request)
            .await
            .expect("http request should succeed");
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(hyper::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("response body should collect")
            .to_bytes();
        let text = String::from_utf8(body.to_vec()).expect("response body should be utf-8");
        (status, content_type, text)
    })
}

#[test]
fn session_restore_roundtrips_through_disk_state() {
    let data_home = unique_temp_dir("session-data");
    let _env = ScopedEnv::set(&[
        ("XDG_DATA_HOME", data_home.clone().into_os_string()),
        ("TAAROF_SESSION", OsString::from("smoke-restore")),
    ]);

    let state = SessionStateV2 {
        version: 2,
        session_namespace: None,
        session_identity: None,
        session_name: Some("smoke-session".to_string()),
        workspaces: vec![SavedWorkspace {
            id: 7,
            work_origin: Some("workspace-runtime-smoke-feature".to_string()),
            name: "feature".to_string(),
            collapsed: true,
            repo_root: Some("/tmp/taarof".to_string()),
            is_worktree: true,
            working_tree_path: Some("/tmp/taarof-feature".to_string()),
            branch_name: Some("feature/a010".to_string()),
            linked_issue: Some("A010".to_string()),
            tabs: vec![SavedTab {
                name: "Shell".to_string(),
                work_origin: Some("tab-runtime-smoke-shell".to_string()),
                cwd: Some("/tmp/taarof-feature".to_string()),
                panes: Some(SavedPaneNode::Split {
                    direction: "vertical".to_string(),
                    ratio: 0.5,
                    first: Box::new(SavedPaneNode::Leaf {
                        work_origin: Some("pane-runtime-smoke-first".to_string()),
                        cwd: Some("/tmp/taarof-feature".to_string()),
                        ssh_command: None,
                        tmux_session: None,
                        tmux_host: None,
                        tmux_identity: None,
                        current_task: None,
                        agent_session: None,
                    }),
                    second: Box::new(SavedPaneNode::Leaf {
                        work_origin: Some("pane-runtime-smoke-second".to_string()),
                        cwd: Some("/tmp/taarof-feature/tests".to_string()),
                        ssh_command: Some(vec!["ssh".to_string(), "ci-box".to_string()]),
                        tmux_session: None,
                        tmux_host: None,
                        tmux_identity: None,
                        current_task: None,
                        agent_session: None,
                    }),
                }),
                discovery_cwd: Some("/tmp/taarof-feature".to_string()),
            }],
            active_tab_index: 0,
            tmux_backed: false,
            host_config_name: None,
        }],
        active_workspace_index: 0,
        window_width: 1280,
        window_height: 900,
        detached_sessions: Vec::new(),
        background_section_collapsed: Some(false),
    };

    taarof_app::session::save_v2(&state);
    let restored = taarof_app::session::load_v2().expect("saved session should reload");

    assert_eq!(restored.session_identity.as_deref(), Some("smoke-restore"));
    assert_eq!(restored.session_name.as_deref(), Some("smoke-session"));
    assert_eq!(restored.window_width, 1280);
    assert_eq!(restored.window_height, 900);
    assert_eq!(restored.background_section_collapsed, Some(false));
    assert_eq!(restored.workspaces.len(), 1);
    assert_eq!(restored.workspaces[0].name, "feature");
    assert_eq!(
        restored.workspaces[0].work_origin.as_deref(),
        Some("workspace-runtime-smoke-feature")
    );
    assert_eq!(restored.workspaces[0].tabs.len(), 1);
    assert_eq!(
        restored.workspaces[0].tabs[0].work_origin.as_deref(),
        Some("tab-runtime-smoke-shell")
    );
    assert_eq!(
        restored.workspaces[0].tabs[0].discovery_cwd.as_deref(),
        Some("/tmp/taarof-feature")
    );
    match restored.workspaces[0].tabs[0]
        .panes
        .as_ref()
        .expect("panes should be restored")
    {
        SavedPaneNode::Split {
            direction,
            first,
            second,
            ..
        } => {
            assert_eq!(direction, "vertical");
            assert!(matches!(first.as_ref(), SavedPaneNode::Leaf { .. }));
            match second.as_ref() {
                SavedPaneNode::Leaf { ssh_command, .. } => {
                    assert_eq!(
                        ssh_command.as_deref(),
                        Some(&["ssh".to_string(), "ci-box".to_string()][..])
                    );
                }
                _ => panic!("second pane should be a leaf"),
            }
        }
        _ => panic!("restored pane tree should remain a split"),
    }
}

#[test]
fn http_server_smoke_covers_health_and_state_surfaces() {
    let runtime_dir = unique_temp_dir("http-server");
    let state_payload = serde_json::json!({
        "schema": "taarof.state.v1",
        "session_name": "smoke-http",
        "active_workspace": 10,
        "active_tab": 101,
        "health": {
            "state": "degraded",
            "degraded": true,
            "degraded_components": 1,
        },
        "workspaces": [{
            "id": 10,
            "name": "default",
            "tabs": [
                {
                    "tab_id": 101,
                    "name": "editor",
                    "panes": [{
                        "pane_id": 7,
                        "title": "editor",
                        "attach_supported": true,
                        "attach_kind": "tmux",
                    }],
                },
                {
                    "tab_id": 102,
                    "name": "logs",
                    "panes": [{
                        "pane_id": 8,
                        "title": "logs",
                        "attach_supported": false,
                        "attach_kind": "unsupported",
                    }],
                }
            ],
        }],
        "dashboard": {
            "saved_views": [{"name": "Recent Alerts"}],
        },
        "detached_sessions": [{
            "session_name": "taarof-detached",
            "is_detached": true,
        }],
    });
    let events_payload = serde_json::json!({
        "schema": "taarof.events.v1",
        "events": [{
            "seq": 42,
            "ts_unix_ms": 1712534400000u64,
            "event_type": "alert_raised",
            "payload": {"message": "disk almost full"},
        }],
    });

    let (port, (bridge_rx, _events)) = start_http_server_on_free_loopback_port(|config| {
        http::start_http_server(&runtime_dir, config)
    });
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime should start");
        rt.block_on(async move {
            let mut bridge_rx = bridge_rx;
            while let Some(request) = bridge_rx.recv().await {
                match request {
                    http::HttpBridgeRequest::QueryState { reply } => {
                        let _ = reply.send(state_payload.clone());
                    }
                    http::HttpBridgeRequest::QueryStateProjection { projection, reply } => {
                        let _ = reply.send(http_state_projection(&state_payload, projection));
                    }
                    http::HttpBridgeRequest::QueryHealth { reply } => {
                        let _ = reply.send(state_payload["health"].clone());
                    }
                    http::HttpBridgeRequest::QueryEvents { reply, .. } => {
                        let _ = reply.send(events_payload.clone());
                    }
                    http::HttpBridgeRequest::QueryAgentBindings { reply } => {
                        let _ = reply.send(Vec::new());
                    }
                    http::HttpBridgeRequest::ResolvePaneAttach { reply, .. } => {
                        let _ = reply.send(http::PaneAttachLookup::NotFound);
                    }
                    http::HttpBridgeRequest::CaptureVtePaneSnapshot { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                    http::HttpBridgeRequest::EmitEvent { .. } => {}
                    http::HttpBridgeRequest::ResolvePtyAdapter { reply, .. } => {
                        let _ = reply.send(http::PtyAdapterResolution::NotFound);
                    }
                    http::HttpBridgeRequest::DispatchPtyInput { reply, .. } => {
                        let _ = reply.send(Err("no pty in this test".to_string()));
                    }
                    http::HttpBridgeRequest::DispatchPtyResize { reply, .. } => {
                        let _ = reply.send(Err("no pty in this test".to_string()));
                    }
                    http::HttpBridgeRequest::ControlAction { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                }
            }
        });
    });

    let token_path = http::token_path(&runtime_dir);
    wait_until(Duration::from_secs(2), || token_path.exists());
    let token = std::fs::read_to_string(&token_path).expect("token file should be readable");
    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    let (health_status, health_payload) = http_get_json(addr, "/health", None);
    assert_eq!(health_status, 200);
    assert_eq!(health_payload["ok"], serde_json::json!(true));
    assert_eq!(health_payload["degraded"], serde_json::json!(true));
    assert!(
        health_payload.get("session_name").is_none(),
        "session_name must not leak to unauthenticated /health"
    );
    assert!(
        health_payload.get("health").is_none(),
        "internal health details must not leak to unauthenticated /health"
    );

    let (state_status, state_payload) = http_get_json(addr, "/api/v1/state", Some(token.trim()));
    assert_eq!(state_status, 200);
    assert_eq!(
        state_payload["data"]["schema"],
        serde_json::json!("taarof.state.v1")
    );
    assert_eq!(
        state_payload["data"]["session_name"],
        serde_json::json!("smoke-http")
    );
    assert_eq!(
        state_payload["data"]["active_workspace"],
        serde_json::json!(10)
    );
    assert_eq!(state_payload["data"]["active_tab"], serde_json::json!(101));

    let (sessions_status, sessions_payload) =
        http_get_json(addr, "/api/v1/sessions", Some(token.trim()));
    assert_eq!(sessions_status, 200);
    assert_eq!(
        sessions_payload["data"]["session_name"],
        serde_json::json!("smoke-http")
    );
    assert_eq!(
        sessions_payload["data"]["detached_sessions"][0]["session_name"],
        serde_json::json!("taarof-detached")
    );

    let (workspaces_status, workspaces_payload) =
        http_get_json(addr, "/api/v1/workspaces", Some(token.trim()));
    assert_eq!(workspaces_status, 200);
    assert_eq!(workspaces_payload["data"][0]["id"], serde_json::json!(10));
    assert_eq!(
        workspaces_payload["data"][0]["tabs"][1]["name"],
        serde_json::json!("logs")
    );

    let (tabs_status, tabs_payload) = http_get_json(addr, "/api/v1/tabs", Some(token.trim()));
    assert_eq!(tabs_status, 200);
    assert_eq!(tabs_payload["data"][0]["tab_id"], serde_json::json!(101));
    assert_eq!(
        tabs_payload["data"][0]["workspace_name"],
        serde_json::json!("default")
    );
    assert_eq!(tabs_payload["data"][1]["tab_id"], serde_json::json!(102));

    let (panes_status, panes_payload) = http_get_json(addr, "/api/v1/panes", Some(token.trim()));
    assert_eq!(panes_status, 200);
    assert_eq!(panes_payload["data"][0]["pane_id"], serde_json::json!(7));
    assert_eq!(
        panes_payload["data"][0]["workspace_id"],
        serde_json::json!(10)
    );
    assert_eq!(panes_payload["data"][0]["tab_id"], serde_json::json!(101));
    assert_eq!(
        panes_payload["data"][1]["attach_kind"],
        serde_json::json!("unsupported")
    );

    let (events_status, events_payload) = http_get_json(
        addr,
        "/api/v1/events?since_seq=41&limit=5",
        Some(token.trim()),
    );
    assert_eq!(events_status, 200);
    assert_eq!(
        events_payload["data"]["schema"],
        serde_json::json!("taarof.events.v1")
    );
    assert_eq!(
        events_payload["data"]["events"][0]["event_type"],
        serde_json::json!("alert_raised")
    );
    assert_eq!(
        events_payload["data"]["events"][0]["payload"]["message"],
        serde_json::json!("disk almost full")
    );

    http::cleanup_token_file(&runtime_dir);
}

#[test]
fn http_server_does_not_start_when_token_file_cannot_be_written() {
    let runtime_dir = unique_temp_dir("http-token-missing-parent").join("missing");
    let port = free_loopback_port();
    let config = HttpConfig {
        enabled: true,
        port,
        bind_address: "127.0.0.1".to_string(),
        unsafe_allow_non_loopback: false,
    };

    let server = http::start_http_server(&runtime_dir, &config);
    assert!(
        server.is_none(),
        "HTTP server must not start if the auth token cannot be persisted"
    );
    assert!(!http::token_path(&runtime_dir).exists());
}

#[test]
fn http_events_ws_smoke_streams_broadcast_events() {
    let runtime_dir = unique_temp_dir("http-events-ws");

    let (port, (_bridge_rx, event_tx)) = start_http_server_on_free_loopback_port(|config| {
        http::start_http_server(&runtime_dir, config)
    });

    let token_path = http::token_path(&runtime_dir);
    wait_until(Duration::from_secs(2), || token_path.exists());
    let token = std::fs::read_to_string(&token_path).expect("token file should be readable");
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let expected_event = serde_json::json!({
        "seq": 77,
        "ts_unix_ms": 1712534400123u64,
        "event_type": "tab_selected",
        "payload": {
            "tab_id": 101,
        },
    });

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should start");
    runtime.block_on(async move {
        let url = format!("ws://{addr}/api/v1/events/ws?token={}", token.trim());
        let (mut socket, response) = connect_async(url)
            .await
            .expect("websocket client should connect");
        assert_eq!(response.status(), 101);

        event_tx
            .send(expected_event.clone())
            .expect("event should broadcast to websocket subscribers");

        let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("event frame should arrive")
            .expect("event frame should exist")
            .expect("event frame should be ok")
            .into_text()
            .expect("event frame should be text");
        let payload: Value =
            serde_json::from_str(&message).expect("event frame should be valid json");
        assert_eq!(payload, expected_event);

        let _ = socket.close(None).await;
    });

    http::cleanup_token_file(&runtime_dir);
}

#[test]
fn http_pty_adapter_streams_checkpoint_and_acks_input_over_real_server() {
    use futures_util::SinkExt as _;
    use taarof_app::http::{HttpBridgeRequest, PtyAdapterHandle, PtyAdapterResolution};
    use taarof_app::pty_broker::{PtyBroker, SpawnSpec};
    use tokio_tungstenite::tungstenite::Message;

    let runtime_dir = unique_temp_dir("http-pty-adapter");
    let control = taarof_app::config::HttpControlConfig { enabled: true };
    let dirty = std::sync::Arc::new(taarof_app::http::PaneDirtyRegistry::default());

    let (port, (bridge_rx, _events)) = start_http_server_on_free_loopback_port(|config| {
        http::start_http_server_with_control_config_and_dirty(
            &runtime_dir,
            config,
            &control,
            dirty.clone(),
        )
    });

    let token_path = http::token_path(&runtime_dir);
    wait_until(Duration::from_secs(2), || token_path.exists());
    let token = std::fs::read_to_string(&token_path)
        .expect("token file should be readable")
        .trim()
        .to_string();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("multi-thread runtime should build");
    rt.block_on(async move {
        let pane = std::sync::Arc::new(
            PtyBroker::spawn(SpawnSpec {
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "IFS= read -r line; printf 'ECHO:%s\\n' \"$line\"; sleep 30".to_string(),
                ],
                cwd: None,
                env: Vec::new(),
                cols: 80,
                rows: 24,
            })
            .expect("broker should spawn a fake child"),
        );
        let epoch = pane.epoch().to_string();

        let pane_bg = std::sync::Arc::clone(&pane);
        tokio::spawn(async move {
            let mut rx = bridge_rx;
            while let Some(request) = rx.recv().await {
                match request {
                    HttpBridgeRequest::ResolvePtyAdapter { reply, .. } => {
                        let _ = reply.send(PtyAdapterResolution::Brokered(PtyAdapterHandle {
                            pane: std::sync::Arc::clone(&pane_bg),
                            cols: 80,
                            rows: 24,
                        }));
                    }
                    HttpBridgeRequest::DispatchPtyInput {
                        guard,
                        payload,
                        cancelled,
                        admission_lock,
                        reply,
                        ..
                    } => {
                        let _admission = admission_lock.lock().unwrap();
                        if reply.is_closed() {
                            continue;
                        }
                        let result = if guard.expected_epoch == pane_bg.epoch().to_string() {
                            pane_bg
                                .submit_input(
                                    payload,
                                    Instant::now() + Duration::from_secs(5),
                                    cancelled,
                                )
                                .map_err(|error| error.to_string())
                        } else {
                            Err("epoch changed".to_string())
                        };
                        let _ = reply.send(result);
                    }
                    _ => {}
                }
            }
        });

        let url = format!("ws://{addr}/api/v1/tabs/1/panes/2/pty/ws?token={token}");
        let (mut socket, response) = connect_async(url)
            .await
            .expect("pty websocket should connect");
        assert_eq!(response.status(), 101);

        let first: Value = serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .expect("a checkpoint frame should arrive")
                .expect("frame exists")
                .expect("frame ok")
                .into_text()
                .expect("frame is text"),
        )
        .expect("frame is json");
        assert_eq!(first["kind"], serde_json::json!("checkpoint"));
        assert_eq!(first["epoch"], serde_json::json!(epoch));

        let deadline_ms = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis() as i64)
            + 5000;
        let input = serde_json::json!({
            "kind": "input",
            "protocol_version": { "major": 1, "minor": 0 },
            "runtime_id": "00000000-0000-4000-8000-000000000001",
            "session_name": "default",
            "tab_id": "1",
            "pane_id": "2",
            "epoch": epoch,
            "input_seq": "1",
            "grant_generation": "1",
            "nonce": "00000000-0000-4000-8000-000000000002",
            "deadline_ms": deadline_ms,
            // base64("hi\n")
            "payload_base64": "aGkK",
            "byte_count": 3,
        });
        socket
            .send(Message::text(input.to_string()))
            .await
            .expect("input frame should send");

        let mut saw_ack = false;
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end && !saw_ack {
            let frame: Value = serde_json::from_str(
                &tokio::time::timeout(Duration::from_secs(5), socket.next())
                    .await
                    .expect("a frame should arrive")
                    .expect("frame exists")
                    .expect("frame ok")
                    .into_text()
                    .expect("frame is text"),
            )
            .expect("frame is json");
            if frame["kind"] == serde_json::json!("ack") {
                saw_ack = true;
            }
        }
        assert!(saw_ack, "the dispatched input must be acknowledged");

        let _ = socket.close(None).await;
    });

    http::cleanup_token_file(&runtime_dir);
}

#[test]
fn http_server_returns_500_when_runtime_bridge_is_closed() {
    let runtime_dir = unique_temp_dir("http-closed-bridge");

    let (port, (bridge_rx, _events)) = start_http_server_on_free_loopback_port(|config| {
        http::start_http_server(&runtime_dir, config)
    });
    drop(bridge_rx);

    let token_path = http::token_path(&runtime_dir);
    wait_until(Duration::from_secs(2), || token_path.exists());
    let token = std::fs::read_to_string(&token_path).expect("token file should be readable");

    let (status, payload) = http_get_json(
        SocketAddr::from(([127, 0, 0, 1], port)),
        "/api/v1/state",
        Some(token.trim()),
    );
    assert_eq!(status, 500);
    assert_eq!(payload, serde_json::json!(null));

    http::cleanup_token_file(&runtime_dir);
}

#[test]
fn http_server_serves_web_dist_root_and_spa_fallback() {
    let runtime_dir = unique_temp_dir("http-web-root");
    let dist_dir = unique_temp_dir("http-web-dist");
    let _env = ScopedEnv::set(&[("TAAROF_WEB_DIST_DIR", dist_dir.clone().into_os_string())]);
    std::fs::write(
        dist_dir.join("index.html"),
        "<!doctype html><html><body><div id=\"root\">taarof web smoke</div></body></html>",
    )
    .expect("index.html should be written");
    std::fs::create_dir_all(dist_dir.join("assets")).expect("assets dir should exist");
    std::fs::write(
        dist_dir.join("assets/app.js"),
        "console.log('taarof web smoke');",
    )
    .expect("asset should be written");

    let (port, (bridge_rx, _events)) = start_http_server_on_free_loopback_port(|config| {
        http::start_http_server(&runtime_dir, config)
    });
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime should start");
        rt.block_on(async move {
            let mut bridge_rx = bridge_rx;
            while let Some(request) = bridge_rx.recv().await {
                match request {
                    http::HttpBridgeRequest::QueryState { reply } => {
                        let _ = reply.send(serde_json::json!({
                            "schema": "taarof.state.v1",
                            "session_name": "smoke-http",
                            "health": { "state": "ok", "degraded": false, "degraded_components": 0 },
                            "workspaces": [],
                            "dashboard": {},
                            "detached_sessions": [],
                        }));
                    }
                    http::HttpBridgeRequest::QueryStateProjection { projection: _, reply } => {
                        let _ = reply.send(Value::Array(Vec::new()));
                    }
                    http::HttpBridgeRequest::QueryHealth { reply } => {
                        let _ = reply.send(serde_json::json!({
                            "state": "ok",
                            "degraded": false,
                            "degraded_components": 0,
                        }));
                    }
                    http::HttpBridgeRequest::QueryEvents { reply, .. } => {
                        let _ = reply.send(serde_json::json!({
                            "schema": "taarof.events.v1",
                            "events": [],
                        }));
                    }
                    http::HttpBridgeRequest::QueryAgentBindings { reply } => {
                        let _ = reply.send(Vec::new());
                    }
                    http::HttpBridgeRequest::ResolvePaneAttach { reply, .. } => {
                        let _ = reply.send(http::PaneAttachLookup::NotFound);
                    }
                    http::HttpBridgeRequest::CaptureVtePaneSnapshot { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                    http::HttpBridgeRequest::EmitEvent { .. } => {}
                    http::HttpBridgeRequest::ResolvePtyAdapter { reply, .. } => {
                        let _ = reply.send(http::PtyAdapterResolution::NotFound);
                    }
                    http::HttpBridgeRequest::DispatchPtyInput { reply, .. } => {
                        let _ = reply.send(Err("no pty in this test".to_string()));
                    }
                    http::HttpBridgeRequest::DispatchPtyResize { reply, .. } => {
                        let _ = reply.send(Err("no pty in this test".to_string()));
                    }
                    http::HttpBridgeRequest::ControlAction { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                }
            }
        });
    });

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let (root_status, root_type, root_body) = http_get_text(addr, "/", None);
    assert_eq!(root_status, 200);
    assert!(root_type.starts_with("text/html"));
    assert!(root_body.contains("taarof web smoke"));

    let (spa_status, spa_type, spa_body) = http_get_text(addr, "/workspaces/default", None);
    assert_eq!(spa_status, 200);
    assert!(spa_type.starts_with("text/html"));
    assert!(spa_body.contains("taarof web smoke"));

    let (asset_status, asset_type, asset_body) = http_get_text(addr, "/assets/app.js", None);
    assert_eq!(asset_status, 200);
    assert!(asset_type.contains("javascript"));
    assert!(asset_body.contains("taarof web smoke"));

    http::cleanup_token_file(&runtime_dir);
}

#[test]
fn http_server_falls_back_when_web_dist_override_is_invalid() {
    let runtime_dir = unique_temp_dir("http-web-override-fallback");
    let missing_dist = unique_temp_dir("http-web-override-fallback-dist").join("missing");
    let _env = ScopedEnv::set(&[("TAAROF_WEB_DIST_DIR", missing_dist.clone().into_os_string())]);

    let (port, (bridge_rx, _events)) = start_http_server_on_free_loopback_port(|config| {
        http::start_http_server(&runtime_dir, config)
    });
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime should start");
        rt.block_on(async move {
            let mut bridge_rx = bridge_rx;
            while let Some(request) = bridge_rx.recv().await {
                match request {
                    http::HttpBridgeRequest::QueryState { reply } => {
                        let _ = reply.send(serde_json::json!({
                            "schema": "taarof.state.v1",
                            "session_name": "smoke-http",
                            "health": { "state": "ok", "degraded": false, "degraded_components": 0 },
                            "workspaces": [],
                            "dashboard": {},
                            "detached_sessions": [],
                        }));
                    }
                    http::HttpBridgeRequest::QueryStateProjection { projection: _, reply } => {
                        let _ = reply.send(Value::Array(Vec::new()));
                    }
                    http::HttpBridgeRequest::QueryHealth { reply } => {
                        let _ = reply.send(serde_json::json!({
                            "state": "ok",
                            "degraded": false,
                            "degraded_components": 0,
                        }));
                    }
                    http::HttpBridgeRequest::QueryEvents { reply, .. } => {
                        let _ = reply.send(serde_json::json!({
                            "schema": "taarof.events.v1",
                            "events": [],
                        }));
                    }
                    http::HttpBridgeRequest::QueryAgentBindings { reply } => {
                        let _ = reply.send(Vec::new());
                    }
                    http::HttpBridgeRequest::ResolvePaneAttach { reply, .. } => {
                        let _ = reply.send(http::PaneAttachLookup::NotFound);
                    }
                    http::HttpBridgeRequest::CaptureVtePaneSnapshot { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                    http::HttpBridgeRequest::EmitEvent { .. } => {}
                    http::HttpBridgeRequest::ResolvePtyAdapter { reply, .. } => {
                        let _ = reply.send(http::PtyAdapterResolution::NotFound);
                    }
                    http::HttpBridgeRequest::DispatchPtyInput { reply, .. } => {
                        let _ = reply.send(Err("no pty in this test".to_string()));
                    }
                    http::HttpBridgeRequest::DispatchPtyResize { reply, .. } => {
                        let _ = reply.send(Err("no pty in this test".to_string()));
                    }
                    http::HttpBridgeRequest::ControlAction { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                }
            }
        });
    });

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let (status, content_type, body) = http_get_text(addr, "/", None);
    assert_eq!(status, 200);
    assert!(content_type.starts_with("text/html"));
    assert!(body.contains("<div id=\"root\"></div>"));
    assert!(!body.contains("web bundle missing"));

    http::cleanup_token_file(&runtime_dir);
}

#[test]
fn http_server_returns_generic_503_and_logs_attempted_paths_when_web_bundle_is_missing() {
    let runtime_dir = unique_temp_dir("http-web-missing");
    let state_home = unique_temp_dir("http-web-missing-state");
    let session_name = "http-web-missing";
    let diagnostics_dir = state_home.join("taarof");
    let diagnostics_log = diagnostics_dir.join(format!("diagnostics-{session_name}.jsonl"));
    let diagnostics_archive = diagnostics_dir.join(format!("diagnostics-{session_name}.jsonl.1"));
    let missing_dist = unique_temp_dir("http-web-missing-dist").join("missing");
    let missing_dist_text = missing_dist.display().to_string();
    let _env = ScopedEnv::set(&[
        ("XDG_STATE_HOME", state_home.clone().into_os_string()),
        ("TAAROF_SESSION", OsString::from(session_name)),
    ]);
    taarof_app::reset_diagnostics_journal_for_tests(
        Some(diagnostics_log.clone()),
        Some(diagnostics_archive),
    );

    let (port, (bridge_rx, _events)) = start_http_server_on_free_loopback_port(|config| {
        http::start_http_server_with_web_asset_candidates(
            &runtime_dir,
            config,
            vec![missing_dist.clone()],
        )
    });
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime should start");
        rt.block_on(async move {
            let mut bridge_rx = bridge_rx;
            while let Some(request) = bridge_rx.recv().await {
                match request {
                    http::HttpBridgeRequest::QueryState { reply } => {
                        let _ = reply.send(serde_json::json!({
                            "schema": "taarof.state.v1",
                            "session_name": "smoke-http",
                            "health": { "state": "ok", "degraded": false, "degraded_components": 0 },
                            "workspaces": [],
                            "dashboard": {},
                            "detached_sessions": [],
                        }));
                    }
                    http::HttpBridgeRequest::QueryStateProjection { projection: _, reply } => {
                        let _ = reply.send(Value::Array(Vec::new()));
                    }
                    http::HttpBridgeRequest::QueryHealth { reply } => {
                        let _ = reply.send(serde_json::json!({
                            "state": "ok",
                            "degraded": false,
                            "degraded_components": 0,
                        }));
                    }
                    http::HttpBridgeRequest::QueryEvents { reply, .. } => {
                        let _ = reply.send(serde_json::json!({
                            "schema": "taarof.events.v1",
                            "events": [],
                        }));
                    }
                    http::HttpBridgeRequest::QueryAgentBindings { reply } => {
                        let _ = reply.send(Vec::new());
                    }
                    http::HttpBridgeRequest::ResolvePaneAttach { reply, .. } => {
                        let _ = reply.send(http::PaneAttachLookup::NotFound);
                    }
                    http::HttpBridgeRequest::CaptureVtePaneSnapshot { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                    http::HttpBridgeRequest::EmitEvent { .. } => {}
                    http::HttpBridgeRequest::ResolvePtyAdapter { reply, .. } => {
                        let _ = reply.send(http::PtyAdapterResolution::NotFound);
                    }
                    http::HttpBridgeRequest::DispatchPtyInput { reply, .. } => {
                        let _ = reply.send(Err("no pty in this test".to_string()));
                    }
                    http::HttpBridgeRequest::DispatchPtyResize { reply, .. } => {
                        let _ = reply.send(Err("no pty in this test".to_string()));
                    }
                    http::HttpBridgeRequest::ControlAction { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                }
            }
        });
    });

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let (status, content_type, body) = http_get_text(addr, "/", None);
    assert_eq!(status, 503);
    assert_eq!(content_type, "text/plain; charset=utf-8");
    assert!(body.contains("taarof web bundle missing"));
    assert!(!body.contains(&missing_dist_text));

    wait_until(Duration::from_secs(2), || {
        std::fs::read_to_string(&diagnostics_log)
            .map(|contents| contents.contains(&missing_dist_text))
            .unwrap_or(false)
    });

    let diagnostics_contents =
        std::fs::read_to_string(&diagnostics_log).expect("diagnostics log should be readable");
    let record = diagnostics_contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("diagnostics line should be json"))
        .find(|record| {
            record["category"] == "http_asset_lookup"
                && record["details"]["attempted_paths"]
                    .as_array()
                    .is_some_and(|paths| {
                        paths
                            .iter()
                            .any(|path| path.as_str() == Some(&missing_dist_text))
                    })
        })
        .expect("missing web bundle diagnostic record should be present");
    assert_eq!(record["level"], "info");
    assert_eq!(record["source"], "http");
    assert_eq!(record["action"], "serve-web-assets");
    assert!(record["message"]
        .as_str()
        .is_some_and(|message| message.contains(&missing_dist_text)));

    http::cleanup_token_file(&runtime_dir);
}

#[test]
fn http_attach_route_rejects_missing_auth() {
    let runtime_dir = unique_temp_dir("http-attach-missing-auth");

    let (port, (bridge_rx, _events)) = start_http_server_on_free_loopback_port(|config| {
        http::start_http_server(&runtime_dir, config)
    });
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime should start");
        rt.block_on(async move {
            let mut bridge_rx = bridge_rx;
            while let Some(request) = bridge_rx.recv().await {
                match request {
                    http::HttpBridgeRequest::QueryState { reply } => {
                        let _ = reply.send(serde_json::json!({
                            "schema": "taarof.state.v1",
                            "session_name": "smoke-http",
                            "health": { "state": "ok", "degraded": false, "degraded_components": 0 },
                            "workspaces": [],
                            "dashboard": {},
                            "detached_sessions": [],
                        }));
                    }
                    http::HttpBridgeRequest::QueryStateProjection { projection: _, reply } => {
                        let _ = reply.send(Value::Array(Vec::new()));
                    }
                    http::HttpBridgeRequest::QueryHealth { reply } => {
                        let _ = reply.send(serde_json::json!({
                            "state": "ok",
                            "degraded": false,
                            "degraded_components": 0,
                        }));
                    }
                    http::HttpBridgeRequest::QueryEvents { reply, .. } => {
                        let _ = reply.send(serde_json::json!({
                            "schema": "taarof.events.v1",
                            "events": [],
                        }));
                    }
                    http::HttpBridgeRequest::QueryAgentBindings { reply } => {
                        let _ = reply.send(Vec::new());
                    }
                    http::HttpBridgeRequest::ResolvePaneAttach { reply, .. } => {
                        let _ = reply.send(http::PaneAttachLookup::NotFound);
                    }
                    http::HttpBridgeRequest::CaptureVtePaneSnapshot { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                    http::HttpBridgeRequest::EmitEvent { .. } => {}
                    http::HttpBridgeRequest::ResolvePtyAdapter { reply, .. } => {
                        let _ = reply.send(http::PtyAdapterResolution::NotFound);
                    }
                    http::HttpBridgeRequest::DispatchPtyInput { reply, .. } => {
                        let _ = reply.send(Err("no pty in this test".to_string()));
                    }
                    http::HttpBridgeRequest::DispatchPtyResize { reply, .. } => {
                        let _ = reply.send(Err("no pty in this test".to_string()));
                    }
                    http::HttpBridgeRequest::ControlAction { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                }
            }
        });
    });

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let (status, _, _) = http_request_text(
        addr,
        "/api/v1/panes/7/attach",
        &[
            ("connection", "upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="), // gitleaks:allow -- RFC 6455 sample nonce
        ],
    );
    assert_eq!(status, 401);

    http::cleanup_token_file(&runtime_dir);
}

#[test]
fn http_attach_route_rejects_unsupported_panes() {
    let runtime_dir = unique_temp_dir("http-attach-unsupported");

    let (port, (bridge_rx, _events)) = start_http_server_on_free_loopback_port(|config| {
        http::start_http_server(&runtime_dir, config)
    });
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime should start");
        rt.block_on(async move {
            let mut bridge_rx = bridge_rx;
            while let Some(request) = bridge_rx.recv().await {
                match request {
                    http::HttpBridgeRequest::QueryState { reply } => {
                        let _ = reply.send(serde_json::json!({
                            "schema": "taarof.state.v1",
                            "session_name": "smoke-http",
                            "health": { "state": "ok", "degraded": false, "degraded_components": 0 },
                            "workspaces": [],
                            "dashboard": {},
                            "detached_sessions": [],
                        }));
                    }
                    http::HttpBridgeRequest::QueryStateProjection { projection: _, reply } => {
                        let _ = reply.send(Value::Array(Vec::new()));
                    }
                    http::HttpBridgeRequest::QueryHealth { reply } => {
                        let _ = reply.send(serde_json::json!({
                            "state": "ok",
                            "degraded": false,
                            "degraded_components": 0,
                        }));
                    }
                    http::HttpBridgeRequest::QueryEvents { reply, .. } => {
                        let _ = reply.send(serde_json::json!({
                            "schema": "taarof.events.v1",
                            "events": [],
                        }));
                    }
                    http::HttpBridgeRequest::QueryAgentBindings { reply } => {
                        let _ = reply.send(Vec::new());
                    }
                    http::HttpBridgeRequest::ResolvePaneAttach { reply, .. } => {
                        let _ = reply.send(http::PaneAttachLookup::Unsupported);
                    }
                    http::HttpBridgeRequest::CaptureVtePaneSnapshot { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                    http::HttpBridgeRequest::EmitEvent { .. } => {}
                    http::HttpBridgeRequest::ResolvePtyAdapter { reply, .. } => {
                        let _ = reply.send(http::PtyAdapterResolution::NotFound);
                    }
                    http::HttpBridgeRequest::DispatchPtyInput { reply, .. } => {
                        let _ = reply.send(Err("no pty in this test".to_string()));
                    }
                    http::HttpBridgeRequest::DispatchPtyResize { reply, .. } => {
                        let _ = reply.send(Err("no pty in this test".to_string()));
                    }
                    http::HttpBridgeRequest::ControlAction { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                }
            }
        });
    });

    let token_path = http::token_path(&runtime_dir);
    wait_until(Duration::from_secs(2), || token_path.exists());
    let token = std::fs::read_to_string(&token_path).expect("token file should be readable");

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let (status, content_type, body) = http_request_text(
        addr,
        "/api/v1/panes/7/attach",
        &[
            ("connection", "upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="), // gitleaks:allow -- RFC 6455 sample nonce
            ("authorization", &format!("Bearer {}", token.trim())),
        ],
    );
    assert_eq!(status, 409);
    assert!(content_type.starts_with("application/json"));
    assert!(body.contains("live attach is not supported for this pane type"));

    http::cleanup_token_file(&runtime_dir);
}

#[test]
fn socket_server_smoke_handles_query_state_requests() {
    let runtime_dir = unique_temp_dir("socket-runtime");
    let config_home = unique_temp_dir("socket-config");
    let _env = ScopedEnv::set(&[
        ("XDG_RUNTIME_DIR", runtime_dir.clone().into_os_string()),
        ("XDG_CONFIG_HOME", config_home.clone().into_os_string()),
        ("TAAROF_SESSION", OsString::from("socket-smoke")),
    ]);

    let config_dir = config_home.join("taarof");
    std::fs::create_dir_all(&config_dir).expect("config dir should exist");
    std::fs::write(
        config_dir.join("views.json"),
        serde_json::json!({
            "version": 1,
            "views": [
                {
                    "name": "Recent Alerts",
                    "preset": "recent-alerts",
                    "limit": 7,
                }
            ],
        })
        .to_string(),
    )
    .expect("saved views fixture should be written");
    std::fs::write(
        config_dir.join("templates.json"),
        serde_json::json!({
            "version": 1,
            "templates": [
                {
                    "kind": "tab",
                    "name": "Focus",
                    "tab": {
                        "name": "Shell",
                        "cwd": "/tmp/focus",
                        "panes": {
                            "type": "leaf",
                            "cwd": "/tmp/focus",
                            "ssh_command": null,
                            "tmux_session": null,
                            "tmux_host": null
                        },
                        "discovery_cwd": "/tmp/focus"
                    }
                },
                {
                    "kind": "workspace",
                    "name": "Review",
                    "workspace": {
                        "tabs": [
                            {
                                "name": "editor",
                                "cwd": "/tmp/review",
                                "panes": {
                                    "type": "leaf",
                                    "cwd": "/tmp/review",
                                    "ssh_command": null,
                                    "tmux_session": null,
                                    "tmux_host": null
                                },
                                "discovery_cwd": "/tmp/review"
                            },
                            {
                                "name": "tests",
                                "cwd": "/tmp/review",
                                "panes": {
                                    "type": "leaf",
                                    "cwd": "/tmp/review",
                                    "ssh_command": null,
                                    "tmux_session": null,
                                    "tmux_host": null
                                },
                                "discovery_cwd": "/tmp/review"
                            }
                        ],
                        "active_tab_index": 1
                    }
                }
            ]
        })
        .to_string(),
    )
    .expect("saved templates fixture should be written");

    let state = Rc::new(RefCell::new(AppState::new()));
    let socket_path = socket::start_read_only_socket_server(state, &runtime_dir)
        .expect("socket server should start");

    let (response_sender, response_receiver) = mpsc::channel();
    std::thread::spawn({
        let socket_path = socket_path.clone();
        move || {
            let mut stream = connect_unix(&socket_path);
            stream
                .write_all(br#"{"action":"query-state"}"#)
                .expect("socket request should be written");
            stream
                .shutdown(Shutdown::Write)
                .expect("socket write half should close");
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .expect("socket response should be readable");
            response_sender
                .send(response)
                .expect("socket response should be forwarded");
        }
    });

    let main_context = glib::MainContext::default();
    let response = {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(response) = response_receiver.try_recv() {
                break response;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for socket response"
            );
            while main_context.pending() {
                main_context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    let payload: Value =
        serde_json::from_str(response.trim()).expect("socket response should be json");
    assert_eq!(payload["ok"], serde_json::json!(true));
    assert_eq!(
        payload["data"]["schema"],
        serde_json::json!("taarof.state.v1")
    );
    assert_eq!(
        payload["data"]["session_name"],
        serde_json::json!("socket-smoke")
    );
    assert_eq!(
        payload["data"]["saved_views"][0]["name"],
        serde_json::json!("Recent Alerts")
    );
    assert_eq!(
        payload["data"]["saved_views"][0]["effective_limit"],
        serde_json::json!(7)
    );
    assert_eq!(
        payload["data"]["saved_templates"][0]["kind"],
        serde_json::json!("tab")
    );
    assert_eq!(
        payload["data"]["saved_templates"][0]["pane_count"],
        serde_json::json!(1)
    );
    assert_eq!(
        payload["data"]["saved_templates"][1]["kind"],
        serde_json::json!("workspace")
    );
    assert_eq!(
        payload["data"]["saved_templates"][1]["tab_names"],
        serde_json::json!(["editor", "tests"])
    );

    socket::cleanup_socket(&socket_path);
}

#[test]
fn detached_session_socket_roundtrip_lists_detached_entry() {
    let runtime_dir = unique_temp_dir("ds1");
    let _env = ScopedEnv::set(&[("TAAROF_SESSION", OsString::from("ds1"))]);

    let mut state = AppState::new();
    let workspace_id = state.active_workspace;
    let (_tab_id, pane_id) = seed_headless_terminal_tab(
        &mut state,
        workspace_id,
        "Detached tmux",
        HeadlessPaneSeed {
            shell_running: true,
            tmux_session: Some("taarof-smoke-detached".to_string()),
            ..HeadlessPaneSeed::default()
        },
    )
    .expect("headless pane should be seeded");
    let state = Rc::new(RefCell::new(state));
    let socket_path = socket::start_headless_smoke_socket_server(state.clone(), &runtime_dir)
        .expect("headless socket server should start");

    let detach = socket_request_json(
        &socket_path,
        serde_json::json!({
            "action": "detach-pane",
            "pane": pane_id,
        }),
    );
    assert_eq!(detach["ok"], serde_json::json!(true));
    assert_eq!(
        detach["data"]["session"],
        serde_json::json!("taarof-smoke-detached")
    );
    assert_eq!(state.borrow().detached_sessions.len(), 1);

    let list = socket_request_json(
        &socket_path,
        serde_json::json!({ "action": "list-detached" }),
    );
    let entries = list["data"]
        .as_array()
        .expect("list-detached should return an array");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0]["session_name"],
        serde_json::json!("taarof-smoke-detached")
    );
    assert_eq!(entries[0]["workspace"], serde_json::json!("default"));
    assert_eq!(entries[0]["is_detached"], serde_json::json!(true));

    let snapshot =
        socket_request_json(&socket_path, serde_json::json!({ "action": "query-state" }));
    let detached_sessions = snapshot["data"]["detached_sessions"]
        .as_array()
        .expect("query-state detached sessions should be an array");
    assert_eq!(detached_sessions.len(), 1);
    assert_eq!(
        detached_sessions[0]["session_name"],
        serde_json::json!("taarof-smoke-detached")
    );
    assert_eq!(detached_sessions[0]["is_detached"], serde_json::json!(true));

    socket::cleanup_socket(&socket_path);
}

#[test]
fn close_tab_socket_detach_roundtrip_lists_and_reattaches_session() {
    let runtime_dir = unique_temp_dir("close-detach");
    let _env = ScopedEnv::set(&[("TAAROF_SESSION", OsString::from("close-detach"))]);

    let mut state = AppState::new();
    let workspace_id = state.active_workspace;
    let (closing_tab_id, _closing_pane_id) = seed_headless_terminal_tab(
        &mut state,
        workspace_id,
        "Socket close target",
        HeadlessPaneSeed {
            shell_running: true,
            tmux_session: Some("taarof-close-socket-detach".to_string()),
            ..HeadlessPaneSeed::default()
        },
    )
    .expect("closing tab should be seeded");
    let (survivor_tab_id, survivor_pane_id) = seed_headless_terminal_tab(
        &mut state,
        workspace_id,
        "Socket close survivor",
        HeadlessPaneSeed {
            shell_running: true,
            ..HeadlessPaneSeed::default()
        },
    )
    .expect("survivor tab should be seeded");
    let state = Rc::new(RefCell::new(state));
    let socket_path = socket::start_headless_smoke_socket_server_with_close_behavior(
        state.clone(),
        &runtime_dir,
        TmuxCloseBehavior::Detach,
    )
    .expect("headless socket server should start");

    let close = socket_request_json(
        &socket_path,
        serde_json::json!({
            "action": "close-tab",
            "tab": closing_tab_id.to_string(),
        }),
    );
    assert_eq!(close["ok"], serde_json::json!(true));
    assert!(state.borrow().find_tab(closing_tab_id).is_none());

    let list = socket_request_json(
        &socket_path,
        serde_json::json!({ "action": "list-detached" }),
    );
    assert_eq!(
        list["data"],
        serde_json::json!([{
            "session_name": "taarof-close-socket-detach",
            "host": "localhost",
            "workspace": "default",
            "ssh_target": null,
            "finished": false,
            "last_command": null,
            "is_detached": true,
        }])
    );

    let attach = socket_request_json(
        &socket_path,
        serde_json::json!({
            "action": "attach-session",
            "session_name": "taarof-close-socket-detach",
        }),
    );
    assert_eq!(attach["ok"], serde_json::json!(true));
    assert!(state.borrow().detached_sessions.is_empty());
    let st = state.borrow();
    assert_eq!(
        st.active_ws().map(|workspace| workspace.active_tab),
        Some(survivor_tab_id)
    );
    assert_eq!(
        st.headless_pane(survivor_tab_id, survivor_pane_id)
            .and_then(|pane| pane.tmux_backing.as_ref())
            .map(|backing| backing.session_name.as_str()),
        Some("taarof-close-socket-detach")
    );

    socket::cleanup_socket(&socket_path);
}

#[test]
fn close_tab_socket_detach_preflight_preserves_last_tab_backing() {
    let runtime_dir = unique_temp_dir("close-detach-last");
    let _env = ScopedEnv::set(&[("TAAROF_SESSION", OsString::from("close-detach-last"))]);
    let mut state = AppState::new();
    let workspace_id = state.active_workspace;
    let (tab_id, pane_id) = seed_headless_terminal_tab(
        &mut state,
        workspace_id,
        "Only tmux tab",
        HeadlessPaneSeed {
            tmux_session: Some("must-not-detach".to_string()),
            ..HeadlessPaneSeed::default()
        },
    )
    .expect("only tab should be seeded");
    let state = Rc::new(RefCell::new(state));
    let socket_path = socket::start_headless_smoke_socket_server_with_close_behavior(
        state.clone(),
        &runtime_dir,
        TmuxCloseBehavior::Detach,
    )
    .expect("headless socket server should start");

    let close = socket_request_json(
        &socket_path,
        serde_json::json!({ "action": "close-tab", "tab": tab_id.to_string() }),
    );

    assert_eq!(close["ok"], serde_json::json!(false));
    assert!(close["error"].as_str().unwrap().contains("last tab"));
    let st = state.borrow();
    assert!(st.detached_sessions.is_empty());
    assert!(st.find_tab(tab_id).is_some());
    assert_eq!(
        st.headless_pane(tab_id, pane_id)
            .and_then(|pane| pane.tmux_backing.as_ref())
            .map(|backing| backing.session_name.as_str()),
        Some("must-not-detach")
    );

    socket::cleanup_socket(&socket_path);
}

#[test]
fn close_tab_socket_close_policy_does_not_create_detached_entry() {
    let runtime_dir = unique_temp_dir("close-policy");
    let _env = ScopedEnv::set(&[("TAAROF_SESSION", OsString::from("close-policy"))]);
    let mut state = AppState::new();
    let workspace_id = state.active_workspace;
    let (closing_tab_id, _) = seed_headless_terminal_tab(
        &mut state,
        workspace_id,
        "Close policy target",
        HeadlessPaneSeed {
            tmux_session: Some("close-policy-session".to_string()),
            ..HeadlessPaneSeed::default()
        },
    )
    .expect("closing tab should be seeded");
    seed_headless_terminal_tab(
        &mut state,
        workspace_id,
        "Survivor",
        HeadlessPaneSeed::default(),
    )
    .expect("survivor should be seeded");
    let state = Rc::new(RefCell::new(state));
    let socket_path = socket::start_headless_smoke_socket_server_with_close_behavior(
        state.clone(),
        &runtime_dir,
        TmuxCloseBehavior::Close,
    )
    .expect("headless socket server should start");

    let close = socket_request_json(
        &socket_path,
        serde_json::json!({ "action": "close-tab", "tab": closing_tab_id.to_string() }),
    );

    assert_eq!(close["ok"], serde_json::json!(true));
    assert!(state.borrow().find_tab(closing_tab_id).is_none());
    assert!(state.borrow().detached_sessions.is_empty());
    let list = socket_request_json(
        &socket_path,
        serde_json::json!({ "action": "list-detached" }),
    );
    assert_eq!(list["data"], serde_json::json!([]));

    socket::cleanup_socket(&socket_path);
}

#[test]
fn task_bound_work_report_socket_roundtrip_is_scoped_and_observational() {
    let runtime_dir = unique_temp_dir("work-report");
    let checkout = unique_temp_dir("work-report-checkout");
    std::fs::create_dir_all(checkout.join(".git")).unwrap();
    std::fs::create_dir_all(checkout.join(".plan")).unwrap();
    std::fs::create_dir_all(checkout.join("src")).unwrap();
    let plan_path = checkout.join(".plan/tasks.json");
    std::fs::write(
        &plan_path,
        r#"{"tasks":[{"id":"EXAMPLE-112","title":"Report work","status":"in_progress"}]}"#,
    )
    .unwrap();
    let plan_before = std::fs::read(&plan_path).unwrap();
    let _env = ScopedEnv::set(&[
        ("TAAROF_SESSION", OsString::from("work-report")),
        ("XDG_DATA_HOME", runtime_dir.join("data").into_os_string()),
    ]);

    let mut app = AppState::new();
    let workspace = app.active_workspace;
    let (tab_id, pane_id) = seed_headless_terminal_tab(
        &mut app,
        workspace,
        "Bound agent",
        HeadlessPaneSeed {
            cwd: Some(checkout.join("src").to_string_lossy().into_owned()),
            shell_running: true,
            ..Default::default()
        },
    )
    .unwrap();
    app.bind_pane_to_task(tab_id, pane_id, "EXAMPLE-112")
        .unwrap();
    let (focused_tab_id, _focused_pane_id) = seed_headless_terminal_tab(
        &mut app,
        workspace,
        "Focused elsewhere",
        HeadlessPaneSeed {
            cwd: Some(checkout.to_string_lossy().into_owned()),
            shell_running: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_ne!(focused_tab_id, tab_id);
    let state = Rc::new(RefCell::new(app));
    let socket_path = socket::start_headless_smoke_socket_server(state.clone(), &runtime_dir)
        .expect("headless socket server should start");

    let context = socket_request_json(
        &socket_path,
        serde_json::json!({
            "action": "work-context",
            "tab": tab_id.to_string(),
            "pane": pane_id,
        }),
    );
    assert_eq!(context["ok"], serde_json::json!(true));
    assert_eq!(context["data"]["task_id"], serde_json::json!("EXAMPLE-112"));
    assert_eq!(
        context["data"]["checkout_root"],
        serde_json::json!(checkout)
    );
    assert!(context["data"]["instructions"]
        .as_str()
        .unwrap()
        .contains("never mutate .plan"));

    let wrong_tab = socket_request_json(
        &socket_path,
        serde_json::json!({
            "action": "work-context",
            "tab": focused_tab_id.to_string(),
            "pane": pane_id,
        }),
    );
    assert_eq!(wrong_tab["ok"], serde_json::json!(false));

    let data = context["data"].clone();
    let verified_pr = taarof_app::work_ledger::VerifiedPullRequestBinding {
        identity: taarof_app::work_ledger::WorkIdentity {
            session: data["session"].as_str().unwrap().to_string(),
            workspace_origin: data["workspace_origin"].as_str().unwrap().to_string(),
            tab_origin: data["tab_origin"].as_str().unwrap().to_string(),
            pane_origin: data["pane_origin"].as_str().unwrap().to_string(),
            workspace_id: workspace,
            workspace_name: "default".into(),
            tab_id,
            tab_name: "Bound agent".into(),
            pane_id,
            task_id: Some("EXAMPLE-112".into()),
            task_title: Some("Report work".into()),
        },
        task_id: "EXAMPLE-112".into(),
        title: "Report work".into(),
        checkout_root: checkout.to_string_lossy().into_owned(),
        reporting_token: data["binding_token"].as_str().unwrap().to_string(),
        base_repository: "owner/repo".into(),
        head_owner: "owner".into(),
        branch: "agent/EXAMPLE-112".into(),
    };
    let pr_data = taarof_app::BranchPullRequestsData {
        total: 1,
        open: 1,
        draft: 0,
        merged: 0,
        closed: 0,
        query_complete: true,
        pull_requests: vec![taarof_app::BranchPullRequestEntry {
            number: 112,
            title: "Report work".into(),
            state: "open".into(),
            is_draft: false,
            review_decision: None,
            url: Some("https://github.com/owner/repo/pull/112".into()),
            head_ref_name: "agent/EXAMPLE-112".into(),
            head_repository_owner: "owner".into(),
            base_ref_name: "main".into(),
            updated_at: None,
            checks: taarof_app::PullRequestChecks::Pending,
            correlation: Some(taarof_app::PullRequestCorrelationMetadata {
                task_id: "EXAMPLE-112".into(),
                repo: "owner/repo".into(),
                branch: "agent/EXAMPLE-112".into(),
                dispatch_source: "agent".into(),
            }),
            correlation_marker_present: true,
        }],
    };
    state
        .borrow_mut()
        .observe_verified_pull_requests(&verified_pr, &pr_data);
    let pr_before = state
        .borrow()
        .work_ledger
        .records()
        .into_iter()
        .find_map(|record| record.pull_request)
        .expect("fixture should seed a verified PR fact");
    let report_payload = serde_json::json!({
        "action": "work-report",
        "milestone": "finished",
        "note": "Acceptance checks passed",
        "session": data["session"],
        "workspace_origin": data["workspace_origin"],
        "tab_origin": data["tab_origin"],
        "pane_origin": data["pane_origin"],
        "task_id": data["task_id"],
        "checkout_root": data["checkout_root"],
        "binding_token": data["binding_token"],
    });
    let report = socket_request_json(&socket_path, report_payload.clone());
    assert_eq!(report["ok"], serde_json::json!(true));
    assert_eq!(report["tab_id"], serde_json::json!(tab_id));
    assert_eq!(report["pane_id"], serde_json::json!(pane_id));
    assert_eq!(report["data"]["milestone"], serde_json::json!("finished"));
    assert_eq!(std::fs::read(&plan_path).unwrap(), plan_before);
    let pr_after = state
        .borrow()
        .work_ledger
        .records()
        .into_iter()
        .filter_map(|record| record.pull_request)
        .next_back()
        .expect("verified PR fact should remain present");
    assert_eq!(pr_after, pr_before);

    // A transcript completion claim is a second observational source. Neither
    // it nor the finished milestone may write canonical task state.
    state
        .borrow_mut()
        .project_agent_message(&serde_json::json!({
            "tab_id": tab_id,
            "pane_id": pane_id,
            "session_id": "fixture-agent-session",
            "work_events": [{ "kind": "assistant_completed", "ordinal": 1 }],
        }));
    let probe = {
        let state = state.borrow();
        taarof_app::work_ledger::collect_probe(&state)
    };
    let probe = taarof_app::work_ledger::enrich_plan_probe(probe);
    state.borrow_mut().observe_work_probe(probe);
    assert_eq!(std::fs::read(&plan_path).unwrap(), plan_before);
    let pr_after_transcript = state
        .borrow()
        .work_ledger
        .records()
        .into_iter()
        .filter_map(|record| record.pull_request)
        .next_back()
        .expect("verified PR fact should survive transcript observation");
    assert_eq!(pr_after_transcript, pr_before);

    let mut stale = report_payload.clone();
    stale["binding_token"] = serde_json::json!("ctx-00000000000000000000000000000000");
    assert_eq!(
        socket_request_json(&socket_path, stale)["ok"],
        serde_json::json!(false)
    );
    let mut secret = report_payload;
    secret["note"] = serde_json::json!("password=hunter2");
    assert_eq!(
        socket_request_json(&socket_path, secret)["ok"],
        serde_json::json!(false)
    );
    let records = state.borrow().work_ledger.records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record.kind,
                taarof_app::work_ledger::WorkKind::AgentReportedFinished
            ))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record.kind,
                taarof_app::work_ledger::WorkKind::AssistantMessageCompleted
            ))
            .count(),
        1
    );
    let snapshot =
        socket_request_json(&socket_path, serde_json::json!({ "action": "query-state" }));
    let finished = snapshot["data"]["work"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["record"]["kind"] == "agent_reported_finished")
        .expect("finished work entry should be projected");
    assert_eq!(finished["reconciliation"]["status"], "unverified");
    assert_eq!(finished["reconciliation"]["current_value"], "in_progress");
    let events = socket_request_json(
        &socket_path,
        serde_json::json!({ "action": "query-events" }),
    );
    let event_text = serde_json::to_string(&events).unwrap();
    assert!(!event_text.contains("ctx-"));
    assert!(!event_text.contains("hunter2"));
    assert_eq!(std::fs::read(&plan_path).unwrap(), plan_before);

    socket::cleanup_socket(&socket_path);
    drop(state);
    let _ = std::fs::remove_dir_all(runtime_dir);
    let _ = std::fs::remove_dir_all(checkout);
}

#[test]
fn detached_session_socket_roundtrip_clears_entry_after_attach() {
    let runtime_dir = unique_temp_dir("ds2");
    let _env = ScopedEnv::set(&[("TAAROF_SESSION", OsString::from("ds2"))]);

    let mut state = AppState::new();
    let workspace_id = state.active_workspace;
    let (tab_id, pane_id) = seed_headless_terminal_tab(
        &mut state,
        workspace_id,
        "Detached tmux",
        HeadlessPaneSeed {
            shell_running: true,
            tmux_session: Some("taarof-smoke-reattach".to_string()),
            ..HeadlessPaneSeed::default()
        },
    )
    .expect("headless pane should be seeded");
    let state = Rc::new(RefCell::new(state));
    let socket_path = socket::start_headless_smoke_socket_server(state.clone(), &runtime_dir)
        .expect("headless socket server should start");

    let detach = socket_request_json(
        &socket_path,
        serde_json::json!({
            "action": "detach-pane",
            "pane": pane_id,
        }),
    );
    assert_eq!(detach["ok"], serde_json::json!(true));
    assert_eq!(state.borrow().detached_sessions.len(), 1);

    let attach = socket_request_json(
        &socket_path,
        serde_json::json!({
            "action": "attach-session",
            "session_name": "taarof-smoke-reattach",
        }),
    );
    assert_eq!(attach["ok"], serde_json::json!(true));
    assert!(state.borrow().detached_sessions.is_empty());

    let list = socket_request_json(
        &socket_path,
        serde_json::json!({ "action": "list-detached" }),
    );
    assert_eq!(list["data"], serde_json::json!([]));

    let snapshot =
        socket_request_json(&socket_path, serde_json::json!({ "action": "query-state" }));
    assert_eq!(snapshot["data"]["detached_sessions"], serde_json::json!([]));
    assert_eq!(
        snapshot["data"]["workspaces"][0]["tabs"][0]["panes"][0]["tmux_session"],
        serde_json::json!("taarof-smoke-reattach")
    );

    let st = state.borrow();
    let (_, tab) = st.find_tab(tab_id).expect("seeded tab should still exist");
    assert_eq!(tab.focused_pane_id, pane_id);
    let pane = st
        .headless_pane(tab_id, pane_id)
        .expect("headless pane metadata should still exist");
    assert_eq!(
        pane.tmux_backing
            .as_ref()
            .map(|backing| backing.session_name.as_str()),
        Some("taarof-smoke-reattach")
    );

    socket::cleanup_socket(&socket_path);
}

#[test]
fn detached_session_socket_attach_requires_target_identity_when_names_collide() {
    let runtime_dir = unique_temp_dir("ds3");
    let _env = ScopedEnv::set(&[("TAAROF_SESSION", OsString::from("ds3"))]);

    let mut state = AppState::new();
    let workspace_id = state.active_workspace;
    let (local_tab_id, local_pane_id) = seed_headless_terminal_tab(
        &mut state,
        workspace_id,
        "Local tmux",
        HeadlessPaneSeed {
            tmux_session: Some("shared-session".to_string()),
            ..HeadlessPaneSeed::default()
        },
    )
    .expect("headless pane should be seeded");
    let (_remote_tab_id, remote_pane_id) = seed_headless_terminal_tab(
        &mut state,
        workspace_id,
        "Remote tmux",
        HeadlessPaneSeed {
            tmux_session: Some("shared-session".to_string()),
            tmux_ssh_target: Some("builder@ci-box".to_string()),
            ..HeadlessPaneSeed::default()
        },
    )
    .expect("remote headless pane should be seeded");

    let state = Rc::new(RefCell::new(state));
    let socket_path = socket::start_headless_smoke_socket_server(state.clone(), &runtime_dir)
        .expect("headless socket server should start");

    let local_detach = socket_request_json(
        &socket_path,
        serde_json::json!({
            "action": "detach-pane",
            "tab": local_tab_id.to_string(),
            "pane": local_pane_id,
        }),
    );
    assert_eq!(local_detach["ok"], serde_json::json!(true));

    let remote_detach = socket_request_json(
        &socket_path,
        serde_json::json!({
            "action": "detach-pane",
            "pane": remote_pane_id,
        }),
    );
    assert_eq!(remote_detach["ok"], serde_json::json!(true));

    let ambiguous = socket_request_json(
        &socket_path,
        serde_json::json!({
            "action": "attach-session",
            "session_name": "shared-session",
        }),
    );
    assert_eq!(ambiguous["ok"], serde_json::json!(false));
    assert!(ambiguous["error"]
        .as_str()
        .expect("attach error should be a string")
        .contains("ambiguous"));

    let attach = socket_request_json(
        &socket_path,
        serde_json::json!({
            "action": "attach-session",
            "session_name": "shared-session",
            "ssh_target": "builder@ci-box",
        }),
    );
    assert_eq!(attach["ok"], serde_json::json!(true));

    let list = socket_request_json(
        &socket_path,
        serde_json::json!({ "action": "list-detached" }),
    );
    let entries = list["data"]
        .as_array()
        .expect("list-detached should return an array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["host"], serde_json::json!("localhost"));
    assert!(entries[0]["ssh_target"].is_null());

    let st = state.borrow();
    let (_, tab) = st
        .find_tab(local_tab_id)
        .expect("seeded tab should still exist");
    assert_eq!(tab.focused_pane_id, local_pane_id);
    let pane = st
        .headless_pane(local_tab_id, local_pane_id)
        .expect("headless pane metadata should still exist");
    let backing = pane
        .tmux_backing
        .as_ref()
        .expect("headless pane should now be tmux-backed");
    assert_eq!(backing.session_name, "shared-session");
    drop(st);

    let snapshot =
        socket_request_json(&socket_path, serde_json::json!({ "action": "query-state" }));
    assert_eq!(
        snapshot["data"]["workspaces"][0]["tabs"][0]["panes"][0]["tmux_host"],
        serde_json::json!("builder@ci-box")
    );

    socket::cleanup_socket(&socket_path);
}

#[test]
fn broadcast_commit_reentrancy_does_not_recurse_into_peer_commits() {
    let result = taarof_app::exercise_broadcast_commit_reentrancy_harness(
        3,
        Some(taarof_app::BroadcastScope::Tab),
        "status\n",
    );

    assert_eq!(result.forward_entries, 1);
    assert_eq!(result.peer_lookup_count, 1);
    assert_eq!(result.suppressed_reentrant_entries, 2);
}

#[test]
fn broadcast_commit_forwards_bytes_exactly_once_per_peer() {
    let enabled = taarof_app::exercise_broadcast_commit_reentrancy_harness(
        3,
        Some(taarof_app::BroadcastScope::Workspace),
        "echo hi\r",
    );
    assert_eq!(enabled.received_payloads[0], Vec::<String>::new());
    assert_eq!(enabled.received_payloads[1], vec!["echo hi\r".to_string()]);
    assert_eq!(enabled.received_payloads[2], vec!["echo hi\r".to_string()]);

    let disabled = taarof_app::exercise_broadcast_commit_reentrancy_harness(3, None, "echo hi\r");
    assert_eq!(disabled.peer_lookup_count, 0);
    assert_eq!(
        disabled.received_payloads,
        vec![
            Vec::<String>::new(),
            Vec::<String>::new(),
            Vec::<String>::new(),
        ]
    );
}

#[test]
fn remote_host_snapshot_smoke_serializes_remote_pane_fields() {
    let runtime_dir = unique_temp_dir("rp");
    let _env = ScopedEnv::set(&[("TAAROF_SESSION", OsString::from("rp"))]);

    let mut state = AppState::new();
    let workspace_id = state.active_workspace;
    seed_headless_terminal_tab(
        &mut state,
        workspace_id,
        "Remote shell",
        HeadlessPaneSeed {
            cwd: Some("/srv/taarof".to_string()),
            cwd_host: Some("devbox".to_string()),
            has_child_process: true,
            remote_shell: true,
            ssh_command: Some(vec!["ssh".to_string(), "devbox".to_string()]),
            ..HeadlessPaneSeed::default()
        },
    )
    .expect("remote pane should be seeded");
    let state = Rc::new(RefCell::new(state));
    let socket_path = socket::start_read_only_socket_server(state, &runtime_dir)
        .expect("read-only socket server should start");

    let snapshot =
        socket_request_json(&socket_path, serde_json::json!({ "action": "query-state" }));
    let pane = &snapshot["data"]["workspaces"][0]["tabs"][0]["panes"][0];
    assert_eq!(pane["cwd"], serde_json::json!("/srv/taarof"));
    assert_eq!(pane["cwd_host"], serde_json::json!("devbox"));
    assert_eq!(pane["remote_shell"], serde_json::json!(true));
    assert!(pane["has_child_process"].is_null());

    socket::cleanup_socket(&socket_path);
}

#[test]
fn remote_host_snapshot_smoke_serializes_remote_tmux_host() {
    let runtime_dir = unique_temp_dir("rt");
    let _env = ScopedEnv::set(&[("TAAROF_SESSION", OsString::from("rt"))]);

    let mut state = AppState::new();
    let workspace_id = state.active_workspace;
    seed_headless_terminal_tab(
        &mut state,
        workspace_id,
        "Remote tmux",
        HeadlessPaneSeed {
            shell_running: true,
            tmux_session: Some("taarof-remote-smoke".to_string()),
            tmux_ssh_target: Some("builder@ci-box".to_string()),
            ..HeadlessPaneSeed::default()
        },
    )
    .expect("remote tmux pane should be seeded");
    let state = Rc::new(RefCell::new(state));
    let socket_path = socket::start_read_only_socket_server(state, &runtime_dir)
        .expect("read-only socket server should start");

    let snapshot =
        socket_request_json(&socket_path, serde_json::json!({ "action": "query-state" }));
    let pane = &snapshot["data"]["workspaces"][0]["tabs"][0]["panes"][0];
    assert_eq!(
        pane["tmux_session"],
        serde_json::json!("taarof-remote-smoke")
    );
    assert_eq!(pane["tmux_host"], serde_json::json!("builder@ci-box"));
    assert_eq!(pane["remote_shell"], serde_json::json!(false));
    assert!(
        pane["has_child_process"].is_null(),
        "authoritative remote tmux metadata must make missed local probe state unknown"
    );

    socket::cleanup_socket(&socket_path);
}

#[test]
fn multi_workspace_restore_smoke_preserves_workspace_selection() {
    let restored = multi_workspace_restore_roundtrip(
        "multi-workspace-selection",
        serde_json::json!({
            "version": 2,
            "session_name": "multi-workspace",
            "workspaces": [
                {
                    "id": 11,
                    "name": "default",
                    "collapsed": false,
                    "repo_root": "/tmp/taarof",
                    "is_worktree": false,
                    "working_tree_path": null,
                    "branch_name": "main",
                    "linked_issue": null,
                    "tabs": [
                        {
                            "name": "editor",
                            "cwd": "/tmp/taarof",
                            "panes": {"type":"leaf","cwd":"/tmp/taarof"},
                            "discovery_cwd": "/tmp/taarof"
                        },
                        {
                            "name": "logs",
                            "cwd": "/tmp/taarof",
                            "panes": {"type":"leaf","cwd":"/tmp/taarof"},
                            "discovery_cwd": "/tmp/taarof"
                        }
                    ],
                    "active_tab_index": 1,
                    "tmux_backed": false,
                    "host_config_name": null
                },
                {
                    "id": 22,
                    "name": "feature-worktree",
                    "collapsed": true,
                    "repo_root": "/tmp/taarof",
                    "is_worktree": true,
                    "working_tree_path": "/tmp/taarof-feature",
                    "branch_name": "feature/release-smoke",
                    "linked_issue": "TASK-003",
                    "tabs": [
                        {
                            "name": "shell",
                            "cwd": "/tmp/taarof-feature",
                            "panes": {"type":"leaf","cwd":"/tmp/taarof-feature"},
                            "discovery_cwd": "/tmp/taarof-feature"
                        },
                        {
                            "name": "tests",
                            "cwd": "/tmp/taarof-feature",
                            "panes": {"type":"leaf","cwd":"file://devbox/tmp/taarof-feature/tests"},
                            "discovery_cwd": "/tmp/taarof-feature"
                        }
                    ],
                    "active_tab_index": 0,
                    "tmux_backed": true,
                    "host_config_name": "ci-box"
                }
            ],
            "active_workspace_index": 1,
            "window_width": 1440,
            "window_height": 960,
            "detached_sessions": [
                {
                    "session_name": "taarof-feature-detached",
                    "host": "localhost",
                    "workspace": "feature-worktree",
                    "last_command": "cargo test"
                }
            ],
            "background_section_collapsed": false
        }),
    );

    assert_eq!(restored.workspaces.len(), 2);
    assert_eq!(restored.workspaces[0].active_tab_index, 1);
    assert_eq!(restored.workspaces[1].active_tab_index, 0);
    assert_eq!(restored.active_workspace_index, 1);
    assert_eq!(restored.background_section_collapsed, Some(false));
}

#[test]
fn multi_workspace_restore_smoke_preserves_worktree_metadata() {
    let restored = multi_workspace_restore_roundtrip(
        "multi-workspace-metadata",
        serde_json::json!({
            "version": 2,
            "session_name": "multi-workspace",
            "workspaces": [
                {
                    "id": 11,
                    "name": "default",
                    "collapsed": false,
                    "repo_root": "/tmp/taarof",
                    "is_worktree": false,
                    "working_tree_path": null,
                    "branch_name": "main",
                    "linked_issue": null,
                    "tabs": [{"name":"editor","cwd":"/tmp/taarof","panes":{"type":"leaf","cwd":"/tmp/taarof"}}],
                    "active_tab_index": 0,
                    "tmux_backed": false,
                    "host_config_name": null
                },
                {
                    "id": 22,
                    "name": "feature-worktree",
                    "collapsed": true,
                    "repo_root": "/tmp/taarof",
                    "is_worktree": true,
                    "working_tree_path": "/tmp/taarof-feature",
                    "branch_name": "feature/release-smoke",
                    "linked_issue": "TASK-003",
                    "tabs": [{"name":"shell","cwd":"/tmp/taarof-feature","panes":{"type":"leaf","cwd":"/tmp/taarof-feature"}}],
                    "active_tab_index": 0,
                    "tmux_backed": true,
                    "host_config_name": "ci-box"
                }
            ],
            "active_workspace_index": 1,
            "window_width": 1440,
            "window_height": 960,
            "detached_sessions": [],
            "background_section_collapsed": true
        }),
    );

    assert!(!restored.workspaces[0].is_worktree);
    assert_eq!(restored.workspaces[0].branch_name.as_deref(), Some("main"));
    assert!(restored.workspaces[1].is_worktree);
    assert_eq!(
        restored.workspaces[1].working_tree_path.as_deref(),
        Some("/tmp/taarof-feature")
    );
    assert_eq!(
        restored.workspaces[1].branch_name.as_deref(),
        Some("feature/release-smoke")
    );
    assert_eq!(
        restored.workspaces[1].host_config_name.as_deref(),
        Some("ci-box")
    );
}

#[test]
fn multi_workspace_restore_smoke_preserves_detached_sessions() {
    let restored = multi_workspace_restore_roundtrip(
        "multi-workspace-detached",
        serde_json::json!({
            "version": 2,
            "session_name": "multi-workspace",
            "workspaces": [
                {
                    "id": 11,
                    "name": "default",
                    "collapsed": false,
                    "repo_root": "/tmp/taarof",
                    "is_worktree": false,
                    "working_tree_path": null,
                    "branch_name": "main",
                    "linked_issue": null,
                    "tabs": [{"name":"editor","cwd":"/tmp/taarof","panes":{"type":"leaf","cwd":"/tmp/taarof"}}],
                    "active_tab_index": 0,
                    "tmux_backed": false,
                    "host_config_name": null
                },
                {
                    "id": 22,
                    "name": "feature-worktree",
                    "collapsed": false,
                    "repo_root": "/tmp/taarof",
                    "is_worktree": true,
                    "working_tree_path": "/tmp/taarof-feature",
                    "branch_name": "feature/release-smoke",
                    "linked_issue": null,
                    "tabs": [{"name":"shell","cwd":"/tmp/taarof-feature","panes":{"type":"leaf","cwd":"/tmp/taarof-feature"}}],
                    "active_tab_index": 0,
                    "tmux_backed": false,
                    "host_config_name": null
                }
            ],
            "active_workspace_index": 0,
            "window_width": 1440,
            "window_height": 960,
            "detached_sessions": [
                {
                    "session_name": "taarof-default-detached",
                    "host": "localhost",
                    "workspace": "default",
                    "last_command": "npm test"
                },
                {
                    "session_name": "taarof-remote-detached",
                    "host": "ci-box",
                    "workspace": "feature-worktree",
                    "ssh_target": "builder@ci-box",
                    "last_command": "cargo test"
                }
            ],
            "background_section_collapsed": true
        }),
    );

    let serialized =
        serde_json::to_value(&restored).expect("restored session should serialize back to json");
    let detached_sessions = serialized["detached_sessions"]
        .as_array()
        .expect("detached sessions should remain serialized");
    assert_eq!(detached_sessions.len(), 2);
    assert_eq!(
        detached_sessions[0]["session_name"],
        serde_json::json!("taarof-default-detached")
    );
    assert_eq!(
        detached_sessions[1]["ssh_target"],
        serde_json::json!("builder@ci-box")
    );
    assert_eq!(restored.background_section_collapsed, Some(true));
}

/// Regression test: guard that broadcast call sites always route
/// through `forward_broadcast_bytes` rather than calling `feed_terminals`
/// directly. If a refactor ever reverts a call site to bypass the re-entrancy
/// guard, this test will catch it before it ships.
///
/// The test is source-level (grep-style) because VTE terminals require a live
/// display, making a true behavioral regression test infeasible in CI. A
/// source assertion is the approach endorsed in the original review.
#[test]
fn broadcast_call_sites_all_route_through_forward_broadcast_bytes() {
    // Resolve terminal.rs and its broadcast submodule relative to this
    // integration-test file. Broadcast logic lives in terminal/broadcast.rs
    // after the module decomposition refactor. CARGO_MANIFEST_DIR points at
    // taarof-app/ during `cargo test`.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR must be set by cargo during test builds");
    let src_dir = std::path::Path::new(&manifest_dir).join("src");
    let terminal_src = src_dir.join("terminal.rs");
    let broadcast_src = src_dir.join("terminal").join("broadcast.rs");
    let terminal_source = std::fs::read_to_string(&terminal_src)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", terminal_src.display()));
    let broadcast_source = std::fs::read_to_string(&broadcast_src)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", broadcast_src.display()));
    let source = format!("{terminal_source}\n{broadcast_source}");

    // Count call-style uses of feed_terminals (i.e. `feed_terminals(` but NOT
    // any `fn feed_terminals(` definition line, which may have a visibility
    // prefix like `pub(super)` after the module decomposition refactor).
    // Only one such call should exist and it must be inside `forward_broadcast_bytes`.
    let call_count = source
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            // Match any invocation; exclude any function definition for feed_terminals.
            trimmed.contains("feed_terminals(") && !trimmed.contains("fn feed_terminals(")
        })
        .count();

    assert_eq!(
        call_count, 1,
        "Broadcast guard regression: expected exactly one `feed_terminals(` call site \
         (inside `forward_broadcast_bytes`), found {call_count}. \
         All broadcast writes must go through `forward_broadcast_bytes` so the \
         re-entrancy guard is never bypassed."
    );

    // Additionally assert that forward_broadcast_bytes is used at least 3 times
    // (the three production call sites: send_text_to_active_scope, connect_commit,
    // and the key handler). This catches a refactor that deletes the helper entirely.
    let forwarder_call_count = source
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed.contains("forward_broadcast_bytes(")
                && !trimmed.contains("fn forward_broadcast_bytes(")
        })
        .count();

    assert!(
        forwarder_call_count >= 3,
        "Broadcast guard regression: expected at least 3 `forward_broadcast_bytes(` call sites \
         (send_text_to_active_scope, connect_commit, key handler), found {forwarder_call_count}. \
         Ensure all broadcast write paths are preserved."
    );
}

/// A tab moved between workspaces via the sidebar must persist under its new
/// workspace: after the move, the saved session places it in the target
/// workspace (not the source) and keeps active-workspace / active-tab selection
/// pointing at it, with its pane tree, cwd, and discovery metadata intact.
///
/// This exercises the real on-disk `save_v2` -> `load_v2` path against a layout
/// that represents state *after* `AppState::move_tab_to_workspace` relocated the
/// tab from workspace `source` into workspace `target`.
#[test]
fn move_tab_workspace_state_survives_session_restore() {
    let data_home = unique_temp_dir("move-tab-restore");
    let _env = ScopedEnv::set(&[
        ("XDG_DATA_HOME", data_home.clone().into_os_string()),
        ("TAAROF_SESSION", OsString::from("move-tab-restore")),
    ]);

    let moved_tab = SavedTab {
        name: "runner".to_string(),
        work_origin: Some("tab-runtime-smoke-runner".to_string()),
        cwd: Some("/tmp/user/project".to_string()),
        panes: Some(SavedPaneNode::Leaf {
            work_origin: Some("pane-runtime-smoke-runner".to_string()),
            cwd: Some("/tmp/user/project".to_string()),
            ssh_command: None,
            tmux_session: Some("kmux60".to_string()),
            tmux_host: None,
            tmux_identity: None,
            current_task: None,
            agent_session: None,
        }),
        discovery_cwd: Some("/tmp/user/project".to_string()),
    };

    // Post-move layout: source workspace kept its own shell, target workspace
    // now owns the relocated `runner` tab and is both active and pointing at it.
    let state = SessionStateV2 {
        version: 2,
        session_namespace: None,
        session_identity: None,
        session_name: Some("move-tab".to_string()),
        workspaces: vec![
            SavedWorkspace {
                id: 1,
                work_origin: Some("workspace-runtime-smoke-source".to_string()),
                name: "source".to_string(),
                collapsed: false,
                repo_root: None,
                is_worktree: false,
                working_tree_path: None,
                branch_name: None,
                linked_issue: None,
                tabs: vec![SavedTab {
                    name: "source-shell".to_string(),
                    work_origin: Some("tab-runtime-smoke-source".to_string()),
                    cwd: Some("/tmp/user/other".to_string()),
                    panes: Some(SavedPaneNode::Leaf {
                        work_origin: Some("pane-runtime-smoke-source".to_string()),
                        cwd: Some("/tmp/user/other".to_string()),
                        ssh_command: None,
                        tmux_session: None,
                        tmux_host: None,
                        tmux_identity: None,
                        current_task: None,
                        agent_session: None,
                    }),
                    discovery_cwd: None,
                }],
                active_tab_index: 0,
                tmux_backed: false,
                host_config_name: None,
            },
            SavedWorkspace {
                id: 2,
                work_origin: Some("workspace-runtime-smoke-target".to_string()),
                name: "target".to_string(),
                collapsed: false,
                repo_root: None,
                is_worktree: false,
                working_tree_path: None,
                branch_name: None,
                linked_issue: None,
                tabs: vec![
                    SavedTab {
                        name: "target-shell".to_string(),
                        work_origin: Some("tab-runtime-smoke-target".to_string()),
                        cwd: None,
                        panes: Some(SavedPaneNode::Leaf {
                            work_origin: Some("pane-runtime-smoke-target".to_string()),
                            cwd: None,
                            ssh_command: None,
                            tmux_session: None,
                            tmux_host: None,
                            tmux_identity: None,
                            current_task: None,
                            agent_session: None,
                        }),
                        discovery_cwd: None,
                    },
                    moved_tab,
                ],
                // Active tab within the target is the relocated `runner` (index 1).
                active_tab_index: 1,
                tmux_backed: false,
                host_config_name: None,
            },
        ],
        // Active workspace followed the move to the target (index 1).
        active_workspace_index: 1,
        window_width: 1280,
        window_height: 800,
        detached_sessions: Vec::new(),
        background_section_collapsed: Some(false),
    };

    taarof_app::session::save_v2(&state);
    let restored = taarof_app::session::load_v2().expect("moved-tab session should reload");

    let source = restored
        .workspaces
        .iter()
        .find(|ws| ws.name == "source")
        .expect("source workspace should persist");
    let target = restored
        .workspaces
        .iter()
        .find(|ws| ws.name == "target")
        .expect("target workspace should persist");

    // The moved tab lives under the target workspace only.
    assert!(
        source.tabs.iter().all(|t| t.name != "runner"),
        "moved tab must not remain in the source workspace"
    );
    assert!(
        target.tabs.iter().any(|t| t.name == "runner"),
        "moved tab must be present in the target workspace"
    );

    // Active-tab selection points at the relocated tab within the target.
    assert_eq!(
        target.tabs[target.active_tab_index].name, "runner",
        "target active tab should be the moved tab"
    );

    // Active workspace followed the move.
    assert_eq!(
        restored.workspaces[restored.active_workspace_index].name, "target",
        "active workspace should be the move target"
    );

    // Pane tree, cwd, and discovery metadata survived the round trip.
    let runner = target
        .tabs
        .iter()
        .find(|t| t.name == "runner")
        .expect("runner tab should persist");
    assert_eq!(
        runner.work_origin.as_deref(),
        Some("tab-runtime-smoke-runner")
    );
    assert_eq!(runner.cwd.as_deref(), Some("/tmp/user/project"));
    assert_eq!(runner.discovery_cwd.as_deref(), Some("/tmp/user/project"));
    match runner
        .panes
        .as_ref()
        .expect("moved tab should persist a pane tree")
    {
        SavedPaneNode::Leaf {
            cwd, tmux_session, ..
        } => {
            assert_eq!(cwd.as_deref(), Some("/tmp/user/project"));
            assert_eq!(tmux_session.as_deref(), Some("kmux60"));
        }
        other => panic!("expected a leaf pane for the moved tab, got {other:?}"),
    }
}

// ── EXAMPLE-84: lazy session restore ──────────────────────────────────────────

/// A lazily-restored (not-yet-spawned) tab must serialize its stashed layout
/// verbatim so save/load round-trips splits, tmux/host metadata, ssh commands,
/// and the tab cwd without ever building or spawning the panes.
#[test]
fn test_lazy_restore_round_trips_unspawned_tab_layout() {
    let current_task = taarof_app::task_binding::PaneTaskBinding {
        task_id: "EXAMPLE-110".into(),
        title: "Bind lazy panes".into(),
        checkout_root: Some("/repo".into()),
        reporting_token: "ctx-11111111111111111111111111111111".into(),
    };
    let saved = SavedPaneNode::Split {
        direction: "horizontal".into(),
        ratio: 0.4,
        first: Box::new(SavedPaneNode::Leaf {
            work_origin: Some("pane-runtime-smoke-lazy-first".into()),
            cwd: Some("/tmp/user/proj".into()),
            ssh_command: None,
            tmux_session: Some("taarof--lazy--0".into()),
            tmux_host: Some("user@devbox".into()),
            tmux_identity: None,
            current_task: Some(current_task.clone()),
            agent_session: None,
        }),
        second: Box::new(SavedPaneNode::Leaf {
            work_origin: Some("pane-runtime-smoke-lazy-second".into()),
            cwd: None,
            ssh_command: Some(vec!["ssh".into(), "user@devbox".into()]),
            tmux_session: None,
            tmux_host: None,
            tmux_identity: None,
            current_task: Some(current_task.clone()),
            agent_session: None,
        }),
    };

    let mut state = AppState::new();
    let workspace_id = state.active_workspace;
    let _tab_id = seed_pending_restore_tab(
        &mut state,
        workspace_id,
        "lazy",
        saved,
        Some("/tmp/user/proj".into()),
    )
    .expect("pending restore tab should seed");

    // Snapshot -> serialize -> deserialize, exercising the autosave path.
    let snapshot = snapshot_session_state_for_test(&state);
    let json = serde_json::to_string(&snapshot).expect("session should serialize");
    let restored: SessionStateV2 = serde_json::from_str(&json).expect("session should deserialize");

    let ws = &restored.workspaces[0];
    let tab = ws
        .tabs
        .iter()
        .find(|t| t.name == "lazy")
        .expect("lazy tab should persist");
    assert_eq!(tab.cwd.as_deref(), Some("/tmp/user/proj"));

    match tab
        .panes
        .as_ref()
        .expect("pending tab should persist its saved pane tree")
    {
        SavedPaneNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            assert_eq!(direction, "horizontal");
            assert!((ratio - 0.4).abs() < 1e-9, "split ratio should round-trip");
            match first.as_ref() {
                SavedPaneNode::Leaf {
                    cwd,
                    tmux_session,
                    tmux_host,
                    current_task,
                    ..
                } => {
                    assert_eq!(cwd.as_deref(), Some("/tmp/user/proj"));
                    assert_eq!(tmux_session.as_deref(), Some("taarof--lazy--0"));
                    assert_eq!(tmux_host.as_deref(), Some("user@devbox"));
                    assert_eq!(
                        current_task.as_ref().map(|task| task.task_id.as_str()),
                        Some("EXAMPLE-110")
                    );
                }
                other => panic!("expected tmux-backed first leaf, got {other:?}"),
            }
            match second.as_ref() {
                SavedPaneNode::Leaf {
                    ssh_command,
                    current_task,
                    ..
                } => {
                    assert_eq!(
                        ssh_command.clone(),
                        Some(vec!["ssh".to_string(), "user@devbox".to_string()])
                    );
                    assert_eq!(
                        current_task.as_ref().map(|task| task.title.as_str()),
                        Some("Bind lazy panes")
                    );
                }
                other => panic!("expected ssh second leaf, got {other:?}"),
            }
        }
        other => panic!("expected a split pane tree, got {other:?}"),
    }
}

/// On first activation a lazily-restored tab must spawn its full pane tree. The
/// GTK-free planner proves every leaf gets a distinct pane id and the correct
/// tmux/ssh spawn descriptor (the same work the GTK builder does on activation).
#[test]
fn test_lazy_restore_spawns_pane_tree_on_first_activation() {
    let current_task = taarof_app::task_binding::PaneTaskBinding {
        task_id: "EXAMPLE-110".into(),
        title: "Bind lazy panes".into(),
        checkout_root: Some("/repo".into()),
        reporting_token: "ctx-22222222222222222222222222222222".into(),
    };
    let saved = SavedPaneNode::Split {
        direction: "vertical".into(),
        ratio: 0.5,
        first: Box::new(SavedPaneNode::Leaf {
            work_origin: Some("pane-runtime-smoke-bind-first".into()),
            cwd: None,
            ssh_command: None,
            tmux_session: Some("sess-a".into()),
            tmux_host: None,
            tmux_identity: Some(SavedTmuxIdentity {
                session_id: "$1".into(),
                session_created: 1_711_720_000,
                continuity_id: "11111111111111111111111111111111".into(),
            }),
            current_task: Some(current_task.clone()),
            agent_session: None,
        }),
        second: Box::new(SavedPaneNode::Leaf {
            work_origin: Some("pane-runtime-smoke-bind-second".into()),
            cwd: Some("/tmp/user/remote".into()),
            ssh_command: Some(vec!["ssh".into(), "user@host".into()]),
            tmux_session: None,
            tmux_host: None,
            tmux_identity: None,
            current_task: Some(current_task),
            agent_session: None,
        }),
    };

    let plan: Vec<PlannedRestoreSpawn> = plan_restored_spawns(&saved, false);
    assert_eq!(plan.len(), 2, "both leaves should be planned for spawn");
    assert_eq!(plan[0].pane_id, 0);
    assert_eq!(plan[1].pane_id, 1);
    assert_ne!(
        plan[0].pane_id, plan[1].pane_id,
        "pane ids must be distinct"
    );
    assert_eq!(
        plan[0]
            .current_task
            .as_ref()
            .map(|task| task.task_id.as_str()),
        Some("EXAMPLE-110")
    );
    assert_eq!(
        plan[1]
            .current_task
            .as_ref()
            .map(|task| task.title.as_str()),
        Some("Bind lazy panes")
    );

    // First leaf reattaches its tmux session (cwd handled by tmux itself).
    assert_eq!(plan[0].tmux_session.as_deref(), Some("sess-a"));
    assert!(
        plan[0]
            .spawn_cmd
            .as_ref()
            .is_some_and(|argv| argv.iter().any(|arg| arg == "sess-a")),
        "tmux leaf should spawn an attach command for its session: {:?}",
        plan[0].spawn_cmd
    );

    // Second leaf is a plain SSH connection to the saved host.
    assert!(plan[1].tmux_session.is_none());
    let ssh_argv = plan[1]
        .spawn_cmd
        .as_ref()
        .expect("ssh leaf should spawn a command");
    assert!(
        ssh_argv.iter().any(|arg| arg == "user@host"),
        "ssh spawn must target the saved host: {ssh_argv:?}"
    );
}

// ── EXAMPLE-86: send-to-pane payload construction ───────────────────────────────
//
// These exercise the pure payload core that both the socket `send-keys` path and
// the in-app "send to pane" picker deliver. They assert the two behaviors the UI
// promises: an Insert delivers the source once with NO trailing newline (text
// lands in the target composer without executing), while "send and run" appends
// exactly one newline.

#[test]
fn test_send_to_pane_delivers_payload_once_without_newline() {
    use taarof_app::{build_send_to_pane_payload, SendToPaneMode};

    // Selection wins over clipboard, so only ONE source is delivered (once), and
    // Insert mode never appends a newline.
    let payload =
        build_send_to_pane_payload(Some("echo hi"), Some("ignored"), SendToPaneMode::Insert)
            .expect("a non-empty selection must produce a payload");
    assert_eq!(payload, b"echo hi");
    assert!(
        !payload.ends_with(b"\n"),
        "Insert mode must not append a trailing newline"
    );

    // Clipboard is the fallback only when there is no selection.
    assert_eq!(
        build_send_to_pane_payload(None, Some("clip"), SendToPaneMode::Insert).unwrap(),
        b"clip"
    );

    // A trailing newline on the source is trimmed in Insert mode.
    assert_eq!(
        build_send_to_pane_payload(Some("line\n"), None, SendToPaneMode::Insert).unwrap(),
        b"line"
    );

    // No usable text yields nothing to send.
    assert!(build_send_to_pane_payload(None, None, SendToPaneMode::Insert).is_none());
    assert!(build_send_to_pane_payload(Some(""), Some(""), SendToPaneMode::Insert).is_none());
}

#[test]
fn test_send_and_run_appends_single_newline() {
    use taarof_app::{build_send_to_pane_payload, SendToPaneMode};

    // "send and run" appends exactly one newline so the target executes the line.
    assert_eq!(
        build_send_to_pane_payload(Some("echo hi"), None, SendToPaneMode::Run).unwrap(),
        b"echo hi\n"
    );

    // A source that already ends in a newline is normalized to exactly one.
    assert_eq!(
        build_send_to_pane_payload(Some("echo hi\n"), None, SendToPaneMode::Run).unwrap(),
        b"echo hi\n"
    );

    // Multiple trailing newlines collapse to exactly one.
    assert_eq!(
        build_send_to_pane_payload(Some("echo hi\n\n\n"), None, SendToPaneMode::Run).unwrap(),
        b"echo hi\n"
    );
}

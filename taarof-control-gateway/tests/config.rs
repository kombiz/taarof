//! Configuration and startup-validation tests.
//!
//! These exercise the three rejections the gateway must never start without
//! honoring — non-loopback bind, missing runtime identity, and an unavailable
//! token registry — plus strict parsing, positive registry resolution, and a
//! real loopback bind.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use taarof_control_gateway::config::GatewayConfig;
use taarof_control_gateway::error::GatewayError;

fn config_toml(bind: &str, registry_path: &str) -> String {
    format!(
        r#"
[gateway]
bind_address = "{bind}"

[runtime]
session_name = "primary"
instance_id = "01JABCDEF"
registry_path = "{registry_path}"

[database]
path = "/nonexistent/gateway.db"
"#
    )
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("taarof-gw-test-{}-{nanos}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn rejects_non_loopback_bind() {
    let toml = config_toml("0.0.0.0:8710", "/nonexistent/taarof-current.json");
    let err = GatewayConfig::from_toml_str(&toml).unwrap_err();
    assert!(
        matches!(err, GatewayError::NonLoopbackBind(_)),
        "expected NonLoopbackBind, got {err:?}"
    );
}

#[test]
fn accepts_loopback_binds() {
    for bind in ["127.0.0.1:8710", "[::1]:8710", "127.0.0.5:1"] {
        let toml = config_toml(bind, "/some/registry.json");
        GatewayConfig::from_toml_str(&toml)
            .unwrap_or_else(|e| panic!("loopback {bind} should be accepted, got {e:?}"));
    }
}

#[test]
fn rejects_missing_session_name() {
    let toml = r#"
[gateway]
bind_address = "127.0.0.1:8710"

[runtime]
instance_id = "01JABCDEF"
registry_path = "/some/registry.json"

[database]
path = "/nonexistent/gateway.db"
"#;
    let err = GatewayConfig::from_toml_str(toml).unwrap_err();
    assert!(
        matches!(err, GatewayError::MissingRuntimeIdentity("session_name")),
        "expected MissingRuntimeIdentity(session_name), got {err:?}"
    );
}

#[test]
fn rejects_empty_instance_id() {
    let toml = r#"
[gateway]
bind_address = "127.0.0.1:8710"

[runtime]
session_name = "primary"
instance_id = "   "
registry_path = "/some/registry.json"

[database]
path = "/nonexistent/gateway.db"
"#;
    let err = GatewayConfig::from_toml_str(toml).unwrap_err();
    assert!(
        matches!(err, GatewayError::MissingRuntimeIdentity("instance_id")),
        "expected MissingRuntimeIdentity(instance_id), got {err:?}"
    );
}

#[test]
fn rejects_unknown_field() {
    let toml = r#"
[gateway]
bind_address = "127.0.0.1:8710"
surprise = true

[runtime]
session_name = "primary"
instance_id = "01JABCDEF"
registry_path = "/some/registry.json"

[database]
path = "/nonexistent/gateway.db"
"#;
    let err = GatewayConfig::from_toml_str(toml).unwrap_err();
    assert!(
        matches!(err, GatewayError::ParseConfig(_)),
        "expected ParseConfig for unknown field, got {err:?}"
    );
}

#[test]
fn resolve_runtime_missing_registry_is_unavailable() {
    let toml = config_toml("127.0.0.1:8710", "/nonexistent/dir/taarof-current.json");
    let config = GatewayConfig::from_toml_str(&toml).unwrap();
    let err = config.resolve_runtime().unwrap_err();
    assert!(
        matches!(err, GatewayError::TokenRegistryUnavailable { .. }),
        "expected TokenRegistryUnavailable, got {err:?}"
    );
}

#[test]
fn resolve_runtime_reads_pid_and_token_path() {
    let dir = unique_temp_dir();
    let registry = dir.join("taarof-current.json");
    std::fs::write(
        &registry,
        r#"{"pid":4242,"socket_path":"/run/user/1000/taarof-4242.sock"}"#,
    )
    .unwrap();

    let toml = config_toml("127.0.0.1:8710", registry.to_str().unwrap());
    let config = GatewayConfig::from_toml_str(&toml).unwrap();
    let resolved = config.resolve_runtime().unwrap();

    assert_eq!(resolved.pid(), 4242);
    assert_eq!(
        resolved.socket_path(),
        Path::new("/run/user/1000/taarof-4242.sock")
    );
    assert_eq!(
        resolved.bearer_token_path(),
        dir.join("taarof-http-4242.token")
    );
}

#[test]
fn resolve_runtime_malformed_registry_errors() {
    let dir = unique_temp_dir();
    let registry = dir.join("taarof-current.json");
    std::fs::write(&registry, "not json at all").unwrap();

    let toml = config_toml("127.0.0.1:8710", registry.to_str().unwrap());
    let config = GatewayConfig::from_toml_str(&toml).unwrap();
    let err = config.resolve_runtime().unwrap_err();
    assert!(
        matches!(err, GatewayError::MalformedRegistry { .. }),
        "expected MalformedRegistry, got {err:?}"
    );
}

#[tokio::test]
async fn binds_loopback_listener() {
    let toml = config_toml("127.0.0.1:0", "/some/registry.json");
    let config = GatewayConfig::from_toml_str(&toml).unwrap();
    let listener = taarof_control_gateway::bind_listener(&config)
        .await
        .expect("loopback listener should bind");
    let local = listener.local_addr().unwrap();
    assert!(local.ip().is_loopback(), "bound non-loopback {local}");
}

#[test]
fn runtime_http_endpoint_defaults_to_loopback_and_rejects_remote_targets() {
    let text = config_toml("127.0.0.1:8710", "/inert/registry.json");
    let config = GatewayConfig::from_toml_str(&text).unwrap();
    assert_eq!(config.runtime.http_address.to_string(), "127.0.0.1:7800");
    let remote = text.replace("[runtime]", "[runtime]\nhttp_address = \"192.0.2.1:7800\"");
    assert!(matches!(
        GatewayConfig::from_toml_str(&remote),
        Err(GatewayError::NonLoopbackBind(_))
    ));
}

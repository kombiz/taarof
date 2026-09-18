//! Startup-wiring tests.
//!
//! Carry-over from Task 9: production startup must build the pairing manager on
//! the durable SQLite device store, not the no-op in-memory store. If it used
//! the memory store, a device persisted by a prior run would be invisible after
//! restart — so a device written to disk must be visible through a freshly built
//! manager.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use taarof_control_gateway::build_pairing_manager;
use taarof_control_gateway::db::{self, StoredDevice};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_db_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "taarof-gw-startup-{}-{nanos}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("gateway.db")
}

#[test]
fn startup_pairing_manager_loads_devices_from_the_sqlite_store() {
    let db_path = unique_db_path();

    // Simulate a device confirmed by a prior run: write it straight to the
    // gateway database that startup will open.
    {
        let conn = db::open(&db_path).expect("open db");
        db::insert_device(
            &conn,
            &StoredDevice {
                device_uuid: "device-uuid-persisted".to_string(),
                display_name: "Persisted Pixel".to_string(),
                observe_public_key: vec![7u8; 32],
                control_public_key: vec![8u8; 32],
                attestation_facts: None,
                security_level: Some("strongbox".to_string()),
                created_at_ms: 1_600_000_000_000,
                revoked: false,
            },
        )
        .expect("insert device");
    }

    // Startup builds the manager the same way `main` does.
    let manager = build_pairing_manager(&db_path).expect("build pairing manager");

    // A memory-backed manager would report zero here; the SQLite store loads the
    // persisted device.
    assert_eq!(manager.device_count(), 1, "device must survive a restart");
    assert!(
        manager.is_authorized_name("Persisted Pixel"),
        "the persisted device must be authorized after a fresh startup"
    );
}

#[test]
fn startup_pairing_manager_reflects_revocation_persisted_on_disk() {
    let db_path = unique_db_path();
    {
        let conn = db::open(&db_path).expect("open db");
        db::insert_device(
            &conn,
            &StoredDevice {
                device_uuid: "device-uuid-revoked".to_string(),
                display_name: "Revoked Device".to_string(),
                observe_public_key: vec![1u8; 32],
                control_public_key: vec![2u8; 32],
                attestation_facts: None,
                security_level: None,
                created_at_ms: 1_600_000_000_000,
                revoked: false,
            },
        )
        .expect("insert device");
        db::set_device_revoked(&conn, "device-uuid-revoked", 1_600_000_100_000)
            .expect("revoke device");
    }

    let manager = build_pairing_manager(&db_path).expect("build pairing manager");
    assert_eq!(
        manager.device_count(),
        0,
        "a revoked device is not authorized"
    );
    assert!(!manager.is_authorized("device-uuid-revoked"));
}

/// Exercise the shipped binary against an inert authenticated runtime endpoint.
/// Rotation must require an explicitly edited pin, with no listener or DB opened.
#[tokio::test]
async fn runtime_rotation_requires_explicit_repin_and_check_only_has_no_side_effects() {
    use axum::{extract::State, http::HeaderMap, routing::get, Json, Router};
    use std::sync::{Arc, RwLock};
    let identity = Arc::new(RwLock::new(("first".to_owned(), "default".to_owned())));
    let app = Router::new().route("/api/v1/runtime-identity", get(
        |State(identity): State<Arc<RwLock<(String, String)>>>, headers: HeaderMap| async move {
            assert_eq!(headers["authorization"], "Bearer inert-test-token");
            let identity = identity.read().unwrap();
            Json(serde_json::json!({"ok":true,"data":{
                "schema":"taarof.runtime-identity.v1", "runtime_id": identity.0,
                "session_name": identity.1
            }}))
        }
    )).with_state(identity.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let db = unique_db_path();
    let dir = db.parent().unwrap();
    let registry = dir.join("registry.json");
    std::fs::write(&registry, r#"{"pid":4242,"socket_path":"/inert/socket"}"#).unwrap();
    std::fs::write(dir.join("taarof-http-4242.token"), "inert-test-token").unwrap();
    let config = dir.join("gateway.toml");
    let write_config = |pin: &str| {
        let text = format!(
            r#"
[gateway]
bind_address = "127.0.0.1:0"
[runtime]
instance_id = "{pin}"
session_name = "default"
registry_path = "{}"
http_address = "{address}"
[database]
path = "{}"
"#,
            registry.display(),
            db.display()
        );
        std::fs::write(&config, &text).unwrap();
        text
    };
    async fn check(config: PathBuf, check_only: bool) -> std::process::Output {
        tokio::task::spawn_blocking(move || {
            let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_taarof-control-gateway"));
            cmd.env("TAAROF_GATEWAY_CONFIG", config);
            if check_only {
                cmd.arg("--check-runtime");
            }
            cmd.output().unwrap()
        })
        .await
        .unwrap()
    }
    let original = write_config("first");
    assert!(check(config.clone(), true).await.status.success());
    identity.write().unwrap().0 = "second".into();
    for check_only in [true, false] {
        let result = check(config.clone(), check_only).await;
        assert_eq!(result.status.code(), Some(78));
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains("ACTION REQUIRED"));
        assert!(!stderr.contains("inert-test-token"));
        assert!(!stderr.contains("second"));
    }
    assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
    write_config("second");
    assert!(check(config.clone(), true).await.status.success());
    // Session mismatches are equally fatal even when the instance matches.
    identity.write().unwrap().1 = "other-session".into();
    assert_eq!(check(config, true).await.status.code(), Some(78));
    assert!(!db.exists(), "preflight must not open the database");
    server.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn packaged_user_service_bounds_all_retries_and_never_restarts_a_stale_pin() {
    let unit = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../packaging/linux/systemd/taarof-control-gateway.service"),
    )
    .expect("packaged gateway user unit should be present");

    for directive in [
        "Restart=on-failure",
        "RestartPreventExitStatus=78",
        "StartLimitIntervalSec=60",
        "StartLimitBurst=3",
    ] {
        assert!(
            unit.lines().any(|line| line == directive),
            "missing required restart bound: {directive}"
        );
    }
}

#[test]
fn build_info_is_available_without_runtime_configuration() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_taarof-control-gateway"))
        .arg("--build-info")
        .env_remove("TAAROF_GATEWAY_CONFIG")
        .output()
        .expect("gateway build-info command should run");
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema"], "taarof.gateway-build.v1");
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert!(value["build_id"].as_str().is_some_and(|id| !id.is_empty()));
}

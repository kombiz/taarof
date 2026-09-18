//! Binary entry point for the loopback control gateway.
//!
//! Startup fails closed: an invalid configuration, an unreachable taarof
//! runtime registry, a database that cannot be opened, or a non-loopback bind
//! all abort before the listener accepts a connection.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::{fs, os::unix::fs::PermissionsExt};

use taarof_control_gateway::config::GatewayConfig;
use taarof_control_gateway::error::GatewayError;
use taarof_control_gateway::runtime::{HttpRuntimeAdapter, RuntimeIdentity};
use taarof_control_gateway::server::{owner_router, public_router, GatewayServerState};
use taarof_control_gateway::{bind_listener, build_audit_sink, build_pairing_manager, db};

/// Environment variable naming the gateway configuration file.
const CONFIG_ENV: &str = "TAAROF_GATEWAY_CONFIG";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(GatewayError::RuntimeIdentityMismatch { .. }) => {
            eprintln!("taarof-control-gateway: ACTION REQUIRED: stale runtime pin; validate the intended runtime and update gateway.toml, then run --check-runtime before restarting the service");
            ExitCode::from(78)
        }
        Err(e) => {
            eprintln!("taarof-control-gateway: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), GatewayError> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && args[0] == "--build-info" {
        println!("{}", build_info());
        return Ok(());
    }
    let check_only = args.len() == 1 && args[0] == "--check-runtime";
    if !args.is_empty() && !check_only {
        return Err(GatewayError::RuntimeProtocol(
            "usage: taarof-control-gateway [--check-runtime|--build-info]".into(),
        ));
    }
    let config_path = config_path();
    let config = GatewayConfig::from_path(&config_path)?;

    // Fail closed if the runtime registry that supplies the loopback socket and
    // the process-scoped bearer token is unavailable.
    let runtime = config.resolve_runtime()?;
    HttpRuntimeAdapter::new(
        config.runtime.http_address.to_string(),
        runtime,
        RuntimeIdentity::new(config.instance_id(), config.session_name()),
    )
    .verify_identity()
    .await?;
    if check_only {
        eprintln!(
            "taarof-control-gateway: configured runtime pin verified; configuration unchanged"
        );
        return Ok(());
    }

    // Open and migrate the database before accepting connections.
    let _db = db::open(&config.database.path)?;

    // Wire the durable device layer: the pairing manager MUST be backed by the
    // on-disk SQLite store, not the no-op in-memory one, or every restart would
    // silently forget paired devices. Loading it here also fails closed if the
    // store is unreadable.
    let pairing = build_pairing_manager(&config.database.path)?;
    let _audit = build_audit_sink(&config.database.path, config.instance_id())?;
    eprintln!(
        "taarof-control-gateway: loaded {} paired device(s) from the SQLite store",
        pairing.device_count()
    );

    let listener = bind_listener(&config).await?;
    eprintln!(
        "taarof-control-gateway: listening on loopback {}",
        config.bind_address()
    );

    let identity = RuntimeIdentity::new(config.instance_id(), config.session_name());
    let state = Arc::new(GatewayServerState::with_pairing(identity, pairing));
    let app = public_router(state.clone());
    let owner_socket_path = owner_socket_path()?;
    if owner_socket_path.exists() {
        fs::remove_file(&owner_socket_path)?;
    }
    let owner_listener = tokio::net::UnixListener::bind(&owner_socket_path)?;
    fs::set_permissions(&owner_socket_path, fs::Permissions::from_mode(0o600))?;
    eprintln!(
        "taarof-control-gateway: owner channel ready at {}",
        owner_socket_path.display()
    );
    let owner_app = owner_router(state);
    tokio::try_join!(
        axum::serve(listener, app),
        axum::serve(owner_listener, owner_app)
    )
    .map_err(GatewayError::Io)?;
    Ok(())
}

fn build_info() -> serde_json::Value {
    let nonempty = |value: &str| (!value.is_empty()).then(|| value.to_owned());
    serde_json::json!({
        "schema": "taarof.gateway-build.v1",
        "version": env!("CARGO_PKG_VERSION"),
        "build_id": nonempty(env!("TAAROF_BUILD_ID")),
        "source_revision": nonempty(env!("TAAROF_BUILD_SOURCE_REVISION")),
        "source_dirty": match env!("TAAROF_BUILD_SOURCE_DIRTY") {
            "true" => Some(true), "false" => Some(false), _ => None,
        },
        "profile": nonempty(env!("TAAROF_BUILD_PROFILE")),
        "built_at_unix": nonempty(env!("TAAROF_BUILD_SOURCE_BUILT_AT_UNIX")),
    })
}

fn owner_socket_path() -> Result<PathBuf, GatewayError> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| GatewayError::Io(std::io::Error::other("XDG_RUNTIME_DIR is not set")))?;
    let directory = PathBuf::from(runtime_dir).join("taarof-control-gateway");
    fs::create_dir_all(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    Ok(directory.join("owner.sock"))
}

fn config_path() -> PathBuf {
    std::env::var_os(CONFIG_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("gateway.toml"))
}

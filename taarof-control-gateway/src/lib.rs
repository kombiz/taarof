//! Loopback-only control gateway for the Taarof Remote Protocol.
//!
//! This crate is the only remote trust boundary between an authenticated
//! Android device and the desktop taarof runtime. It binds loopback only;
//! tailnet exposure is provided separately by Caddy. This module tree is the
//! scaffold: strict configuration parsing, the fail-closed error type, the
//! SQLite schema and migration runner, the loopback listener, and the runtime
//! adapter boundary. Pairing, attestation, and the concrete runtime adapters
//! are built in later tasks.

pub mod attestation;
pub mod audit;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod grants;
pub mod pairing;
pub mod runtime;
pub mod server;
pub mod terminal;

use std::path::Path;

use tokio::net::TcpListener;

use crate::audit::SqliteAuditSink;
use crate::config::GatewayConfig;
use crate::error::GatewayError;
use crate::pairing::{PairingManager, SqliteDeviceStore};

/// Bind the loopback TCP listener described by `config`.
///
/// Defense in depth: `GatewayConfig::validate` already rejects a non-loopback
/// address at load time, but this refuses to bind one regardless of how the
/// config was constructed.
pub async fn bind_listener(config: &GatewayConfig) -> Result<TcpListener, GatewayError> {
    let addr = config.bind_address();
    if !addr.ip().is_loopback() {
        return Err(GatewayError::NonLoopbackBind(addr));
    }
    TcpListener::bind(addr)
        .await
        .map_err(|source| GatewayError::Bind { addr, source })
}

/// Construct the pairing manager backed by the **durable SQLite device store**,
/// preloaded with every device (and its revocation state) already on disk.
///
/// Production startup must call this rather than [`PairingManager::new`]: the
/// default constructor uses the no-op in-memory store, which would silently
/// discard Task 9's persisted pairings on every restart. Returning a manager
/// whose `device_count` reflects the on-disk rows is the observable proof the
/// SQLite store is in use.
pub fn build_pairing_manager(db_path: &Path) -> Result<PairingManager, GatewayError> {
    let store = SqliteDeviceStore::open(db_path)?;
    PairingManager::with_store(Box::new(store)).map_err(|e| {
        // A store-load failure at startup fails closed, like every other
        // startup error, rather than degrading to an empty in-memory store.
        GatewayError::Io(std::io::Error::other(e.to_string()))
    })
}

/// Construct the metadata-only audit sink for the pinned runtime, sharing the
/// gateway database. Every audited action is stamped with `instance_id`.
pub fn build_audit_sink(
    db_path: &Path,
    instance_id: &str,
) -> Result<SqliteAuditSink, GatewayError> {
    SqliteAuditSink::open(db_path, instance_id)
}

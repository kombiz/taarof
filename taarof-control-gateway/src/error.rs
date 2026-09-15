//! The gateway's single error type.
//!
//! Startup fails closed: an invalid configuration, an unreachable taarof
//! runtime registry, or a database that cannot be opened all surface here and
//! abort the process rather than degrading to an insecure fallback.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

/// Errors produced while configuring, preparing, or binding the gateway.
#[derive(Debug)]
pub enum GatewayError {
    /// The configuration text was not valid TOML, or contained unknown fields.
    ParseConfig(toml::de::Error),
    /// The configured listen address is not a loopback address. The gateway is
    /// the only remote trust boundary and must never bind a routable address;
    /// tailnet exposure is Caddy's job, not the gateway's.
    NonLoopbackBind(SocketAddr),
    /// A required runtime-identity field (`session_name` / `instance_id`) was
    /// absent or empty. The gateway must be pinned to exactly one runtime.
    MissingRuntimeIdentity(&'static str),
    /// The taarof runtime registry file could not be read. Without it the
    /// gateway cannot discover the loopback socket or the process-scoped bearer
    /// token, so it refuses to start.
    TokenRegistryUnavailable {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The runtime registry file was present but not the expected JSON shape.
    MalformedRegistry {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// The SQLite database could not be opened or migrated.
    Database(rusqlite::Error),
    /// Binding the loopback listener failed.
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    /// A supporting filesystem or I/O operation failed.
    Io(std::io::Error),
    /// The live runtime the gateway connected to is not the one it is pinned to.
    /// The gateway refuses to relay to an unexpected runtime — the identity a
    /// client sees must always be the configured `(instance_id, session_name)`.
    RuntimeIdentityMismatch {
        expected_instance: String,
        expected_session: String,
        observed_instance: String,
        observed_session: String,
    },
    /// The runtime could not be reached or did not complete the operation.
    /// Carries a metadata-only reason (never a token, path, PID, or socket).
    RuntimeUnavailable(String),
    /// The runtime spoke something other than the expected protocol frames.
    RuntimeProtocol(String),
}

impl fmt::Display for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GatewayError::ParseConfig(e) => write!(f, "invalid gateway configuration: {e}"),
            GatewayError::NonLoopbackBind(addr) => write!(
                f,
                "refusing to bind non-loopback address {addr}; the gateway must listen on loopback only"
            ),
            GatewayError::MissingRuntimeIdentity(field) => {
                write!(f, "missing runtime identity: `{field}` must be set")
            }
            GatewayError::TokenRegistryUnavailable { path, source } => write!(
                f,
                "taarof runtime registry unavailable at {}: {source}",
                path.display()
            ),
            GatewayError::MalformedRegistry { path, source } => write!(
                f,
                "taarof runtime registry at {} is malformed: {source}",
                path.display()
            ),
            GatewayError::Database(e) => write!(f, "database error: {e}"),
            GatewayError::Bind { addr, source } => {
                write!(f, "could not bind loopback listener on {addr}: {source}")
            }
            GatewayError::Io(e) => write!(f, "i/o error: {e}"),
            GatewayError::RuntimeIdentityMismatch {
                expected_instance,
                expected_session,
                observed_instance,
                observed_session,
            } => write!(
                f,
                "refusing to relay: live runtime ({observed_instance}/{observed_session}) is not \
                 the configured runtime ({expected_instance}/{expected_session})"
            ),
            GatewayError::RuntimeUnavailable(reason) => {
                write!(f, "runtime unavailable: {reason}")
            }
            GatewayError::RuntimeProtocol(reason) => {
                write!(f, "runtime protocol error: {reason}")
            }
        }
    }
}

impl std::error::Error for GatewayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GatewayError::ParseConfig(e) => Some(e),
            GatewayError::TokenRegistryUnavailable { source, .. } => Some(source),
            GatewayError::MalformedRegistry { source, .. } => Some(source),
            GatewayError::Database(e) => Some(e),
            GatewayError::Bind { source, .. } => Some(source),
            GatewayError::Io(e) => Some(e),
            GatewayError::NonLoopbackBind(_)
            | GatewayError::MissingRuntimeIdentity(_)
            | GatewayError::RuntimeIdentityMismatch { .. }
            | GatewayError::RuntimeUnavailable(_)
            | GatewayError::RuntimeProtocol(_) => None,
        }
    }
}

impl From<rusqlite::Error> for GatewayError {
    fn from(e: rusqlite::Error) -> Self {
        GatewayError::Database(e)
    }
}

impl From<std::io::Error> for GatewayError {
    fn from(e: std::io::Error) -> Self {
        GatewayError::Io(e)
    }
}

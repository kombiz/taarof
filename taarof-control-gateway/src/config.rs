//! Gateway configuration parsing and validation.
//!
//! Parsing is strict: unknown fields and malformed values are hard errors, not
//! warnings. Validation enforces the two invariants the gateway cannot start
//! without — a loopback-only listen address and a pinned runtime identity — and
//! resolves (but never persists) the taarof runtime registry that supplies the
//! loopback socket and the process-scoped bearer token.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::GatewayError;

/// Top-level gateway configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    pub gateway: ListenerConfig,
    pub runtime: RuntimeConfig,
    pub database: DatabaseConfig,
}

/// Listener settings. The gateway binds loopback only.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerConfig {
    /// Loopback socket address, e.g. `127.0.0.1:8710` or `[::1]:8710`.
    pub bind_address: SocketAddr,
}

/// Identity of the single taarof runtime this gateway is allowed to serve.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    /// The named taarof session this gateway is pinned to.
    pub session_name: Option<String>,
    /// The stable runtime instance ID this gateway is pinned to. The gateway
    /// fails closed if a different runtime ever appears.
    pub instance_id: Option<String>,
    /// Path to the taarof runtime registry JSON (`taarof-current*.json`) that
    /// advertises the loopback socket path and owning PID.
    pub registry_path: PathBuf,
    /// Explicit loopback runtime HTTP endpoint; never inferred from a remote peer.
    #[serde(default = "default_http_address")]
    pub http_address: SocketAddr,
}

/// Database settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Filesystem path to the gateway's SQLite database.
    pub path: PathBuf,
}

fn default_http_address() -> SocketAddr {
    "127.0.0.1:7800".parse().expect("literal address")
}

impl GatewayConfig {
    /// Parse and validate configuration from a TOML string.
    pub fn from_toml_str(text: &str) -> Result<Self, GatewayError> {
        let config: GatewayConfig = toml::from_str(text).map_err(GatewayError::ParseConfig)?;
        config.validate()?;
        Ok(config)
    }

    /// Read, parse, and validate configuration from a file.
    pub fn from_path(path: &Path) -> Result<Self, GatewayError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_toml_str(&text)
    }

    /// Enforce the invariants required to start: loopback-only bind and a
    /// present, non-empty runtime identity.
    pub fn validate(&self) -> Result<(), GatewayError> {
        let addr = self.gateway.bind_address;
        if !addr.ip().is_loopback() {
            return Err(GatewayError::NonLoopbackBind(addr));
        }
        if !self.runtime.http_address.ip().is_loopback() {
            return Err(GatewayError::NonLoopbackBind(self.runtime.http_address));
        }
        require_identity("session_name", self.runtime.session_name.as_deref())?;
        require_identity("instance_id", self.runtime.instance_id.as_deref())?;
        Ok(())
    }

    /// The validated loopback listen address.
    pub fn bind_address(&self) -> SocketAddr {
        self.gateway.bind_address
    }

    /// The pinned session name (empty only before validation).
    pub fn session_name(&self) -> &str {
        self.runtime.session_name.as_deref().unwrap_or_default()
    }

    /// The pinned runtime instance ID (empty only before validation).
    pub fn instance_id(&self) -> &str {
        self.runtime.instance_id.as_deref().unwrap_or_default()
    }

    /// Resolve the configured taarof runtime registry into the loopback socket
    /// and owning PID. Fails closed if the registry is missing or malformed.
    ///
    /// The returned handle exposes the bearer-token path but not the token
    /// itself; the token is read only at the moment of use and never persisted.
    pub fn resolve_runtime(&self) -> Result<ResolvedRuntime, GatewayError> {
        let path = &self.runtime.registry_path;
        let text = std::fs::read_to_string(path).map_err(|source| {
            GatewayError::TokenRegistryUnavailable {
                path: path.clone(),
                source,
            }
        })?;
        let registry: RuntimeRegistryFile =
            serde_json::from_str(&text).map_err(|source| GatewayError::MalformedRegistry {
                path: path.clone(),
                source,
            })?;
        let runtime_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(ResolvedRuntime {
            pid: registry.pid,
            socket_path: PathBuf::from(registry.socket_path),
            runtime_dir,
        })
    }
}

/// Reject an absent or blank runtime-identity field.
fn require_identity(field: &'static str, value: Option<&str>) -> Result<(), GatewayError> {
    match value {
        Some(v) if !v.trim().is_empty() => Ok(()),
        _ => Err(GatewayError::MissingRuntimeIdentity(field)),
    }
}

/// On-disk shape of the taarof runtime registry (`taarof-current*.json`).
#[derive(Debug, Deserialize)]
struct RuntimeRegistryFile {
    pid: u32,
    socket_path: String,
}

/// A resolved taarof runtime: the loopback socket and the owning PID, plus the
/// directory in which the process-scoped bearer token lives.
///
/// None of these values may be written to the database or returned to a remote
/// client.
#[derive(Debug, Clone)]
pub struct ResolvedRuntime {
    pid: u32,
    socket_path: PathBuf,
    runtime_dir: PathBuf,
}

impl ResolvedRuntime {
    /// PID of the taarof process that owns the runtime socket.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Loopback Unix socket path advertised by the runtime.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Path to the process-scoped bearer token file. The token is read at use;
    /// this only names the file.
    pub fn bearer_token_path(&self) -> PathBuf {
        self.runtime_dir
            .join(format!("taarof-http-{}.token", self.pid))
    }

    /// Read the process-scoped bearer token at the moment of use. The value is
    /// returned to the caller and must never be persisted or logged.
    pub fn read_bearer_token(&self) -> Result<String, GatewayError> {
        let token = std::fs::read_to_string(self.bearer_token_path())?;
        Ok(token.trim().to_string())
    }
}

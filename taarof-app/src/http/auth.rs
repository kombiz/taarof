//! Bearer-token authentication helpers and token file management.

use super::*;

// ── Auth ──

pub(super) fn authorization_bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

pub(super) fn check_auth(headers: &HeaderMap, expected: &str) -> Result<(), StatusCode> {
    match authorization_bearer_token(headers) {
        Some(t) if bool::from(t.as_bytes().ct_eq(expected.as_bytes())) => Ok(()),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

pub(super) fn check_ws_auth(
    headers: &HeaderMap,
    query_token: Option<&str>,
    expected: &str,
) -> Result<(), StatusCode> {
    let header_matches = authorization_bearer_token(headers)
        .map(|token| bool::from(token.as_bytes().ct_eq(expected.as_bytes())))
        .unwrap_or(false);
    let query_matches = query_token
        .map(|token| bool::from(token.as_bytes().ct_eq(expected.as_bytes())))
        .unwrap_or(false);

    if header_matches || query_matches {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

// ── Token generation and persistence ──

pub(super) fn generate_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; HTTP_TOKEN_BYTES];
    getrandom::getrandom(&mut bytes)?;

    let mut token = String::with_capacity(HTTP_TOKEN_BYTES * 2);
    for byte in bytes {
        let _ = write!(&mut token, "{byte:02x}");
    }
    Ok(token)
}

pub(super) fn parse_bind_ip(bind_address: &str) -> Result<IpAddr, String> {
    bind_address.trim().parse::<IpAddr>().map_err(|error| {
        format!(
            "invalid [http].bind_address {:?}: {error}; expected an IP literal such as 127.0.0.1 or ::1",
            bind_address
        )
    })
}

pub(super) fn resolve_bind_addr(config: &crate::config::HttpConfig) -> Result<SocketAddr, String> {
    let bind_ip = parse_bind_ip(&config.bind_address)?;
    if !bind_ip.is_loopback() && !config.unsafe_allow_non_loopback {
        return Err(format!(
            "refusing to bind HTTP API to non-loopback address {}; set {} only if you intentionally want to expose the read-only API beyond localhost",
            config.bind_address, HTTP_REMOTE_BIND_OPT_IN
        ));
    }

    Ok(SocketAddr::new(bind_ip, config.port))
}

pub(super) fn http_control_enabled_for_bind(
    config: &crate::config::HttpControlConfig,
    addr: SocketAddr,
) -> bool {
    config.enabled && addr.ip().is_loopback()
}

pub(super) fn warn_on_non_loopback_bind(addr: SocketAddr) {
    if !addr.ip().is_loopback() {
        eprintln!(
            "taarof: WARNING: binding HTTP API to {addr}; same-network clients who obtain the bearer token can read taarof state and event data"
        );
        eprintln!(
            "taarof: WARNING: keep this on trusted networks only and prefer loopback unless you explicitly need remote access"
        );
    }
}

pub fn token_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(format!("taarof-http-{}.token", std::process::id()))
}

pub(super) fn write_token_file(path: &Path, token: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        use std::io::Write;
        f.write_all(token.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, token)
    }
}

pub fn cleanup_token_file(runtime_dir: &Path) {
    let _ = std::fs::remove_file(token_path(runtime_dir));
}

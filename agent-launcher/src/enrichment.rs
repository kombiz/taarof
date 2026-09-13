//! Optional same-user socket enrichment. Failures preserve local discovery.
use agent_session_core::*;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(2);
const MAX_BYTES: usize = 8 * 1024 * 1024;

pub fn storage_key(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return "default".into();
    }
    let mut slug = String::new();
    let mut dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            slug.push(ch);
            dash = false;
        } else if !dash {
            slug.push('-');
            dash = true;
        }
    }
    slug = slug.trim_matches('-').into();
    if slug.is_empty() {
        slug = name.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    }
    slug.truncate(slug.len().min(48));
    let digest: String = Sha256::digest(name.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{slug}-{}", &digest[..32])
}
fn private_directory(path: &Path) -> Result<(), String> {
    let metadata =
        std::fs::metadata(path).map_err(|_| "Taarof runtime directory is unavailable.")?;
    // getuid has no preconditions and exposes no credential value.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(
            "Taarof runtime directory must be owned by this user and private (0700).".into(),
        );
    }
    Ok(())
}
#[derive(Clone, Debug)]
pub struct SocketClient {
    path: PathBuf,
}
impl SocketClient {
    pub fn new(path: PathBuf) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err("Taarof socket path must be absolute.".into());
        }
        private_directory(path.parent().ok_or("Invalid socket path.")?)?;
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|_| "Taarof socket is unavailable; using local history.")?;
        if !metadata.file_type().is_socket()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err("Taarof socket must be a private same-user Unix socket.".into());
        }
        Ok(Self { path })
    }
    pub fn request(&self, request: Value) -> Result<Value, String> {
        // Recheck filesystem authority on every connection, including mutation.
        Self::new(self.path.clone())?;
        let started = Instant::now();
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
            .map_err(|_| "Could not open Taarof socket.")?;
        let address =
            socket2::SockAddr::unix(&self.path).map_err(|_| "Invalid Taarof socket path.")?;
        socket
            .connect_timeout(&address, DEADLINE)
            .map_err(|_| "Taarof is stopped or unresponsive; using local history.")?;
        let descriptor: std::os::fd::OwnedFd = socket.into();
        let mut stream = UnixStream::from(descriptor);
        let mut bytes = serde_json::to_vec(&request).map_err(|_| "Invalid socket request.")?;
        bytes.push(b'\n');
        if bytes.len() > 65536 {
            return Err("Taarof request exceeds size limit.".into());
        }
        let mut pending = bytes.as_slice();
        while !pending.is_empty() {
            let remaining = DEADLINE
                .checked_sub(started.elapsed())
                .filter(|v| !v.is_zero())
                .ok_or("Taarof query timed out.")?;
            stream
                .set_write_timeout(Some(remaining))
                .map_err(|_| "Could not bound socket write.")?;
            let written = stream
                .write(pending)
                .map_err(|_| "Taarof request failed.")?;
            if written == 0 {
                return Err("Taarof closed the socket.".into());
            }
            pending = &pending[written..];
        }
        // The app frames requests by EOF; newline alone does not dispatch them.
        stream
            .shutdown(Shutdown::Write)
            .map_err(|_| "Could not finish Taarof request.")?;
        let mut response = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let remaining = DEADLINE
                .checked_sub(started.elapsed())
                .filter(|v| !v.is_zero())
                .ok_or("Taarof query timed out.")?;
            stream
                .set_read_timeout(Some(remaining))
                .map_err(|_| "Could not bound socket read.")?;
            let read = stream
                .read(&mut buffer)
                .map_err(|_| "Taarof response timed out or failed.")?;
            if read == 0 {
                break;
            }
            if response.len() + read > MAX_BYTES {
                return Err("Taarof catalog exceeds response limit.".into());
            }
            response.extend_from_slice(&buffer[..read]);
            if response.contains(&b'\n') {
                break;
            }
        }
        let wire: Value =
            serde_json::from_slice(&response).map_err(|_| "Taarof returned malformed JSON.")?;
        if wire.get("ok") != Some(&Value::Bool(true)) {
            return Err("Taarof rejected the request; refresh live state.".into());
        }
        Ok(wire.get("data").cloned().unwrap_or(Value::Null))
    }
    pub fn enrich(&self, local: &mut SessionCatalog) -> Result<(), String> {
        let data =
            self.request(json!({"action":"query-agent-sessions","schema":"agent.sessions.v2"}))?;
        #[derive(Deserialize)]
        struct WireCatalog {
            schema: String,
            providers: Vec<AgentSessionProviderStatus>,
            sessions: Vec<SessionRecord>,
            #[serde(default)]
            remote_hosts: Vec<RemoteHostStatus>,
        }
        let wire: WireCatalog = serde_json::from_value(data)
            .map_err(|_| "Taarof returned an unsupported session contract.")?;
        if wire.schema != "agent.sessions.v2" {
            return Err("Taarof session schema is unsupported.".into());
        }
        merge_catalog(
            local,
            SessionCatalog {
                schema: "agent.sessions.v2",
                providers: wire.providers,
                sessions: wire.sessions,
                remote_hosts: wire.remote_hosts,
            },
        )
    }
    pub fn attach(&self, target: &AttachTarget) -> Result<(), String> {
        self.request(
            json!({"action":"attach-session","session_name":target.session_name,
            "ssh_target":target.ssh_target,"expected_agent":target}),
        )?;
        Ok(())
    }
}
pub fn configured_client(session: Option<&str>) -> Result<SocketClient, String> {
    if let Some(path) = std::env::var_os("TAAROF_SOCK") {
        return SocketClient::new(path.into());
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })));
    private_directory(&runtime)?;
    let from_env = std::env::var("TAAROF_SESSION").ok();
    let session = session
        .or(from_env.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let file = session
        .map(|s| format!("taarof-current-{}.json", storage_key(s)))
        .unwrap_or_else(|| "taarof-current.json".into());
    let registry = read_registry(&runtime.join(file))?;
    let path = registry
        .get("socket_path")
        .and_then(Value::as_str)
        .ok_or("Taarof registry has no socket path.")?;
    SocketClient::new(path.into())
}
fn read_registry(path: &Path) -> Result<Value, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "Named Taarof runtime is unavailable; using local history.")?;
    let metadata = file
        .metadata()
        .map_err(|_| "Taarof registry is unreadable.")?;
    if !metadata.is_file() || metadata.len() > 65536 {
        return Err("Taarof registry must be a bounded regular file.".into());
    }
    let mut bytes = Vec::new();
    file.take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| "Taarof registry is unreadable.")?;
    if bytes.len() > 65536 {
        return Err("Taarof registry exceeds size limit.".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "Taarof registry is malformed.".into())
}
pub fn enrich_catalog(catalog: &mut SessionCatalog, session: Option<&str>) -> Result<(), String> {
    configured_client(session)?.enrich(catalog)
}
pub fn merge_catalog(
    local: &mut SessionCatalog,
    mut incoming: SessionCatalog,
) -> Result<(), String> {
    if incoming.sessions.len() > 4000
        || incoming.providers.len() > 128
        || incoming.remote_hosts.len() > 256
    {
        return Err("Taarof catalog exceeds record limits.".into());
    }
    let mut seen = HashSet::new();
    for row in &mut incoming.sessions {
        let id = &row.stable_ref;
        if id.provider_id.is_empty()
            || id.host_identity.is_empty()
            || id.session_id.is_empty()
            || !seen.insert(id.clone())
        {
            return Err("Taarof catalog contains invalid or duplicate exact identities.".into());
        }
        for field in [
            &mut row.display.provider,
            &mut row.display.host,
            &mut row.display.title,
            &mut row.display.session_id,
            &mut row.display.cwd,
        ] {
            *field = display_text(field);
        }
        row.display.repo_root = row.display.repo_root.as_deref().map(display_text);
        if let Some(binding) = &mut row.live_binding {
            binding.agent = display_text(&binding.agent);
            binding.session_id = binding.session_id.as_deref().map(display_text);
            binding.cwd = binding.cwd.as_deref().map(display_text);
            binding.workspace_name = display_text(&binding.workspace_name);
            binding.tab_name = display_text(&binding.tab_name);
        }
        row.warnings.truncate(32);
        row.warnings = row.warnings.iter().map(|s| display_text(s)).collect();
        // Only structured, identity-bound Taarof attach evidence is executable.
        row.actions.retain(|a| match a.transport {
            Transport::Taarof => {
                a.kind == ActionKind::Attach
                    && a.attach.as_ref().is_some_and(|t| {
                        t.stable_ref == row.stable_ref
                            && row.live_binding.as_ref().is_some_and(|b| {
                                b.workspace_id == t.workspace_id
                                    && b.tab_id == t.tab_id
                                    && b.pane_id == t.pane_id
                            })
                    })
                    && row.confidence == Confidence::ExactLiveBinding
                    && row.live_binding.is_some()
            }
            Transport::Ssh => a.remote.as_ref().is_some_and(|target| {
                target.host_identity == row.stable_ref.host_identity
                    && incoming
                        .remote_hosts
                        .iter()
                        .any(|h| h.ssh_target == target.ssh_target && h.ok && !h.stale)
            }),
            Transport::Local => row.source == SessionSource::LocalHistory,
        });
        if row.confidence != Confidence::ExactLiveBinding || row.live_binding.is_none() {
            row.state = SessionState::Recent;
        }
    }
    for row in incoming.sessions {
        if let Some(existing) = local
            .sessions
            .iter_mut()
            .find(|s| s.stable_ref == row.stable_ref)
        {
            existing.warnings.extend(row.warnings);
            if row.confidence == Confidence::ExactLiveBinding && row.live_binding.is_some() {
                existing.state = row.state;
                existing.confidence = row.confidence;
                existing.live_binding = row.live_binding;
                existing.actions.retain(|a| a.kind != ActionKind::Attach);
                existing.actions.extend(
                    row.actions
                        .into_iter()
                        .filter(|a| a.kind == ActionKind::Attach),
                );
            }
        } else {
            local.sessions.push(row);
        }
    }
    for mut host in incoming.remote_hosts {
        host.host = display_text(&host.host);
        host.warning = host.warning.as_deref().map(display_text);
        host.error = host.error.as_deref().map(display_text);
        local.remote_hosts.push(host);
    }
    local.sessions.sort_by_key(|s| {
        (
            std::cmp::Reverse(s.state == SessionState::Active),
            std::cmp::Reverse(s.updated_at_unix_ms),
        )
    });
    Ok(())
}
fn shell_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
pub fn ssh_command(plan: &ActionPlan) -> Result<Command, String> {
    if plan.transport != Transport::Ssh
        || plan.attach.is_some()
        || !matches!(
            plan.kind,
            ActionKind::Resume | ActionKind::New | ActionKind::Fork
        )
    {
        return Err("Unsupported remote action.".into());
    }
    let target = plan
        .remote
        .as_ref()
        .ok_or("Remote destination is missing.")?;
    let destination = &target.ssh_target;
    if destination.is_empty()
        || destination.starts_with('-')
        || !destination
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._-:[]".contains(c))
    {
        return Err("Invalid structured SSH destination.".into());
    }
    let cwd = plan
        .cwd
        .to_str()
        .filter(|s| s.starts_with('/') && !s.contains('\0'))
        .ok_or("Invalid remote cwd.")?;
    if plan.program.is_empty()
        || plan.program.starts_with('-')
        || plan.program.contains('\0')
        || plan.argv.iter().any(|s| s.contains('\0'))
    {
        return Err("Invalid remote program or arguments.".into());
    }
    // OpenSSH transports a command through the remote login shell. Every word
    // is encoded here from typed data; no catalog-provided shell text is used.
    let command = format!(
        "cd -- {} && exec {}{}",
        shell_word(cwd),
        shell_word(&plan.program),
        plan.argv
            .iter()
            .map(|s| format!(" {}", shell_word(s)))
            .collect::<String>()
    );
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-tt",
        "-oBatchMode=yes",
        "-oConnectTimeout=3",
        "--",
        destination,
        &command,
    ]);
    crate::clean_child_command(&mut cmd);
    Ok(cmd)
}

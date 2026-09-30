//! Tool version chip discovery and caching.

use super::*;

pub(super) fn tool_version_cache() -> &'static Mutex<ToolVersionCache> {
    static CACHE: OnceLock<Mutex<ToolVersionCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ToolVersionCache::default()))
}

pub(super) fn tool_version_cache_key(target: &DiscoveryTarget) -> DiscoveryCacheKey {
    match target {
        DiscoveryTarget::Local { cwd, .. } => {
            let normalized_cwd = nearest_mise_config_dir(Path::new(cwd))
                .unwrap_or_else(|| PathBuf::from(cwd))
                .to_string_lossy()
                .into_owned();
            DiscoveryCacheKey {
                location: DiscoveryCacheLocation::Local {
                    cwd: normalized_cwd,
                },
            }
        }
        DiscoveryTarget::Remote { .. } => DiscoveryCacheKey::from(target),
    }
}

pub(super) fn cached_tool_version_chip(target: &DiscoveryTarget) -> Option<Option<String>> {
    let key = tool_version_cache_key(target);
    let now = Instant::now();
    let mut cache = tool_version_cache()
        .lock()
        .expect("tool version cache lock should not be poisoned");
    match cache.entries.get(&key) {
        Some(ToolVersionCacheEntry::Ready { chip, completed_at })
            if now.duration_since(*completed_at) <= TOOL_VERSION_TTL =>
        {
            Some(chip.clone())
        }
        Some(ToolVersionCacheEntry::Pending { started_at, .. })
            if now.duration_since(*started_at) > DISCOVERY_LEASE =>
        {
            cache.entries.insert(
                key,
                ToolVersionCacheEntry::Failed {
                    reason: DiscoveryFailure::Timeout,
                    completed_at: now,
                },
            );
            Some(Some(DiscoveryFailure::Timeout.label().into()))
        }
        Some(ToolVersionCacheEntry::Failed {
            reason,
            completed_at,
        }) if now.duration_since(*completed_at) <= DISCOVERY_RETRY => {
            Some(Some(reason.label().into()))
        }
        Some(ToolVersionCacheEntry::Ready { .. } | ToolVersionCacheEntry::Failed { .. }) => {
            cache.entries.remove(&key);
            None
        }
        Some(ToolVersionCacheEntry::Pending { .. }) | None => None,
    }
}

pub(super) fn prepare_tool_version_discovery(
    target: &DiscoveryTarget,
) -> (ToolVersionDiscoveryRequest, u64) {
    let key = tool_version_cache_key(target);
    let now = Instant::now();
    let mut cache = tool_version_cache()
        .lock()
        .expect("tool version cache lock should not be poisoned");
    match cache.entries.get(&key) {
        Some(ToolVersionCacheEntry::Pending { started_at, .. })
            if now.duration_since(*started_at) <= DISCOVERY_LEASE =>
        {
            (ToolVersionDiscoveryRequest::Pending, 0)
        }
        Some(ToolVersionCacheEntry::Ready { completed_at, .. })
            if now.duration_since(*completed_at) <= TOOL_VERSION_TTL =>
        {
            (ToolVersionDiscoveryRequest::UseCached, 0)
        }
        Some(ToolVersionCacheEntry::Failed { completed_at, .. })
            if now.duration_since(*completed_at) <= DISCOVERY_RETRY =>
        {
            (ToolVersionDiscoveryRequest::UseCached, 0)
        }
        _ => {
            let generation = next_discovery_generation();
            cache.entries.insert(
                key,
                ToolVersionCacheEntry::Pending {
                    generation,
                    started_at: now,
                },
            );
            (ToolVersionDiscoveryRequest::Start, generation)
        }
    }
}

pub(super) fn complete_tool_version_chip(
    target: &DiscoveryTarget,
    generation: u64,
    result: Result<Option<String>, DiscoveryFailure>,
) {
    let key = tool_version_cache_key(target);
    let mut cache = tool_version_cache()
        .lock()
        .expect("tool version cache lock should not be poisoned");
    if !matches!(cache.entries.get(&key), Some(ToolVersionCacheEntry::Pending { generation: current, .. }) if *current == generation)
    {
        return;
    }
    let completed_at = Instant::now();
    let entry = match result {
        Ok(chip) => ToolVersionCacheEntry::Ready { chip, completed_at },
        Err(reason) => ToolVersionCacheEntry::Failed {
            reason,
            completed_at,
        },
    };
    cache.entries.insert(key, entry);
}

pub(super) fn spawn_tool_version_worker(target: DiscoveryTarget, generation: u64) {
    std::thread::spawn(move || {
        let result = discover_tool_version_chip(&target);
        complete_tool_version_chip(&target, generation, result);
    });
}

/// Non-blocking sidebar/API entry point. A failed probe displays a safe reason
/// during the retry cooldown; the next request can start a new generation.
pub(crate) fn tool_version_chip_text_for_target(target: &DiscoveryTarget) -> Option<String> {
    if let Some(chip) = cached_tool_version_chip(target) {
        return chip;
    }
    if let (ToolVersionDiscoveryRequest::Start, generation) = prepare_tool_version_discovery(target)
    {
        spawn_tool_version_worker(target.clone(), generation);
    }
    None
}

pub(super) fn discover_tool_version_chip(
    target: &DiscoveryTarget,
) -> Result<Option<String>, DiscoveryFailure> {
    if let DiscoveryTarget::Local { cwd, .. } = target {
        if !has_mise_config(Path::new(cwd)) {
            return Ok(None);
        }
    }
    Ok(format_tool_version_chip(
        &discover_tool_versions_for_target(target)?,
    ))
}

pub(super) fn format_tool_version_chip(tool_versions: &[ToolVersionEntry]) -> Option<String> {
    let mut prioritized = tool_versions.to_vec();
    prioritized.sort_by(|left, right| {
        tool_version_priority(&left.tool)
            .cmp(&tool_version_priority(&right.tool))
            .then_with(|| {
                left.tool
                    .to_ascii_lowercase()
                    .cmp(&right.tool.to_ascii_lowercase())
            })
    });

    let summary = prioritized
        .into_iter()
        .take(3)
        .map(|entry| format!("{} {}", entry.tool, compact_tool_version(&entry.version)))
        .collect::<Vec<_>>();

    (!summary.is_empty()).then(|| summary.join(" • "))
}

pub(super) fn compact_tool_version(version: &str) -> String {
    let trimmed = version.trim().trim_start_matches('v');
    let core = trimmed.split('-').next().unwrap_or(trimmed);
    let parts = core.split('.').take(2).collect::<Vec<_>>();

    if parts.is_empty() {
        trimmed.to_string()
    } else {
        parts.join(".")
    }
}

pub(super) fn tool_version_priority(tool: &str) -> usize {
    match tool.to_ascii_lowercase().as_str() {
        "node" => 0,
        "python" => 1,
        "ruby" => 2,
        "rust" => 3,
        "go" => 4,
        _ => 100,
    }
}

pub(super) fn discover_tool_versions_for_target(
    target: &DiscoveryTarget,
) -> Result<Vec<ToolVersionEntry>, DiscoveryFailure> {
    #[cfg(test)]
    if let Some(tool_versions) = run_tool_version_test_probe() {
        return Ok(filter_tool_versions_for_target(tool_versions, target));
    }

    let tool_versions = match target {
        DiscoveryTarget::Local { cwd, binary_path } => {
            let Some(binary_path) = require_local_mise_binary(
                binary_path,
                "current",
                serde_json::json!({
                    "cwd": cwd,
                }),
            ) else {
                return Ok(Vec::new());
            };

            discover_tool_versions_with_binary(cwd, &binary_path)
        }
        DiscoveryTarget::Remote {
            host,
            cwd,
            ssh_argv,
        } => discover_remote_tool_versions(host, cwd, ssh_argv),
    };

    Ok(filter_tool_versions_for_target(tool_versions?, target))
}

pub(super) fn discover_tool_versions_with_binary(
    cwd: &str,
    mise_bin: &Path,
) -> Result<Vec<ToolVersionEntry>, DiscoveryFailure> {
    // Both compatibility attempts share one wall budget.
    let deadline = Instant::now() + DISCOVERY_TIMEOUT;
    let mut last_failure = DiscoveryFailure::Exit;
    for args in [
        ["current", "--json"].as_slice(),
        ["ls", "--current", "--json"].as_slice(),
    ] {
        let mut command = crate::child_process::command(mise_bin);
        command.args(args).current_dir(cwd);
        match run_discovery_command(command, deadline.saturating_duration_since(Instant::now())) {
            Ok(output) => return parse_tool_versions_result(&output),
            Err(DiscoveryFailure::Exit) => last_failure = DiscoveryFailure::Exit,
            Err(reason) => return Err(reason),
        }
    }
    Err(last_failure)
}

fn parse_tool_versions_result(output: &str) -> Result<Vec<ToolVersionEntry>, DiscoveryFailure> {
    let value: serde_json::Value =
        serde_json::from_str(output).map_err(|_| DiscoveryFailure::InvalidJson)?;
    match value {
        serde_json::Value::Object(map) => Ok(map
            .iter()
            .flat_map(|(tool, entries)| parse_tool_version_entries(Some(tool), entries))
            .collect()),
        serde_json::Value::Array(entries) => Ok(entries
            .iter()
            .flat_map(|entry| parse_tool_version_entries(None, entry))
            .collect()),
        _ => Err(DiscoveryFailure::InvalidJson),
    }
}

pub(super) fn parse_tool_version_entries(
    tool_hint: Option<&str>,
    value: &serde_json::Value,
) -> Vec<ToolVersionEntry> {
    match value {
        serde_json::Value::Array(entries) => entries
            .iter()
            .filter_map(|entry| parse_tool_version_entry(tool_hint, entry))
            .collect(),
        _ => parse_tool_version_entry(tool_hint, value)
            .into_iter()
            .collect(),
    }
}

pub(super) fn parse_tool_version_entry(
    tool_hint: Option<&str>,
    value: &serde_json::Value,
) -> Option<ToolVersionEntry> {
    let object = value.as_object()?;
    let tool = tool_hint
        .map(str::to_string)
        .or_else(|| {
            ["plugin_name", "tool", "name"]
                .iter()
                .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
                .map(str::to_string)
        })?
        .trim()
        .to_string();
    let version = ["version", "actual_version", "requested_version"]
        .iter()
        .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))?
        .trim()
        .to_string();
    if tool.is_empty() || version.is_empty() {
        return None;
    }

    let source = object.get("source").and_then(parse_tool_version_source);
    Some(ToolVersionEntry {
        tool,
        version,
        source,
    })
}

pub(super) fn parse_tool_version_source(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(path) => Some(path.clone()),
        serde_json::Value::Object(map) => map
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

pub(super) fn tool_version_scope(
    tool_version: &ToolVersionEntry,
    discovery_root: &Path,
) -> MiseTaskScope {
    let Some(source) = tool_version.source.as_deref() else {
        return MiseTaskScope::ProjectRoot;
    };
    let source_path = Path::new(source);
    let source_dir = source_path.parent().unwrap_or(source_path);

    if source_dir.starts_with(discovery_root) {
        return MiseTaskScope::ProjectRoot;
    }

    if discovery_root.starts_with(source_dir) {
        return MiseTaskScope::ParentDir;
    }

    MiseTaskScope::Global
}

pub(super) fn filter_tool_versions_for_target(
    tool_versions: Vec<ToolVersionEntry>,
    target: &DiscoveryTarget,
) -> Vec<ToolVersionEntry> {
    let discovery_root = match target {
        DiscoveryTarget::Local { cwd, .. } | DiscoveryTarget::Remote { cwd, .. } => Path::new(cwd),
    };

    tool_versions
        .into_iter()
        .filter(|tool_version| {
            tool_version_scope(tool_version, discovery_root) != MiseTaskScope::Global
        })
        .collect()
}

pub(super) fn remote_mise_current_command(cwd: &str) -> String {
    format!(
        "MISE_BIN=$(command -v mise || true); \
if [ -z \"$MISE_BIN\" ] && [ -x \"$HOME/.local/bin/mise\" ]; then MISE_BIN=\"$HOME/.local/bin/mise\"; fi; \
[ -n \"$MISE_BIN\" ] || exit 127; \
cd {} && (\"$MISE_BIN\" current --json 2>/dev/null || \"$MISE_BIN\" ls --current --json)",
        shell_quote(cwd),
    )
}

pub(super) fn discover_remote_tool_versions(
    _host: &str,
    cwd: &str,
    ssh_argv: &[String],
) -> Result<Vec<ToolVersionEntry>, DiscoveryFailure> {
    let remote_command = remote_mise_current_command(cwd);
    let argv =
        ssh_command_with_remote_exec(ssh_argv, remote_command, false, &["-o", "BatchMode=yes"])
            .ok_or(DiscoveryFailure::Unavailable)?;
    let mut command = crate::child_process::command(&argv[0]);
    command.args(&argv[1..]);
    parse_tool_versions_result(&run_discovery_command(command, DISCOVERY_TIMEOUT)?)
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub(super) struct ToolVersionTestProbe {
    pub(super) result: Option<Vec<ToolVersionEntry>>,
    pub(super) calls: usize,
}

#[cfg(test)]
pub(super) fn tool_version_test_probe() -> &'static Mutex<Option<ToolVersionTestProbe>> {
    static PROBE: OnceLock<Mutex<Option<ToolVersionTestProbe>>> = OnceLock::new();
    PROBE.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
pub(super) fn run_tool_version_test_probe() -> Option<Vec<ToolVersionEntry>> {
    let mut probe = tool_version_test_probe()
        .lock()
        .expect("tool version test probe lock should not be poisoned");
    let probe = probe.as_mut()?;
    probe.calls += 1;
    probe.result.clone()
}

#[cfg(test)]
pub(crate) fn install_tool_version_test_probe(result: Option<Vec<ToolVersionEntry>>) {
    tool_version_test_probe()
        .lock()
        .expect("tool version test probe lock should not be poisoned")
        .replace(ToolVersionTestProbe { result, calls: 0 });
}

#[cfg(test)]
pub(crate) fn clear_tool_version_test_probe() {
    tool_version_test_probe()
        .lock()
        .expect("tool version test probe lock should not be poisoned")
        .take();
}

#[cfg(test)]
pub(crate) fn tool_version_test_probe_call_count() -> usize {
    tool_version_test_probe()
        .lock()
        .expect("tool version test probe lock should not be poisoned")
        .as_ref()
        .map(|probe| probe.calls)
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) fn clear_tool_version_cache_for_test() {
    tool_version_cache()
        .lock()
        .expect("tool version cache lock should not be poisoned")
        .entries
        .clear();
}

#[cfg(test)]
pub(crate) fn wait_for_tool_version_chip(target: &DiscoveryTarget) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let _ = tool_version_chip_text_for_target(target);
        let key = tool_version_cache_key(target);
        let cache = tool_version_cache()
            .lock()
            .expect("tool version cache lock should not be poisoned");
        if let Some(ToolVersionCacheEntry::Ready { chip, .. }) = cache.entries.get(&key) {
            return chip.clone();
        }
        drop(cache);
        assert!(
            Instant::now() < deadline,
            "timed out waiting for tool version chip discovery for {target:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
pub(crate) fn tool_version_test_entry(
    tool: &str,
    version: &str,
    source: &Path,
) -> ToolVersionEntry {
    ToolVersionEntry {
        tool: tool.to_string(),
        version: version.to_string(),
        source: Some(source.join(".mise.toml").to_string_lossy().into_owned()),
    }
}

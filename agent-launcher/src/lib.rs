//! Standalone catalog and exact launcher. Provider arguments never pass through a shell.
pub mod enrichment;
use agent_session_core::adapters::ExternalRegistry;
use agent_session_core::*;
use std::path::{Path, PathBuf};

pub fn local_host() -> Result<String, String> {
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map_err(|_| "local host identity unavailable")?;
    let hostname = hostname.trim().trim_end_matches('.').to_ascii_lowercase();
    if hostname.is_empty() {
        return Err("local host identity unavailable".into());
    }
    Ok(hostname)
}
pub fn local_registry() -> Result<BuiltinRegistry, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute() && home.is_dir())
        .ok_or("HOME must name an existing absolute history root")?;
    Ok(BuiltinRegistry::new(DiscoveryRoots::from_home(Some(home))))
}
pub fn local_catalog() -> Result<SessionCatalog, String> {
    let host = local_host()?;
    let mut catalog = SessionCatalog::from_discovery(local_registry()?.discover(), &host);
    external_registry()?.merge_catalog(&mut catalog, &host);
    Ok(catalog)
}
/// Discovery always retains healthy local history if the optional runtime fails.
pub fn enriched_catalog(session: Option<&str>) -> Result<(SessionCatalog, Option<String>), String> {
    let mut catalog = local_catalog()?;
    let warning = enrichment::enrich_catalog(&mut catalog, session).err();
    Ok((catalog, warning))
}
pub fn executable(program: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let candidates: Vec<PathBuf> = if program.contains('/') {
        vec![PathBuf::from(program)]
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|p| p.join(program))
            .collect()
    };
    candidates
        .into_iter()
        .find(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .and_then(|p| std::fs::canonicalize(p).ok())
        .ok_or_else(|| format!("provider executable unavailable: {}", display_text(program)))
}
pub fn provider_report(doctor: bool) -> Result<serde_json::Value, String> {
    let registry = local_registry()?;
    let mut providers: Vec<_> = registry.adapters().iter().map(|a| {
        let meta = a.metadata();
        let mut history = a.probe();
        history.name = display_text(&history.name);
        history.error = history.error.as_deref().map(display_text);
        history.warning = history.warning.as_deref().map(display_text);
        serde_json::json!({"id":meta.id,"actions":meta.actions,"executable_available":executable(&meta.id).is_ok(),"history":history})
    }).collect();
    let external = external_registry()?;
    let mut catalog = SessionCatalog::from_discovery(Default::default(), &local_host()?);
    external.merge_catalog(&mut catalog, &local_host()?);
    for mut history in catalog.providers {
        let adapter = external
            .adapters()
            .iter()
            .find(|a| a.manifest.id == history.name);
        if doctor && history.ok {
            if let Some(adapter) =
                adapter.filter(|a| a.manifest.capabilities.iter().any(|c| c == "new"))
            {
                if let Err(error) = adapter.checked_plan(
                    ActionKind::New,
                    &std::env::current_dir().map_err(|_| "cwd unavailable")?,
                    None,
                ) {
                    history.ok = false;
                    history.error = Some(error);
                }
            }
        }
        providers.push(serde_json::json!({"id":history.name,"external":true,"actions":adapter.map(|a|a.metadata().actions).unwrap_or_default(),"executable_available":history.ok,"history":history}));
    }
    Ok(
        serde_json::json!({"schema":if doctor { "agent.doctor.v1" } else { "agent.providers.v1" }, "providers":providers}),
    )
}
pub fn stable_ref_selector(reference: &StableRef) -> String {
    serde_json::to_string(reference).expect("stable reference is serializable")
}
pub fn select_resume(catalog: &SessionCatalog, selector: &str) -> Result<ActionPlan, String> {
    select_action(catalog, selector, ActionKind::Resume)
}
pub fn select_action(
    catalog: &SessionCatalog,
    selector: &str,
    kind: ActionKind,
) -> Result<ActionPlan, String> {
    // Full references are JSON objects so delimiters inside opaque IDs are lossless.
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Selector {
        provider_id: String,
        host_identity: String,
        session_id: String,
    }
    let exact = if selector.starts_with('{') {
        let value: Selector =
            serde_json::from_str(selector).map_err(|_| "malformed stable reference")?;
        if value.provider_id.is_empty()
            || value.host_identity.is_empty()
            || value.session_id.is_empty()
        {
            return Err("malformed stable reference".into());
        }
        Some(StableRef::new(
            &value.provider_id,
            &value.host_identity,
            &value.session_id,
        ))
    } else {
        None
    };
    if selector.is_empty() {
        return Err("missing session selector".into());
    }
    let matches: Vec<_> = catalog
        .sessions
        .iter()
        .filter(|s| {
            exact
                .as_ref()
                .map_or(s.stable_ref.session_id == selector, |r| &s.stable_ref == r)
        })
        .collect();
    if matches.len() != 1 {
        return Err(if matches.is_empty() {
            "session missing or stale"
        } else {
            "ambiguous session selector; use stable_ref from --json"
        }
        .into());
    }
    let session = matches[0];
    if session.stable_ref.session_id.is_empty()
        || session.stable_ref.session_id.starts_with('-')
        || session.stable_ref.session_id.contains('\0')
    {
        return Err("malformed session identity".into());
    }
    if session.source == SessionSource::LocalHistory
        && kind != ActionKind::Attach
        && catalog
            .providers
            .iter()
            .any(|p| p.name == session.stable_ref.provider_id && !p.ok)
    {
        return Err("provider discovery unavailable; refusing stale selection".into());
    }
    session
        .actions
        .iter()
        .find(|a| {
            a.kind == kind
                && (a.transport != Transport::Ssh
                    || a.remote.as_ref().is_some_and(|r| {
                        catalog
                            .remote_hosts
                            .iter()
                            .any(|h| h.ssh_target == r.ssh_target && h.ok && !h.stale)
                    }))
        })
        .cloned()
        .ok_or_else(|| "action unavailable or unsupported; refresh the catalog".into())
}
pub fn plan_new(provider: &str, cwd: PathBuf) -> Result<ActionPlan, String> {
    if let Some(adapter) = local_registry()?
        .adapters()
        .iter()
        .find(|a| a.metadata().id == provider)
    {
        return adapter
            .plan_new(cwd)
            .ok_or_else(|| "new action unsupported".into());
    }
    external_registry()?
        .adapters()
        .iter()
        .find(|a| a.manifest.id == provider)
        .ok_or_else(|| "unknown provider or new action unsupported".to_string())?
        .checked_plan(ActionKind::New, &cwd, None)
}
pub fn validate_plan(plan: &ActionPlan) -> Result<PathBuf, String> {
    if plan.transport != Transport::Local
        || plan.remote.is_some()
        || plan.attach.is_some()
        || !matches!(plan.kind, ActionKind::New | ActionKind::Resume)
    {
        return Err("unsupported execution transport or action".into());
    }
    if !plan.cwd.is_absolute() || !Path::new(&plan.cwd).is_dir() {
        return Err("session cwd is missing or unavailable".into());
    }
    if plan.program.contains('\0') || plan.argv.iter().any(|a| a.contains('\0')) {
        return Err("malformed execution argument".into());
    }
    executable(&plan.program)
}
pub(crate) fn clean_child_command(
    command: &mut std::process::Command,
) -> &mut std::process::Command {
    command
        .env_remove("INFISICAL_TOKEN")
        .env_remove("INFISICAL_SERVICE_TOKEN")
}
pub fn execute_plan(plan: &ActionPlan) -> Result<(), String> {
    execute_plan_for_session(plan, None)
}
pub fn execute_plan_for_session(plan: &ActionPlan, session: Option<&str>) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    if plan.transport == Transport::Taarof {
        if plan.kind != ActionKind::Attach || plan.remote.is_some() || !plan.argv.is_empty() {
            return Err("Unsupported Taarof action.".into());
        }
        return enrichment::configured_client(session)?
            .attach(plan.attach.as_ref().ok_or("Exact live target missing.")?);
    }
    let mut command = if plan.transport == Transport::Ssh {
        enrichment::ssh_command(plan)?
    } else {
        let program = validate_plan(plan)?;
        let mut command = std::process::Command::new(program);
        command.args(&plan.argv).current_dir(&plan.cwd);
        command
    };
    let error = clean_child_command(&mut command).exec();
    Err(format!("provider execution failed: {error}"))
}

/// Identity embedded when this launcher was compiled, independent of installation.
pub fn build_info() -> serde_json::Value {
    let nonempty = |value: &str| (!value.is_empty()).then(|| value.to_owned());
    serde_json::json!({
        "schema": "agent.build.v1",
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
/// Explicit XDG configuration only; PATH scanning never discovers providers.
pub fn external_registry() -> Result<ExternalRegistry, String> {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(path) if Path::new(&path).is_absolute() => PathBuf::from(path),
        Some(_) => return Err("XDG_CONFIG_HOME must be absolute".into()),
        None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME unavailable")?).join(".config"),
    };
    Ok(ExternalRegistry::load(&config.join("agent/providers.d")))
}
pub fn adapter_conformance(id: &str) -> Result<serde_json::Value, String> {
    let registry = external_registry()?;
    let adapter = registry
        .adapters()
        .iter()
        .find(|a| a.manifest.id == id)
        .ok_or("enabled adapter manifest unavailable")?;
    adapter.check_conformance(&std::env::current_dir().map_err(|_| "cwd unavailable")?)?;
    Ok(serde_json::json!({"schema":"agent.adapter-conformance.v1","provider":id,"ok":true}))
}
pub mod tui;

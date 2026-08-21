//! Agent signature file loading and default catalogue.
//!
//! This module owns the deserializable signature structs, the config-path
//! resolver, the on-disk loader, and the merged-signature resolver that
//! combines the global catalogue with per-project overrides.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub(super) struct AgentSignature {
    pub(super) name: String,
    #[serde(default)]
    pub(super) patterns: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct AgentSignatureFile {
    #[serde(default)]
    pub(super) signatures: Vec<AgentSignature>,
}

pub(super) fn default_agent_signatures() -> Vec<AgentSignature> {
    vec![
        AgentSignature {
            name: "claude".into(),
            patterns: vec!["claude".into()],
        },
        AgentSignature {
            name: "codex".into(),
            patterns: vec!["codex".into()],
        },
        AgentSignature {
            name: "aider".into(),
            patterns: vec!["aider".into()],
        },
        AgentSignature {
            name: "copilot".into(),
            patterns: vec!["copilot".into()],
        },
        AgentSignature {
            name: "opencode".into(),
            patterns: vec!["opencode".into()],
        },
        AgentSignature {
            name: "pi".into(),
            patterns: vec!["pii".into()],
        },
        AgentSignature {
            name: "cursor".into(),
            patterns: vec!["cursor".into()],
        },
        AgentSignature {
            name: "cline".into(),
            patterns: vec!["cline".into()],
        },
        AgentSignature {
            name: "continue".into(),
            patterns: vec!["continue".into()],
        },
        AgentSignature {
            name: "goose".into(),
            patterns: vec!["goose".into()],
        },
        AgentSignature {
            name: "gemini".into(),
            patterns: vec!["gemini".into()],
        },
        AgentSignature {
            name: "kimi".into(),
            patterns: vec!["kimi".into()],
        },
    ]
}

pub(super) fn agent_signature_config_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("TAAROF_AGENT_SIGNATURES") {
        let path = PathBuf::from(path);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    dirs::config_dir().map(|dir| dir.join("taarof/agent-signatures.json"))
}

pub(super) fn load_agent_signatures_from_path(path: &Path) -> Option<Vec<AgentSignature>> {
    let content = fs::read_to_string(path).ok()?;
    let parsed: AgentSignatureFile = serde_json::from_str(&content).ok()?;
    let signatures: Vec<AgentSignature> = parsed
        .signatures
        .into_iter()
        .filter(|sig| !sig.name.trim().is_empty())
        .map(|mut sig| {
            if sig.patterns.is_empty() {
                sig.patterns.push(sig.name.clone());
            }
            sig
        })
        .collect();
    if signatures.is_empty() {
        None
    } else {
        Some(signatures)
    }
}

pub(super) fn agent_signatures() -> &'static [AgentSignature] {
    static SIGNATURES: OnceLock<Vec<AgentSignature>> = OnceLock::new();
    SIGNATURES
        .get_or_init(|| {
            agent_signature_config_path()
                .as_deref()
                .and_then(load_agent_signatures_from_path)
                .unwrap_or_else(default_agent_signatures)
        })
        .as_slice()
}

pub(super) fn merged_agent_signatures_for_cwd(cwd: Option<&str>) -> Vec<AgentSignature> {
    let mut signatures = agent_signatures().to_vec();

    let Some(project) = cwd.and_then(crate::project_config::load_for_workspace_cwd) else {
        return signatures;
    };

    signatures.extend(
        project
            .agents
            .signatures
            .into_iter()
            .map(|signature| AgentSignature {
                name: signature.name,
                patterns: signature.patterns,
            }),
    );
    signatures
}

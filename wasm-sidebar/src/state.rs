use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use zellij_tile::prelude::*;

// --- Configuration ---

#[derive(Clone, Debug, PartialEq, Default)]
pub enum PluginRole {
    Tabs, // Left strip: just tab names + numbers
    #[default]
    Info, // Right panel: sessions, project, agents, alerts, shortcuts
}

#[derive(Default)]
pub struct SidebarConfig {
    pub role: PluginRole,
    pub home: String,
    pub agents_json: String,
    pub projects_json: String,
    pub alerts_log: String,
    pub pids_dir: String,
}

// --- Data Types ---

#[derive(Clone, Debug)]
pub struct Session {
    pub name: String,
    pub active: bool,
    pub attached: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ProjectContext {
    pub cwd: String,
    pub branch: String,
    pub commit: String,
    pub recent_projects: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct TabEntry {
    pub name: String,
    pub position: usize,
    pub active: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct Agent {
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub command: String,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct AgentStatus {
    pub running: bool,
    pub pid: u32,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Alert {
    pub timestamp: String,
    pub title: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize)]
struct AgentsFile {
    #[serde(default)]
    agents: Vec<Agent>,
}

#[derive(Clone, Debug, Deserialize)]
struct ProjectsFile {
    #[serde(default)]
    projects: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct PidFile {
    pid: u32,
}

// --- UI State ---
// Info panel sections: SESSIONS, PROJECT, AGENTS, FILES, ALERTS, SHORTCUTS

pub const NUM_SECTIONS: usize = 6;
pub const SEC_SESSIONS: usize = 0;
pub const SEC_PROJECT: usize = 1;
pub const SEC_AGENTS: usize = 2;
pub const SEC_FILES: usize = 3;
pub const SEC_ALERTS: usize = 4;
pub const SEC_SHORTCUTS: usize = 5;

#[allow(dead_code)]
pub const SECTION_NAMES: [&str; NUM_SECTIONS] = [
    "SESSIONS",
    "PROJECT",
    "AGENTS",
    "FILES",
    "ALERTS",
    "SHORTCUTS",
];

#[derive(Clone, Debug)]
pub enum ClickTarget {
    Tab(usize),     // tab index (0-based)
    Section(usize), // section header index
}

#[derive(Clone, Debug)]
pub struct UIState {
    pub sections: [bool; NUM_SECTIONS],
    pub focused_section: usize,
    pub click_map: Vec<Option<ClickTarget>>, // line → target
}

impl Default for UIState {
    fn default() -> Self {
        Self {
            //               SESS  PROJ  AGNT  FILE  ALRT  SHORT
            sections: [true, true, true, false, true, true],
            focused_section: 0,
            click_map: Vec::new(),
        }
    }
}

// --- Main State ---

#[derive(Default)]
pub struct SidebarState {
    pub config: SidebarConfig,
    pub tabs: Vec<TabEntry>,
    pub sessions: Vec<Session>,
    pub project: ProjectContext,
    pub agents: Vec<Agent>,
    pub agent_status: BTreeMap<String, AgentStatus>,
    pub files: Vec<String>,
    pub alerts: Vec<Alert>,
    pub ui: UIState,
    pub current_session: String,
}

// Command context tags
const CTX_GIT_BRANCH: &str = "git-branch";
const CTX_GIT_COMMIT: &str = "git-commit";
const CTX_PID_CHECK: &str = "pid-check";
const CTX_AGENTS_JSON: &str = "agents-json";
const CTX_PROJECTS_JSON: &str = "projects-json";
const CTX_ALERTS_LOG: &str = "alerts-log";
const CTX_PID_LIST: &str = "pid-list";
const CTX_PID_READ: &str = "pid-read";
const CTX_CWD: &str = "cwd";
pub const CTX_FILES: &str = "files";

impl SidebarState {
    pub fn load_config(&mut self, config: BTreeMap<String, String>) {
        // Plugin role: "tabs" for left tab strip, default "info" for right panel
        self.config.role = match config.get("role").map(|s| s.as_str()) {
            Some("tabs") => PluginRole::Tabs,
            _ => PluginRole::Info,
        };
        let home = config
            .get("home")
            .cloned()
            .or_else(|| std::env::var("HOME").ok().filter(|h| !h.is_empty()))
            .unwrap_or_else(|| "/tmp/user".to_string());
        self.config.agents_json = config
            .get("agents_json")
            .cloned()
            .unwrap_or_else(|| format!("{home}/.config/agent-terminal/agents.json"));
        self.config.projects_json = config
            .get("projects_json")
            .cloned()
            .unwrap_or_else(|| format!("{home}/.local/share/agent-terminal/recent-projects.json"));
        self.config.alerts_log = config
            .get("alerts_log")
            .cloned()
            .unwrap_or_else(|| format!("{home}/.local/share/agent-terminal/alerts.log"));
        self.config.pids_dir = config
            .get("pids_dir")
            .cloned()
            .unwrap_or_else(|| format!("{home}/.local/share/agent-terminal/pids"));
        self.config.home = home;
    }

    pub fn request_all_data(&self) {
        let home = PathBuf::from(&self.config.home);
        // "." resolves to where zellij was launched (the Zellij server's CWD)
        let project = PathBuf::from(".");

        // Git info (run from project dir, not home)
        let mut ctx = BTreeMap::new();
        ctx.insert("source".to_string(), CTX_GIT_BRANCH.to_string());
        run_command_with_env_variables_and_cwd(
            &["git", "branch", "--show-current"],
            BTreeMap::new(),
            project.clone(),
            ctx,
        );

        let mut ctx = BTreeMap::new();
        ctx.insert("source".to_string(), CTX_GIT_COMMIT.to_string());
        run_command_with_env_variables_and_cwd(
            &["git", "rev-parse", "--short", "HEAD"],
            BTreeMap::new(),
            project.clone(),
            ctx,
        );

        // CWD (discover where we actually are)
        let mut ctx = BTreeMap::new();
        ctx.insert("source".to_string(), CTX_CWD.to_string());
        run_command_with_env_variables_and_cwd(&["pwd"], BTreeMap::new(), project.clone(), ctx);

        // Config file reads use absolute home paths
        let mut ctx = BTreeMap::new();
        ctx.insert("source".to_string(), CTX_AGENTS_JSON.to_string());
        run_command_with_env_variables_and_cwd(
            &["cat", &self.config.agents_json],
            BTreeMap::new(),
            home.clone(),
            ctx,
        );

        let mut ctx = BTreeMap::new();
        ctx.insert("source".to_string(), CTX_PROJECTS_JSON.to_string());
        run_command_with_env_variables_and_cwd(
            &["cat", &self.config.projects_json],
            BTreeMap::new(),
            home.clone(),
            ctx,
        );

        let mut ctx = BTreeMap::new();
        ctx.insert("source".to_string(), CTX_ALERTS_LOG.to_string());
        run_command_with_env_variables_and_cwd(
            &["tail", "-5", &self.config.alerts_log],
            BTreeMap::new(),
            home.clone(),
            ctx,
        );

        let mut ctx = BTreeMap::new();
        ctx.insert("source".to_string(), CTX_PID_LIST.to_string());
        run_command_with_env_variables_and_cwd(
            &["ls", &self.config.pids_dir],
            BTreeMap::new(),
            home,
            ctx,
        );

        // File listing from project dir
        let mut ctx = BTreeMap::new();
        ctx.insert("source".to_string(), CTX_FILES.to_string());
        run_command_with_env_variables_and_cwd(
            &["ls", "-1", "--color=never"],
            BTreeMap::new(),
            project,
            ctx,
        );
    }

    pub fn handle_command_result(
        &mut self,
        exit_code: Option<i32>,
        stdout: Vec<u8>,
        _stderr: Vec<u8>,
        context: BTreeMap<String, String>,
    ) {
        let source = context.get("source").map(|s| s.as_str()).unwrap_or("");
        let output = String::from_utf8_lossy(&stdout);

        match source {
            CTX_GIT_BRANCH => {
                self.project.branch = output.trim().to_string();
            }
            CTX_GIT_COMMIT => {
                self.project.commit = output.trim().to_string();
            }
            CTX_CWD => {
                let raw = output.trim().to_string();
                let home = &self.config.home;
                if !home.is_empty() && raw.starts_with(home) {
                    self.project.cwd = format!("~{}", &raw[home.len()..]);
                } else {
                    self.project.cwd = raw;
                }
            }
            CTX_AGENTS_JSON if exit_code == Some(0) => {
                if let Ok(parsed) = serde_json::from_str::<AgentsFile>(output.trim()) {
                    self.agents = parsed.agents;
                }
            }
            CTX_PROJECTS_JSON if exit_code == Some(0) => {
                if let Ok(parsed) = serde_json::from_str::<ProjectsFile>(output.trim()) {
                    self.project.recent_projects = parsed.projects.into_iter().take(5).collect();
                }
            }
            CTX_ALERTS_LOG if exit_code == Some(0) => {
                self.alerts = output
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|line| {
                        let parts: Vec<&str> = line.splitn(3, '|').collect();
                        if parts.len() >= 2 {
                            Some(Alert {
                                timestamp: parts[0].trim().to_string(),
                                title: parts[1].trim().to_string(),
                                message: parts.get(2).unwrap_or(&"").trim().to_string(),
                            })
                        } else {
                            None
                        }
                    })
                    .collect();
            }
            CTX_PID_LIST if exit_code == Some(0) => {
                self.agent_status.clear();
                let cwd = PathBuf::from(&self.config.home);
                for filename in output.lines() {
                    let filename = filename.trim();
                    if filename.ends_with(".json") {
                        let name = filename.trim_end_matches(".json").to_string();
                        let path = format!("{}/{}", self.config.pids_dir, filename);
                        let mut ctx = BTreeMap::new();
                        ctx.insert("source".to_string(), CTX_PID_READ.to_string());
                        ctx.insert("agent".to_string(), name);
                        run_command_with_env_variables_and_cwd(
                            &["cat", &path],
                            BTreeMap::new(),
                            cwd.clone(),
                            ctx,
                        );
                    }
                }
            }
            CTX_PID_READ if exit_code == Some(0) => {
                if let Some(agent_name) = context.get("agent") {
                    if let Ok(pidfile) = serde_json::from_str::<PidFile>(output.trim()) {
                        let mut ctx = BTreeMap::new();
                        ctx.insert("source".to_string(), CTX_PID_CHECK.to_string());
                        ctx.insert("agent".to_string(), agent_name.clone());
                        ctx.insert("pid".to_string(), pidfile.pid.to_string());
                        run_command_with_env_variables_and_cwd(
                            &["kill", "-0", &pidfile.pid.to_string()],
                            BTreeMap::new(),
                            PathBuf::from(&self.config.home),
                            ctx,
                        );
                    }
                }
            }
            CTX_PID_CHECK => {
                if let Some(agent_name) = context.get("agent") {
                    let pid: u32 = context.get("pid").and_then(|p| p.parse().ok()).unwrap_or(0);
                    let running = exit_code == Some(0);
                    if running {
                        self.agent_status
                            .insert(agent_name.clone(), AgentStatus { running, pid });
                    } else {
                        self.agent_status.remove(agent_name);
                        // Clean up stale PID file
                        let pidfile = format!("{}/{}.json", self.config.pids_dir, agent_name);
                        let mut ctx = BTreeMap::new();
                        ctx.insert("source".to_string(), "cleanup".to_string());
                        run_command_with_env_variables_and_cwd(
                            &["rm", "-f", &pidfile],
                            BTreeMap::new(),
                            PathBuf::from(&self.config.home),
                            ctx,
                        );
                    }
                }
            }
            CTX_FILES if exit_code == Some(0) => {
                self.files = output
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| l.trim().to_string())
                    .take(15)
                    .collect();
            }
            _ => {}
        }
    }

    pub fn handle_tab_update(&mut self, tab_infos: Vec<TabInfo>) {
        self.tabs = tab_infos
            .into_iter()
            .map(|t| TabEntry {
                position: t.position,
                active: t.active,
                name: t.name,
            })
            .collect();
    }

    pub fn handle_session_update(&mut self, sessions: Vec<SessionInfo>) {
        if !sessions.is_empty() {
            for s in &sessions {
                if s.is_current_session {
                    self.current_session = s.name.clone();
                    break;
                }
            }
            self.sessions = sessions
                .into_iter()
                .map(|s| Session {
                    attached: s.is_current_session,
                    active: s.is_current_session || !s.name.is_empty(),
                    name: s.name,
                })
                .take(10)
                .collect();
        }
    }
}

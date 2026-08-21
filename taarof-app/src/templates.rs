use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::session::SavedTab;

const TEMPLATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TemplateKind {
    Tab,
    Workspace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedWorkspaceTemplate {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub work_origin: Option<String>,
    #[serde(default)]
    pub tabs: Vec<SavedTab>,
    #[serde(default)]
    pub active_tab_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[allow(clippy::large_enum_variant)] // Keep the stable template JSON/model shape; boxing would spread through the public template API.
pub enum TemplateRecord {
    Tab {
        name: String,
        tab: SavedTab,
    },
    Workspace {
        name: String,
        workspace: SavedWorkspaceTemplate,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TemplateStore {
    #[serde(default = "template_schema_version")]
    version: u32,
    #[serde(default)]
    templates: Vec<TemplateRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TemplateStoreSignature {
    modified: Option<std::time::SystemTime>,
    len: u64,
}

#[derive(Debug, Clone)]
struct TemplateStoreCacheEntry {
    path: PathBuf,
    signature: Option<TemplateStoreSignature>,
    store: TemplateStore,
}

type TemplateStoreCache = Option<TemplateStoreCacheEntry>;

fn template_store_cache() -> &'static Mutex<TemplateStoreCache> {
    static CACHE: OnceLock<Mutex<TemplateStoreCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

impl Default for TemplateStore {
    fn default() -> Self {
        Self {
            version: TEMPLATE_SCHEMA_VERSION,
            templates: Vec::new(),
        }
    }
}

impl TemplateRecord {
    pub fn name(&self) -> &str {
        match self {
            Self::Tab { name, .. } | Self::Workspace { name, .. } => name,
        }
    }

    pub fn kind(&self) -> TemplateKind {
        match self {
            Self::Tab { .. } => TemplateKind::Tab,
            Self::Workspace { .. } => TemplateKind::Workspace,
        }
    }

    pub fn as_tab(&self) -> Option<&SavedTab> {
        match self {
            Self::Tab { tab, .. } => Some(tab),
            Self::Workspace { .. } => None,
        }
    }

    pub fn as_workspace(&self) -> Option<&SavedWorkspaceTemplate> {
        match self {
            Self::Workspace { workspace, .. } => Some(workspace),
            Self::Tab { .. } => None,
        }
    }
}

pub fn try_list() -> io::Result<Vec<TemplateRecord>> {
    let mut store = cached_store(&templates_path())?;
    sort_templates(&mut store.templates);
    Ok(store.templates)
}

#[cfg(test)]
fn list() -> Vec<TemplateRecord> {
    try_list().unwrap_or_default()
}

pub fn save(record: TemplateRecord) -> io::Result<()> {
    let path = templates_path();
    let mut store = load_store(&path)?;
    upsert_template(&mut store.templates, strip_template_task_bindings(record));
    sort_templates(&mut store.templates);
    save_store(&path, &store)?;
    cache_store(&path, &store);
    Ok(())
}

fn strip_template_task_bindings(record: TemplateRecord) -> TemplateRecord {
    match record {
        TemplateRecord::Tab { name, mut tab } => {
            strip_saved_tab_task_bindings(&mut tab);
            TemplateRecord::Tab { name, tab }
        }
        TemplateRecord::Workspace {
            name,
            mut workspace,
        } => {
            workspace.work_origin = None;
            for tab in &mut workspace.tabs {
                strip_saved_tab_task_bindings(tab);
            }
            TemplateRecord::Workspace { name, workspace }
        }
    }
}

fn strip_saved_tab_task_bindings(tab: &mut SavedTab) {
    // A template is a factory, not a restored tab. Reusing its origin would
    // merge independent instantiations in the work ledger.
    tab.work_origin = None;
    if let Some(panes) = tab.panes.as_mut() {
        strip_saved_pane_task_bindings(panes);
    }
}

fn strip_saved_pane_task_bindings(node: &mut crate::session::SavedPaneNode) {
    match node {
        crate::session::SavedPaneNode::Leaf {
            current_task,
            work_origin,
            agent_session,
            ..
        } => {
            *current_task = None;
            *work_origin = None;
            *agent_session = None;
        }
        crate::session::SavedPaneNode::Split { first, second, .. } => {
            strip_saved_pane_task_bindings(first);
            strip_saved_pane_task_bindings(second);
        }
    }
}

pub fn delete(kind: TemplateKind, name: &str) -> io::Result<bool> {
    let path = templates_path();
    let mut store = load_store(&path)?;
    let before = store.templates.len();
    store.templates.retain(|template| {
        !(template.kind() == kind && template.name().eq_ignore_ascii_case(name))
    });

    if store.templates.len() == before {
        return Ok(false);
    }

    sort_templates(&mut store.templates);
    save_store(&path, &store)?;
    cache_store(&path, &store);
    Ok(true)
}

fn template_schema_version() -> u32 {
    TEMPLATE_SCHEMA_VERSION
}

fn templates_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("taarof/templates.json")
}

fn cached_store(path: &Path) -> io::Result<TemplateStore> {
    let current_signature = load_store_signature(path)?;

    if let Some(cached_entry) = template_store_cache()
        .lock()
        .expect("template cache lock should not be poisoned")
        .as_ref()
        .filter(|cached_entry| {
            cached_entry.path == path && cached_entry.signature == current_signature
        })
    {
        return Ok(cached_entry.store.clone());
    }

    let store = load_store(path)?;
    cache_store(path, &store);
    Ok(store)
}

fn cache_store(path: &Path, store: &TemplateStore) {
    let signature = load_store_signature(path).ok().flatten();
    *template_store_cache()
        .lock()
        .expect("template cache lock should not be poisoned") = Some(TemplateStoreCacheEntry {
        path: path.to_path_buf(),
        signature,
        store: store.clone(),
    });
}

fn load_store_signature(path: &Path) -> io::Result<Option<TemplateStoreSignature>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(TemplateStoreSignature {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn load_store(path: &Path) -> io::Result<TemplateStore> {
    if !path.exists() {
        return Ok(TemplateStore::default());
    }

    let content = std::fs::read_to_string(path)?;
    serde_json::from_str(&content).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn save_store(path: &Path, store: &TemplateStore) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(store)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    write_atomically(path, &json)
}

fn write_atomically(path: &Path, content: &str) -> io::Result<()> {
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

fn upsert_template(templates: &mut Vec<TemplateRecord>, record: TemplateRecord) {
    if let Some(existing) = templates.iter_mut().find(|template| {
        template.kind() == record.kind() && template.name().eq_ignore_ascii_case(record.name())
    }) {
        *existing = record;
    } else {
        templates.push(record);
    }
}

fn sort_templates(templates: &mut [TemplateRecord]) {
    templates.sort_by(|left, right| {
        left.kind().cmp(&right.kind()).then_with(|| {
            left.name()
                .to_ascii_lowercase()
                .cmp(&right.name().to_ascii_lowercase())
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SavedPaneNode, SavedTab};
    use std::ffi::OsString;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct ScopedEnv {
        lock_dir: PathBuf,
        previous: Vec<(&'static str, Option<OsString>)>,
    }

    impl ScopedEnv {
        fn set(pairs: &[(&'static str, OsString)]) -> Self {
            let lock_dir = acquire_env_lock();
            let mut previous = Vec::with_capacity(pairs.len());
            for (key, value) in pairs {
                previous.push((*key, std::env::var_os(key)));
                std::env::set_var(key, value);
            }
            Self { lock_dir, previous }
        }
    }

    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            for (key, value) in self.previous.iter().rev() {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
            let _ = std::fs::remove_dir(&self.lock_dir);
        }
    }

    fn temp_path() -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time");
        let dir = std::env::temp_dir().join(format!("taarof-template-test-{}", ts.as_nanos()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("templates.json")
    }

    fn temp_config_home(label: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time");
        let dir =
            std::env::temp_dir().join(format!("taarof-template-config-{label}-{}", ts.as_nanos()));
        std::fs::create_dir_all(&dir).expect("create config home");
        dir
    }

    fn config_templates_path(config_home: &Path) -> PathBuf {
        config_home.join("taarof/templates.json")
    }

    fn write_template_fixture(path: &Path, templates: Vec<TemplateRecord>) {
        let store = TemplateStore {
            version: TEMPLATE_SCHEMA_VERSION,
            templates,
        };
        save_store(path, &store).expect("fixture should be written");
    }

    fn acquire_env_lock() -> PathBuf {
        let lock_dir = std::env::temp_dir().join("taarof-test-env-lock");
        loop {
            match std::fs::create_dir(&lock_dir) {
                Ok(()) => return lock_dir,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(err) => panic!("failed to acquire env lock: {err}"),
            }
        }
    }

    fn sample_tab(name: &str, cwd: &str) -> SavedTab {
        SavedTab {
            name: name.into(),
            work_origin: Some("tab-template-source".into()),
            cwd: Some(cwd.into()),
            panes: Some(SavedPaneNode::Leaf {
                work_origin: Some("pane-template-source".into()),
                cwd: Some(cwd.into()),
                ssh_command: None,
                tmux_session: None,
                tmux_host: None,
                current_task: None,
                agent_session: None,
            }),
            discovery_cwd: Some(cwd.into()),
        }
    }

    #[test]
    fn tab_template_roundtrips_through_json() {
        let record = TemplateRecord::Tab {
            name: "Focus".into(),
            tab: sample_tab("Shell", "/tmp/focus"),
        };

        let json = serde_json::to_string(&record).expect("serialize template");
        let restored: TemplateRecord = serde_json::from_str(&json).expect("deserialize template");

        match restored {
            TemplateRecord::Tab { name, tab } => {
                assert_eq!(name, "Focus");
                assert_eq!(tab.name, "Shell");
                assert_eq!(tab.cwd.as_deref(), Some("/tmp/focus"));
            }
            _ => panic!("expected tab template"),
        }
    }

    #[test]
    fn workspace_template_roundtrips_through_json() {
        let record = TemplateRecord::Workspace {
            name: "Review".into(),
            workspace: SavedWorkspaceTemplate {
                work_origin: Some("workspace-template-source".into()),
                tabs: vec![
                    sample_tab("editor", "/tmp/review"),
                    sample_tab("tests", "/tmp/review"),
                ],
                active_tab_index: 1,
            },
        };

        let json = serde_json::to_string(&record).expect("serialize template");
        let restored: TemplateRecord = serde_json::from_str(&json).expect("deserialize template");

        match restored {
            TemplateRecord::Workspace { name, workspace } => {
                assert_eq!(name, "Review");
                assert_eq!(workspace.tabs.len(), 2);
                assert_eq!(workspace.tabs[1].name, "tests");
                assert_eq!(workspace.active_tab_index, 1);
            }
            _ => panic!("expected workspace template"),
        }
    }

    #[test]
    fn tab_and_workspace_templates_strip_source_work_origins() {
        fn pane_origins_are_empty(node: &SavedPaneNode) -> bool {
            match node {
                SavedPaneNode::Leaf { work_origin, .. } => work_origin.is_none(),
                SavedPaneNode::Split { first, second, .. } => {
                    pane_origins_are_empty(first) && pane_origins_are_empty(second)
                }
            }
        }

        let tab = strip_template_task_bindings(TemplateRecord::Tab {
            name: "Tab factory".into(),
            tab: sample_tab("shell", "/tmp/tab"),
        });
        let tab = tab.as_tab().unwrap();
        assert!(tab.work_origin.is_none());
        assert!(pane_origins_are_empty(tab.panes.as_ref().unwrap()));

        let workspace = strip_template_task_bindings(TemplateRecord::Workspace {
            name: "Workspace factory".into(),
            workspace: SavedWorkspaceTemplate {
                work_origin: Some("workspace-template-source".into()),
                tabs: vec![
                    sample_tab("one", "/tmp/workspace"),
                    sample_tab("two", "/tmp/workspace"),
                ],
                active_tab_index: 0,
            },
        });
        assert!(workspace.as_workspace().unwrap().work_origin.is_none());
        assert!(workspace.as_workspace().unwrap().tabs.iter().all(|tab| {
            tab.work_origin.is_none() && tab.panes.as_ref().is_none_or(pane_origins_are_empty)
        }));
    }

    #[test]
    fn save_overwrites_matching_template_name_and_kind() {
        let path = temp_path();
        let mut store = TemplateStore::default();
        upsert_template(
            &mut store.templates,
            TemplateRecord::Tab {
                name: "Focus".into(),
                tab: sample_tab("one", "/tmp/one"),
            },
        );
        save_store(&path, &store).expect("write initial store");

        let mut loaded = load_store(&path).expect("load initial store");
        upsert_template(
            &mut loaded.templates,
            TemplateRecord::Tab {
                name: "focus".into(),
                tab: sample_tab("two", "/tmp/two"),
            },
        );
        save_store(&path, &loaded).expect("write updated store");

        let stored = load_store(&path).expect("reload store");
        assert_eq!(stored.templates.len(), 1);
        match &stored.templates[0] {
            TemplateRecord::Tab { name, tab } => {
                assert_eq!(name, "focus");
                assert_eq!(tab.name, "two");
            }
            _ => panic!("expected tab template"),
        }
    }

    #[test]
    fn delete_removes_matching_template() {
        let path = temp_path();
        let mut store = TemplateStore::default();
        store.templates.push(TemplateRecord::Tab {
            name: "Keep".into(),
            tab: sample_tab("keep", "/tmp/keep"),
        });
        store.templates.push(TemplateRecord::Workspace {
            name: "Drop".into(),
            workspace: SavedWorkspaceTemplate {
                work_origin: None,
                tabs: vec![sample_tab("drop", "/tmp/drop")],
                active_tab_index: 0,
            },
        });
        save_store(&path, &store).expect("seed store");

        let mut loaded = load_store(&path).expect("load store");
        loaded.templates.retain(|template| {
            !(template.kind() == TemplateKind::Workspace
                && template.name().eq_ignore_ascii_case("drop"))
        });
        save_store(&path, &loaded).expect("save pruned store");

        let stored = load_store(&path).expect("reload store");
        assert_eq!(stored.templates.len(), 1);
        assert_eq!(stored.templates[0].name(), "Keep");
    }

    #[test]
    fn public_template_api_roundtrips_through_xdg_config_home() {
        let config_home = temp_config_home("public-api");
        let env = ScopedEnv::set(&[("XDG_CONFIG_HOME", config_home.clone().into_os_string())]);

        save(TemplateRecord::Tab {
            name: "Focus".into(),
            tab: sample_tab("Shell", "/tmp/focus"),
        })
        .expect("tab template should persist");
        save(TemplateRecord::Workspace {
            name: "Review".into(),
            workspace: SavedWorkspaceTemplate {
                work_origin: None,
                tabs: vec![
                    sample_tab("editor", "/tmp/review"),
                    sample_tab("tests", "/tmp/review"),
                ],
                active_tab_index: 1,
            },
        })
        .expect("workspace template should persist");
        save(TemplateRecord::Tab {
            name: "focus".into(),
            tab: sample_tab("Console", "/tmp/focus-next"),
        })
        .expect("case-insensitive tab overwrite should persist");

        let listed = list();
        assert_eq!(listed.len(), 2);
        match &listed[0] {
            TemplateRecord::Tab { name, tab } => {
                assert_eq!(name, "focus");
                assert_eq!(tab.name, "Console");
                assert_eq!(tab.cwd.as_deref(), Some("/tmp/focus-next"));
            }
            _ => panic!("first template should be the saved tab"),
        }
        match &listed[1] {
            TemplateRecord::Workspace { name, workspace } => {
                assert_eq!(name, "Review");
                assert_eq!(workspace.tabs.len(), 2);
                assert_eq!(workspace.active_tab_index, 1);
            }
            _ => panic!("second template should be the saved workspace"),
        }

        let deleted = delete(TemplateKind::Workspace, "review")
            .expect("existing workspace template should delete");
        assert!(deleted);
        assert!(!delete(TemplateKind::Workspace, "missing")
            .expect("missing workspace template delete should not error"));

        let listed_after_delete = list();
        assert_eq!(listed_after_delete.len(), 1);
        assert!(matches!(listed_after_delete[0], TemplateRecord::Tab { .. }));

        drop(env);
        let _ = std::fs::remove_dir_all(config_home);
    }

    #[test]
    fn template_save_strips_current_task_bindings_recursively() {
        let config_home = temp_config_home("strip-current-task");
        let env = ScopedEnv::set(&[("XDG_CONFIG_HOME", config_home.clone().into_os_string())]);
        let bound = crate::task_binding::PaneTaskBinding {
            task_id: "EXAMPLE-110".into(),
            title: "Do not inherit".into(),
            checkout_root: Some("/repo".into()),
            reporting_token: crate::task_binding::new_reporting_token(),
        };

        save(TemplateRecord::Tab {
            name: "Bound".into(),
            tab: SavedTab {
                name: "Bound tab".into(),
                work_origin: Some("tab-bound-template-source".into()),
                cwd: Some("/tmp/bound".into()),
                panes: Some(SavedPaneNode::Split {
                    direction: "vertical".into(),
                    ratio: 0.5,
                    first: Box::new(SavedPaneNode::Leaf {
                        work_origin: Some("pane-bound-first".into()),
                        cwd: Some("/tmp/bound".into()),
                        ssh_command: None,
                        tmux_session: None,
                        tmux_host: None,
                        current_task: Some(bound.clone()),
                        agent_session: None,
                    }),
                    second: Box::new(SavedPaneNode::Leaf {
                        work_origin: Some("pane-bound-second".into()),
                        cwd: Some("/tmp/bound".into()),
                        ssh_command: None,
                        tmux_session: None,
                        tmux_host: None,
                        current_task: Some(bound),
                        agent_session: None,
                    }),
                }),
                discovery_cwd: None,
            },
        })
        .expect("template should save");

        let listed = list();
        let TemplateRecord::Tab { tab, .. } = &listed[0] else {
            panic!("expected tab template");
        };
        let SavedPaneNode::Split { first, second, .. } =
            tab.panes.as_ref().expect("pane tree should persist")
        else {
            panic!("expected split");
        };
        assert!(matches!(
            first.as_ref(),
            SavedPaneNode::Leaf {
                current_task: None,
                work_origin: None,
                ..
            }
        ));
        assert!(matches!(
            second.as_ref(),
            SavedPaneNode::Leaf {
                current_task: None,
                work_origin: None,
                ..
            }
        ));

        drop(env);
        let _ = std::fs::remove_dir_all(config_home);
    }

    #[test]
    fn public_template_api_observes_external_edits_after_cache_warm() {
        let config_home = temp_config_home("external-edit");
        let templates_path = config_templates_path(&config_home);
        let env = ScopedEnv::set(&[("XDG_CONFIG_HOME", config_home.clone().into_os_string())]);

        save(TemplateRecord::Tab {
            name: "Focus".into(),
            tab: sample_tab("Shell", "/tmp/focus"),
        })
        .expect("initial template should persist");
        let warmed = try_list().expect("cache should warm");
        assert_eq!(warmed.len(), 1);

        write_template_fixture(
            &templates_path,
            vec![
                TemplateRecord::Tab {
                    name: "Console".into(),
                    tab: sample_tab("Console", "/tmp/console"),
                },
                TemplateRecord::Workspace {
                    name: "Review".into(),
                    workspace: SavedWorkspaceTemplate {
                        work_origin: None,
                        tabs: vec![
                            sample_tab("editor", "/tmp/review"),
                            sample_tab("tests", "/tmp/review"),
                        ],
                        active_tab_index: 1,
                    },
                },
            ],
        );

        let refreshed = try_list().expect("external edit should invalidate cache");
        assert_eq!(refreshed.len(), 2);
        assert!(matches!(
            &refreshed[0],
            TemplateRecord::Tab { name, tab }
                if name == "Console" && tab.cwd.as_deref() == Some("/tmp/console")
        ));
        assert!(matches!(
            &refreshed[1],
            TemplateRecord::Workspace { name, workspace }
                if name == "Review" && workspace.active_tab_index == 1
        ));

        drop(env);
        let _ = std::fs::remove_dir_all(config_home);
    }

    #[test]
    fn public_template_api_save_preserves_external_changes() {
        let config_home = temp_config_home("external-save");
        let templates_path = config_templates_path(&config_home);
        let env = ScopedEnv::set(&[("XDG_CONFIG_HOME", config_home.clone().into_os_string())]);

        save(TemplateRecord::Tab {
            name: "Focus".into(),
            tab: sample_tab("Shell", "/tmp/focus"),
        })
        .expect("initial template should persist");
        let warmed = try_list().expect("cache should warm");
        assert_eq!(warmed.len(), 1);

        write_template_fixture(
            &templates_path,
            vec![
                TemplateRecord::Tab {
                    name: "Console".into(),
                    tab: sample_tab("Console", "/tmp/console"),
                },
                TemplateRecord::Workspace {
                    name: "Review".into(),
                    workspace: SavedWorkspaceTemplate {
                        work_origin: None,
                        tabs: vec![sample_tab("editor", "/tmp/review")],
                        active_tab_index: 0,
                    },
                },
            ],
        );

        save(TemplateRecord::Workspace {
            name: "Deploy".into(),
            workspace: SavedWorkspaceTemplate {
                work_origin: None,
                tabs: vec![sample_tab("ops", "/tmp/deploy")],
                active_tab_index: 0,
            },
        })
        .expect("save should merge with externally updated store");

        let merged = try_list().expect("merged templates should load");
        assert_eq!(merged.len(), 3);
        assert!(matches!(
            &merged[0],
            TemplateRecord::Tab { name, tab }
                if name == "Console" && tab.cwd.as_deref() == Some("/tmp/console")
        ));
        assert!(matches!(
            &merged[1],
            TemplateRecord::Workspace { name, workspace }
                if name == "Deploy" && workspace.tabs.len() == 1
        ));
        assert!(matches!(
            &merged[2],
            TemplateRecord::Workspace { name, workspace }
                if name == "Review" && workspace.tabs.len() == 1
        ));

        drop(env);
        let _ = std::fs::remove_dir_all(config_home);
    }
}

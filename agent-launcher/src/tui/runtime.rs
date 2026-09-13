use super::*;
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, IsTerminal};

pub struct Snapshot {
    pub catalog: SessionCatalog,
    pub warning: Option<String>,
    pub new_providers: Vec<NewProvider>,
    pub local_host: String,
}
struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> Result<Self, String> {
        enable_raw_mode().map_err(|e| e.to_string())?;
        let guard = Self;
        execute!(io::stderr(), EnterAlternateScreen).map_err(|e| e.to_string())?;
        Ok(guard)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stderr(), LeaveAlternateScreen, crossterm::cursor::Show);
    }
}
pub fn local_snapshot() -> Result<Snapshot, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let builtin = crate::local_registry()?;
    let external = crate::external_registry()?;
    let new_providers = builtin
        .adapters()
        .iter()
        .map(|provider| provider.as_ref())
        .chain(
            external
                .adapters()
                .iter()
                .map(|provider| provider as &dyn ProviderAdapter),
        )
        .filter_map(|provider| {
            let metadata = provider.metadata();
            if !metadata.actions.contains(&ActionKind::New) {
                return None;
            }
            let plan = provider.plan_new(cwd.clone())?;
            crate::validate_plan(&plan).ok()?;
            Some(NewProvider {
                id: metadata.id.to_string(),
                plan,
            })
        })
        .collect();
    Ok(Snapshot {
        warning: None,
        catalog: crate::local_catalog()?,
        new_providers,
        local_host: crate::local_host()?,
    })
}
pub fn run(mode: Mode) -> Result<(), String> {
    run_for_session(mode, None)
}
pub fn run_for_session(mode: Mode, session: Option<&str>) -> Result<(), String> {
    run_with(
        mode,
        || {
            let mut snapshot = local_snapshot()?;
            snapshot.warning =
                crate::enrichment::enrich_catalog(&mut snapshot.catalog, session).err();
            Ok(snapshot)
        },
        |_, _, plan| crate::execute_plan_for_session(plan, session),
    )
}
/// Callers can enrich snapshots and supply an exact action executor without
/// changing provider-neutral picker state or rendering.
pub fn run_with(
    mode: Mode,
    mut load: impl FnMut() -> Result<Snapshot, String>,
    mut execute_action: impl FnMut(&RowKey, ActionKind, &ActionPlan) -> Result<(), String>,
) -> Result<(), String> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(
            "interactive selection requires a terminal; use --json or specify a target".into(),
        );
    }
    let snapshot = load()?;
    let mut picker = Picker::new_in_directory(
        snapshot.catalog,
        snapshot.new_providers,
        snapshot.local_host,
        &std::env::current_dir().map_err(|e| e.to_string())?,
    );
    picker.message = snapshot.warning.unwrap_or_default();
    picker.mode = mode;
    picker.choose_first();
    let guard = TerminalGuard::enter()?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stderr())).map_err(|e| e.to_string())?;
    loop {
        terminal
            .draw(|frame| picker.render(frame))
            .map_err(|e| e.to_string())?;
        let event = event::read().map_err(|e| e.to_string())?;
        let Event::Key(key) = event else {
            continue;
        };
        match picker.key(key) {
            Intent::Cancel => return Ok(()),
            Intent::None => {}
            Intent::Refresh => match load() {
                Ok(snapshot) => {
                    picker.refresh(snapshot.catalog, snapshot.new_providers);
                    if let Some(warning) = snapshot.warning {
                        picker.message = display_text(&warning);
                    }
                }
                Err(error) => picker.message = format!("Refresh failed: {}", display_text(&error)),
            },
            Intent::Launch { key, kind } => {
                let plan = load().and_then(|snapshot| {
                    picker.revalidate(&key, kind, snapshot.catalog, snapshot.new_providers)
                });
                match plan {
                    Ok(plan) => {
                        // exec replaces the process; explicitly restore first.
                        drop(terminal);
                        drop(guard);
                        return execute_action(&key, kind, &plan);
                    }
                    Err(error) => picker.message = display_text(&error),
                }
            }
        }
    }
}

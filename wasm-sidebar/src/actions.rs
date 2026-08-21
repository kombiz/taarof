use crate::state::*;
use std::collections::BTreeMap;
use std::path::PathBuf;
use zellij_tile::prelude::*;

pub fn handle_mouse(state: &mut SidebarState, mouse: Mouse) -> bool {
    match mouse {
        Mouse::LeftClick(line, _col) => {
            if line < 0 {
                return false;
            }
            let row = line as usize;
            if let Some(Some(target)) = state.ui.click_map.get(row) {
                match target {
                    ClickTarget::Tab(idx) => {
                        go_to_tab(*idx as u32 + 1);
                        false
                    }
                    ClickTarget::Section(idx) => {
                        state.ui.focused_section = *idx;
                        state.ui.sections[*idx] = !state.ui.sections[*idx];
                        true
                    }
                }
            } else {
                false
            }
        }
        Mouse::ScrollUp(_) => {
            if state.config.role == PluginRole::Info && state.ui.focused_section > 0 {
                state.ui.focused_section -= 1;
                true
            } else {
                false
            }
        }
        Mouse::ScrollDown(_) => {
            if state.config.role == PluginRole::Info && state.ui.focused_section < NUM_SECTIONS - 1
            {
                state.ui.focused_section += 1;
                true
            } else {
                false
            }
        }
        // Hover, Hold, Release, RightClick — silently ignore
        Mouse::Hover(..) | Mouse::Hold(..) | Mouse::Release(..) | Mouse::RightClick(..) => false,
    }
}

pub fn handle_key(state: &mut SidebarState, key: KeyWithModifier) -> bool {
    let shifted = key.key_modifiers.contains(&KeyModifier::Shift);

    // Tab switching works in both roles
    if let BareKey::Char(c @ '1'..='9') = key.bare_key {
        if !shifted {
            let tab_idx = (c as u32 - '1' as u32) as usize;
            if tab_idx < state.tabs.len() {
                go_to_tab(tab_idx as u32 + 1);
            }
            return false;
        }
    }

    // Info panel gets full section navigation
    if state.config.role == PluginRole::Info {
        match key.bare_key {
            BareKey::Char('j') | BareKey::Down if !shifted => {
                if state.ui.focused_section < NUM_SECTIONS - 1 {
                    state.ui.focused_section += 1;
                }
                return true;
            }
            BareKey::Char('k') | BareKey::Up if !shifted => {
                if state.ui.focused_section > 0 {
                    state.ui.focused_section -= 1;
                }
                return true;
            }
            BareKey::Enter => {
                let idx = state.ui.focused_section;
                state.ui.sections[idx] = !state.ui.sections[idx];
                return true;
            }
            _ => {}
        }
    }

    // Shared keybinds
    match key.bare_key {
        BareKey::Char('r') if !shifted => {
            state.request_all_data();
            true
        }
        BareKey::Char('s') if !shifted => {
            open_session_manager();
            false
        }
        BareKey::Char('k') if shifted => {
            if let Some(target) = state.sessions.iter().find(|s| !s.attached && s.active) {
                let name = target.name.clone();
                let mut ctx = BTreeMap::new();
                ctx.insert("source".to_string(), "action".to_string());
                run_command_with_env_variables_and_cwd(
                    &["zellij", "kill-session", &name],
                    BTreeMap::new(),
                    PathBuf::from("/tmp"),
                    ctx,
                );
            }
            false
        }
        BareKey::Char('d') if shifted => {
            if let Some(target) = state.sessions.iter().find(|s| !s.active) {
                let name = target.name.clone();
                let mut ctx = BTreeMap::new();
                ctx.insert("source".to_string(), "action".to_string());
                run_command_with_env_variables_and_cwd(
                    &["zellij", "delete-session", &name],
                    BTreeMap::new(),
                    PathBuf::from("/tmp"),
                    ctx,
                );
            }
            false
        }
        _ => false,
    }
}

fn open_session_manager() {
    let mut ctx = BTreeMap::new();
    ctx.insert("source".to_string(), "action".to_string());
    run_command_with_env_variables_and_cwd(
        &[
            "zellij",
            "action",
            "launch-or-focus-plugin",
            "session-manager",
        ],
        BTreeMap::new(),
        PathBuf::from("/tmp"),
        ctx,
    );
}

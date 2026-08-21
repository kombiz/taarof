mod actions;
mod render;
mod state;

use state::SidebarState;
use std::collections::BTreeMap;
use zellij_tile::prelude::*;

register_plugin!(SidebarPlugin);

#[derive(Default)]
struct SidebarPlugin {
    state: SidebarState,
}

impl ZellijPlugin for SidebarPlugin {
    fn load(&mut self, config: BTreeMap<String, String>) {
        request_permission(&[
            PermissionType::RunCommands,
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
        ]);
        subscribe(&[
            EventType::Key,
            EventType::Mouse,
            EventType::Timer,
            EventType::RunCommandResult,
            EventType::PermissionRequestResult,
            EventType::SessionUpdate,
            EventType::TabUpdate,
            EventType::ModeUpdate,
        ]);
        self.state.load_config(config);
        set_timeout(0.5);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                self.state.request_all_data();
                true
            }
            Event::Timer(_) => {
                self.state.request_all_data();
                set_timeout(3.0);
                true
            }
            Event::RunCommandResult(exit_code, stdout, stderr, context) => {
                self.state
                    .handle_command_result(exit_code, stdout, stderr, context);
                true
            }
            Event::SessionUpdate(sessions, _) => {
                self.state.handle_session_update(sessions);
                true
            }
            Event::TabUpdate(tabs) => {
                self.state.handle_tab_update(tabs);
                true
            }
            Event::Key(key) => actions::handle_key(&mut self.state, key),
            Event::Mouse(mouse) => actions::handle_mouse(&mut self.state, mouse),
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        render::render_sidebar(&mut self.state, rows, cols);
    }
}

use crate::state::*;

// ── True-color palette (Catppuccin Mocha-inspired) ──
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const ITALIC: &str = "\x1b[3m";
const FG_TEXT: &str = "\x1b[38;2;205;214;244m"; // cdd6f4 — main text
const FG_SUBTEXT: &str = "\x1b[38;2;166;173;200m"; // a6adc8 — secondary
const FG_OVERLAY: &str = "\x1b[38;2;108;112;134m"; // 6c7086 — muted
const FG_GREEN: &str = "\x1b[38;2;166;227;161m"; // a6e3a1 — success
const FG_TEAL: &str = "\x1b[38;2;148;226;213m"; // 94e2d5 — accent
const FG_BLUE: &str = "\x1b[38;2;137;180;250m"; // 89b4fa — links
const FG_MAUVE: &str = "\x1b[38;2;203;166;247m"; // cba6f7 — highlight
const FG_PEACH: &str = "\x1b[38;2;250;179;135m"; // fab387 — warning
const FG_YELLOW: &str = "\x1b[38;2;249;226;175m"; // f9e2af — focus
const FG_RED: &str = "\x1b[38;2;243;139;168m"; // f38ba8 — error/alert

const BG_ACTIVE: &str = "\x1b[48;2;69;71;90m"; // 45475a — surface1
const BG_HIGHLIGHT: &str = "\x1b[48;2;88;91;112m"; // 585b70 — surface2

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

struct LineBuilder {
    lines: Vec<String>,
    targets: Vec<Option<ClickTarget>>,
}

impl LineBuilder {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            targets: Vec::new(),
        }
    }
    fn push(&mut self, line: String) {
        self.lines.push(line);
        self.targets.push(None);
    }
    fn push_target(&mut self, line: String, target: ClickTarget) {
        self.lines.push(line);
        self.targets.push(Some(target));
    }
    fn len(&self) -> usize {
        self.lines.len()
    }
}

pub fn render_sidebar(state: &mut SidebarState, rows: usize, cols: usize) {
    match state.config.role {
        PluginRole::Tabs => render_tabs_strip(state, rows, cols),
        PluginRole::Info => render_info_panel(state, rows, cols),
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// LEFT SIDEBAR: Vertical tab strip
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

fn render_tabs_strip(state: &mut SidebarState, rows: usize, cols: usize) {
    let w = cols.saturating_sub(1);
    let mut lb = LineBuilder::new();

    // Logo
    lb.push(format!("{FG_MAUVE}{BOLD}  taarof{RESET}"));
    lb.push(format!("{FG_OVERLAY}  ────────{RESET}"));

    // Tabs
    if state.tabs.is_empty() {
        lb.push(format!("  {FG_OVERLAY}{ITALIC}no tabs{RESET}"));
    } else {
        for t in &state.tabs {
            let idx = t.position + 1;
            let name = truncate(&t.name, w.saturating_sub(5));
            if t.active {
                lb.push_target(
                    format!("{BG_ACTIVE}{FG_TEAL}{BOLD} {idx} {name} {RESET}"),
                    ClickTarget::Tab(t.position),
                );
            } else {
                lb.push_target(
                    format!("  {FG_OVERLAY}{idx}{RESET} {FG_SUBTEXT}{name}{RESET}"),
                    ClickTarget::Tab(t.position),
                );
            }
        }
    }

    lb.push(String::new());

    // Session
    if !state.current_session.is_empty() {
        let sname = truncate(&state.current_session, w.saturating_sub(4));
        lb.push(format!("{FG_OVERLAY}  ────────{RESET}"));
        lb.push(format!("  {FG_GREEN}● {sname}{RESET}"));
    }

    // Running agents
    let running = state.agent_status.values().filter(|s| s.running).count();
    if running > 0 {
        lb.push(format!("  {FG_TEAL} {running} active{RESET}"));
    }

    // Fill + footer
    output(
        state,
        &lb,
        rows,
        cols,
        &format!("{FG_OVERLAY}  1-9 switch{RESET}"),
    );
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// RIGHT SIDEBAR: Info panel
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

fn render_info_panel(state: &mut SidebarState, rows: usize, cols: usize) {
    let w = cols.saturating_sub(1);
    let mut lb = LineBuilder::new();

    // ── SESSIONS ──
    section_header(&mut lb, SEC_SESSIONS, " SESSIONS", state);
    if state.ui.sections[SEC_SESSIONS] {
        if state.sessions.is_empty() {
            lb.push(format!("   {FG_OVERLAY}{ITALIC}no sessions{RESET}"));
        } else {
            for s in &state.sessions {
                let name = truncate(&s.name, w.saturating_sub(6));
                if s.attached {
                    lb.push(format!("   {FG_GREEN}{BOLD}● {name}{RESET}"));
                } else if s.active {
                    lb.push(format!("   {FG_SUBTEXT}○ {name}{RESET}"));
                } else {
                    lb.push(format!("   {FG_OVERLAY}◌ {name}{RESET}"));
                }
            }
        }
        lb.push(String::new());
    }

    // ── PROJECT ──
    section_header(&mut lb, SEC_PROJECT, " PROJECT", state);
    if state.ui.sections[SEC_PROJECT] {
        let cwd = if state.project.cwd.is_empty() {
            "…"
        } else {
            &state.project.cwd
        };
        lb.push(format!(
            "   {FG_TEXT}{BOLD}{}{RESET}",
            truncate(cwd, w.saturating_sub(3))
        ));
        if !state.project.branch.is_empty() {
            let mut info = state.project.branch.clone();
            if !state.project.commit.is_empty() {
                info.push_str(&format!(
                    " @ {}",
                    &state.project.commit[..7.min(state.project.commit.len())]
                ));
            }
            lb.push(format!(
                "   {FG_BLUE} {}{RESET}",
                truncate(&info, w.saturating_sub(5))
            ));
        }
        lb.push(String::new());
    }

    // ── AGENTS ──
    section_header(&mut lb, SEC_AGENTS, " AGENTS", state);
    if state.ui.sections[SEC_AGENTS] {
        if state.agents.is_empty() {
            lb.push(format!("   {FG_OVERLAY}{ITALIC}none configured{RESET}"));
        } else {
            for a in &state.agents {
                let running = state.agent_status.get(&a.name).is_some_and(|s| s.running);
                let label = if !a.label.is_empty() {
                    &a.label
                } else {
                    &a.name
                };
                let label = truncate(label, w.saturating_sub(6));
                if running {
                    lb.push(format!("   {FG_GREEN}{BOLD} {label}{RESET}"));
                } else {
                    lb.push(format!("   {FG_OVERLAY} {label}{RESET}"));
                }
            }
        }
        lb.push(String::new());
    }

    // ── FILES ──
    section_header(&mut lb, SEC_FILES, " FILES", state);
    if state.ui.sections[SEC_FILES] {
        if state.files.is_empty() {
            lb.push(format!("   {FG_OVERLAY}{ITALIC}empty{RESET}"));
        } else {
            for f in &state.files {
                let (icon, color) = file_icon_color(f);
                let name = truncate(f, w.saturating_sub(6));
                lb.push(format!("   {color}{icon}{RESET} {FG_TEXT}{name}{RESET}"));
            }
        }
        lb.push(String::new());
    }

    // ── ALERTS ──
    let alert_count = state.alerts.len();
    let alert_title = if alert_count > 0 {
        format!("{FG_RED} ALERTS ({alert_count})")
    } else {
        " ALERTS".to_string()
    };
    section_header_custom(&mut lb, SEC_ALERTS, &alert_title, state);
    if state.ui.sections[SEC_ALERTS] {
        if state.alerts.is_empty() {
            lb.push(format!("   {FG_OVERLAY}{ITALIC}all clear{RESET}"));
        } else {
            for a in &state.alerts {
                lb.push(format!(
                    "   {FG_PEACH}{BOLD}{}{RESET}",
                    truncate(&a.title, w.saturating_sub(3))
                ));
            }
        }
        lb.push(String::new());
    }

    // ── SHORTCUTS ──
    section_header(&mut lb, SEC_SHORTCUTS, " KEYS", state);
    if state.ui.sections[SEC_SHORTCUTS] {
        let shortcuts = [
            ("↑↓/jk", "navigate"),
            ("Enter", "toggle"),
            ("r", "refresh"),
            ("s", "sessions"),
            ("K", "kill sess"),
            ("D", "del sess"),
            ("", ""),
            ("Alt+←→", "pane"),
            ("Alt+n", "new pane"),
            ("Alt+t", "new tab"),
        ];
        for (key, desc) in &shortcuts {
            if key.is_empty() {
                lb.push(String::new());
            } else {
                lb.push(format!(
                    "   {FG_MAUVE}{key:<8}{RESET} {FG_OVERLAY}{desc}{RESET}"
                ));
            }
        }
        lb.push(String::new());
    }

    output(
        state,
        &lb,
        rows,
        cols,
        &format!("{FG_OVERLAY}  click ▸/▾ toggle{RESET}"),
    );
}

fn output(state: &mut SidebarState, lb: &LineBuilder, rows: usize, cols: usize, footer: &str) {
    let max_lines = if rows > 1 { rows - 1 } else { rows };

    state.ui.click_map = Vec::with_capacity(rows);
    for i in 0..rows {
        if i < lb.len() {
            state
                .ui
                .click_map
                .push(lb.targets.get(i).cloned().flatten());
        } else {
            state.ui.click_map.push(None);
        }
    }

    for (i, line) in lb.lines.iter().enumerate() {
        if i >= max_lines {
            break;
        }
        println!("{}", truncate(line, cols));
    }
    if rows > 1 {
        print!("{footer}");
    }
}

fn section_header(lb: &mut LineBuilder, idx: usize, title: &str, state: &SidebarState) {
    section_header_custom(lb, idx, title, state);
}

fn section_header_custom(lb: &mut LineBuilder, idx: usize, title: &str, state: &SidebarState) {
    let (arrow, bg) = if state.ui.sections[idx] {
        ("▾", "")
    } else {
        ("▸", "")
    };

    let line = if state.ui.focused_section == idx {
        format!("{BG_HIGHLIGHT} {FG_YELLOW}{BOLD}{arrow} {title}{RESET}{bg}")
    } else {
        format!(" {FG_TEAL}{BOLD}{arrow} {title}{RESET}{bg}")
    };
    lb.push_target(line, ClickTarget::Section(idx));
}

fn file_icon_color(name: &str) -> (&'static str, &'static str) {
    if name.ends_with('/') {
        return ("", FG_BLUE);
    }
    match name.rsplit('.').next() {
        Some("rs") => ("", FG_PEACH),
        Some("py") => ("", FG_YELLOW),
        Some("js") => ("", FG_YELLOW),
        Some("ts" | "tsx") => ("", FG_BLUE),
        Some("md") => ("", FG_SUBTEXT),
        Some("toml" | "yaml" | "yml" | "json" | "kdl") => ("", FG_TEAL),
        Some("sh" | "bash" | "zsh") => ("", FG_GREEN),
        Some("wasm") => ("", FG_MAUVE),
        Some("lock") => ("", FG_OVERLAY),
        Some("git" | "gitignore") => ("", FG_PEACH),
        _ => ("", FG_SUBTEXT),
    }
}

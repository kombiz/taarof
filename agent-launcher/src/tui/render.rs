use super::*;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
pub fn action_meaning(kind: ActionKind) -> &'static str {
    match kind {
        ActionKind::New => "New: start a new provider session",
        ActionKind::Attach => "Attach: focus or reconnect to the existing live process",
        ActionKind::Resume => "Resume: relaunch the provider using its saved history",
        ActionKind::Fork => "Fork: create a separate session from saved history",
    }
}
impl Picker {
    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(2),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(format!(
                "Search: {}\nProvider: {}   Scope: {:?}   Sort: {:?}",
                display_text(&self.query),
                self.provider_filter
                    .as_deref()
                    .map(display_text)
                    .unwrap_or_else(|| "all".into()),
                self.scope,
                self.sort
            ))
            .block(Block::default().title("agent sessions")),
            vertical[0],
        );
        let body = Layout::default()
            .direction(if area.width >= 90 {
                Direction::Horizontal
            } else {
                Direction::Vertical
            })
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(vertical[1]);
        let rows = self.rows();
        let mut items = Vec::new();
        let mut selected = None;
        let mut previous_section = "";
        for row in &rows {
            let section = if self.sort == Sort::Repository && row.section != "New" {
                if self.current_repository.is_none() {
                    "Last sent (newest first)"
                } else if row.current_repository {
                    "This repository (all worktrees)"
                } else {
                    "Other repositories / unknown"
                }
            } else if self.sort == Sort::LastMessage && row.section != "New" {
                "Last sent (newest first)"
            } else {
                row.section
            };
            if section != previous_section {
                items.push(
                    ListItem::new(section).style(Style::default().add_modifier(Modifier::BOLD)),
                );
                previous_section = section;
            }
            if Some(&row.key) == self.selected.as_ref() {
                selected = Some(items.len());
            }
            items.push(ListItem::new(row.label.clone()));
        }
        if rows.is_empty() {
            items.push(ListItem::new("No matching sessions"));
        }
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(
            List::new(items)
                .block(Block::default().borders(Borders::ALL).title("Sessions"))
                .highlight_symbol("> ")
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
            body[0],
            &mut state,
        );
        let mut preview = Vec::new();
        if self.help {
            preview.extend(
                [
                    "Enter: run declared default action",
                    "Up / Down: move selection",
                    "Type: fuzzy search; Backspace: edit",
                    "Tab: local / all hosts / active only",
                    "Ctrl+N: focus New rows",
                    "Ctrl+P: cycle provider filter",
                    "Ctrl+R: refresh catalog",
                    "Ctrl+S: repository / time / active groups",
                    "Last sent: your latest message; local time",
                    "Unknown: no reliable message time available",
                    "?: toggle this help",
                    "Esc / Ctrl+C: cancel",
                ]
                .map(|s| s.to_string()),
            );
            if self
                .selected_row()
                .is_some_and(|r| r.actions.iter().any(|a| a.kind == ActionKind::Fork))
            {
                preview.push("Ctrl+F: fork selected session".into());
            }
        } else if let Some(row) = self.selected_row() {
            preview.extend(row.metadata);
            preview.extend(
                row.actions
                    .iter()
                    .map(|a| action_meaning(a.kind).to_string()),
            );
            if row.actions.is_empty() {
                preview.push("No available action for this session.".into());
            }
        } else {
            preview.push("Choose a session to see metadata and available actions.".into());
        }
        for status in &self.catalog.providers {
            if !status.ok || status.warning.is_some() {
                preview.push(format!(
                    "{}: {}",
                    display_text(&status.name),
                    display_text(
                        status
                            .error
                            .as_deref()
                            .or(status.warning.as_deref())
                            .unwrap_or("History unavailable")
                    )
                ));
            }
        }
        for host in &self.catalog.remote_hosts {
            if !host.ok || host.stale {
                preview.push(format!(
                    "Host {}: {} — {}",
                    display_text(&host.host),
                    if host.stale { "stale" } else { "unavailable" },
                    display_text(
                        host.error
                            .as_deref()
                            .or(host.warning.as_deref())
                            .unwrap_or("Refresh pending")
                    )
                ));
            }
        }
        frame.render_widget(
            Paragraph::new(preview.into_iter().map(Line::from).collect::<Vec<_>>())
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(if self.help {
                    "Keys"
                } else {
                    "Metadata / actions"
                })),
            body[1],
        );
        let fork = if self
            .selected_row()
            .is_some_and(|r| r.actions.iter().any(|a| a.kind == ActionKind::Fork))
        {
            "  Ctrl+F fork"
        } else {
            ""
        };
        frame.render_widget(Paragraph::new(format!("Enter select  Ctrl+N new  Ctrl+P provider  Ctrl+R refresh  Ctrl+S sort  Tab scope  ? help  Esc cancel{fork}\n{}",display_text(&self.message))),vertical[2]);
    }
}

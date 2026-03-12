use orchestr8r_core::types::RunStatus;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use crate::app::{App, Mode};

pub fn render(f: &mut Frame, app: &App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(f.area());

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(26), Constraint::Min(1)])
        .split(outer[0]);

    render_runs(f, app, main[0]);
    render_logs(f, app, main[1]);
    render_status_bar(f, app, outer[1]);
}

fn render_runs(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .runs
        .iter()
        .map(|r| {
            let icon = match r.status {
                RunStatus::Running => ">",
                RunStatus::WaitingCheckpoint => "~",
                RunStatus::Completed => "+",
                RunStatus::Failed => "!",
            };
            ListItem::new(format!("{} {}", icon, r.workflow_name))
        })
        .collect();

    let mut state = ListState::default();
    if !app.runs.is_empty() {
        state.select(Some(app.selected_run));
    }

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Workflows"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    f.render_stateful_widget(list, area, &mut state);
}

fn render_logs(f: &mut Frame, app: &App, area: Rect) {
    let logs = app.selected_logs();
    let lines: Vec<Line> = logs
        .iter()
        .flat_map(|entry| {
            let time = entry.timestamp.format("%H:%M:%S").to_string();
            let prefix = format!("[{}] {}:", time, entry.step_type);
            let text = if !entry.stdout.is_empty() {
                &entry.stdout
            } else if !entry.stderr.is_empty() {
                &entry.stderr
            } else if let Some(ref fb) = entry.feedback {
                fb
            } else {
                ""
            };
            let first_line = text.lines().next().unwrap_or("");
            vec![Line::from(format!("{} {}", prefix, first_line))]
        })
        .collect();

    let scroll_offset = lines.len().saturating_sub(area.height as usize - 2) as u16;
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Logs"))
        .scroll((scroll_offset, 0));

    f.render_widget(para, area);
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let content = match app.mode {
        Mode::FeedbackInput => format!("Feedback: {}_", app.feedback_input),
        Mode::Normal => {
            if let Some(cp) = app.active_checkpoint() {
                let count = app.pending_checkpoints.len();
                let index_label = if count > 1 {
                    format!(
                        " ({}/{})",
                        app.selected_checkpoint + 1,
                        count
                    )
                } else {
                    String::new()
                };
                let nav = if count > 1 { "  [Tab] Next" } else { "" };
                if cp.feedback_available {
                    format!(
                        "CHECKPOINT{}: {}  [c] Continue  [s] Stop  [f] Feedback{}",
                        index_label, cp.message, nav
                    )
                } else {
                    format!(
                        "CHECKPOINT{}: {}  [c] Continue  [s] Stop{}",
                        index_label, cp.message, nav
                    )
                }
            } else {
                "[q] Quit  [Up/Down] Navigate".to_string()
            }
        }
    };

    // Truncate to available width (area width minus 2 for left/right borders)
    let available_width = area.width.saturating_sub(2) as usize;
    let truncated = if content.len() > available_width {
        format!("{}…", content.chars().take(available_width.saturating_sub(1)).collect::<String>())
    } else {
        content
    };

    let para = Paragraph::new(truncated).block(Block::default().borders(Borders::ALL));
    f.render_widget(para, area);
}

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    widgets::{Clear, Paragraph},
};

use crate::app::{App, Focus, Modal, Mode};
use crate::help_modal::render_help_modal;
use crate::right_panel::{render_right_panel, right_panel_hints};
use crate::runs_panel::{render_runs, left_panel_hints};
use crate::status_bar::{render_status_bar, PanelHint, StatusBarMode};
use crate::styles::base_style;
use crate::text::wrap_into_chunks;

fn centered_rect(width: Constraint, height: Constraint, area: Rect) -> Rect {
    let [_, v, _] = Layout::vertical([Constraint::Fill(1), height, Constraint::Fill(1)]).areas(area);
    let [_, h, _] = Layout::horizontal([Constraint::Fill(1), width, Constraint::Fill(1)]).areas(v);
    h
}

pub fn render(f: &mut Frame, app: &mut App) {
    f.render_widget(Paragraph::new("").style(base_style()), f.area());

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // top padding
            Constraint::Min(0),    // content
            Constraint::Length(1), // bottom padding
        ])
        .split(f.area());

    // render_header(f, rows[1]);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(2), // left padding
            Constraint::Min(0),    // content
            Constraint::Length(2), // right padding
        ])
        .split(rows[1]);

    let content = cols[1];

    let status_bar_height = if app.mode == Mode::FeedbackInput {
        let panel_width = content.width as usize;
        let overhead = 15;
        let available_width = panel_width.saturating_sub(overhead);
        let actual_width = available_width.max(20);
        let input_lines = if app.feedback_input.is_empty() {
            1
        } else {
            let wrapped = wrap_into_chunks(&app.feedback_input, actual_width);
            wrapped.len()
        };
        (2 + input_lines).min(7) as u16
    } else {
        3
    };

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(status_bar_height)])
        .split(content);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(26), Constraint::Min(1)])
        .split(inner[0]);

    render_runs(f, app, main[0]);
    render_right_panel(f, app, main[1]);

    if let Some(modal) = &mut app.modal {
        match modal {
            Modal::Help { scroll } => {
                let popup_area = centered_rect(
                    Constraint::Percentage(80),
                    Constraint::Percentage(80),
                    inner[0],
                );
                f.render_widget(Clear, popup_area);
                render_help_modal(f, popup_area, scroll);
            }
        }
    }

    let status_mode = if let Some(modal) = &app.modal {
        match modal {
            Modal::Help { .. } => {
                StatusBarMode::Modal {
                    hints: vec![PanelHint::new("[↑↓]", "Scroll")],
                    close: PanelHint::new("[Any]", "Close help"),
                    tick: app.tick,
                }
            }
        }
    } else {
        match app.mode {
            Mode::FeedbackInput => {
                let overhead = 15;
                let available_width = (inner[1].width as usize).saturating_sub(overhead);
                StatusBarMode::Prompt {
                    input: &app.feedback_input,
                    available_width,
                    tick: app.tick,
                }
            }
            Mode::Normal if app.focus == Focus::Right => StatusBarMode::Normal {
                panel_hints: right_panel_hints(app),
                other_checkpoints: 0,
                tick: app.tick,
            },
            Mode::Normal => match app.active_checkpoint() {
                Some(cp) => StatusBarMode::Action { feedback_available: cp.feedback_available },
                None => StatusBarMode::Normal {
                    panel_hints: left_panel_hints(app),
                    other_checkpoints: app.other_checkpoint_count(),
                    tick: app.tick,
                },
            },
        }
    };

    render_status_bar(f, status_mode, inner[1]);
}

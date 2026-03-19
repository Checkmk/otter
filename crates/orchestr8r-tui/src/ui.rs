use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    widgets::Paragraph,
};

use crate::app::{App, Focus, Mode};
use crate::right_panel::{render_right_panel, right_panel_hints};
use crate::runs_panel::{render_runs, left_panel_hints};
use crate::status_bar::{render_status_bar, StatusBarMode};
use crate::styles::base_style;
use crate::text::wrap_into_chunks;

pub fn render(f: &mut Frame, app: &mut App) {
    f.render_widget(Paragraph::new("").style(base_style()), f.area());

    let status_bar_height = if app.mode == Mode::FeedbackInput {
        let panel_width = f.area().width as usize;
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

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(status_bar_height)])
        .split(f.area());

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(26), Constraint::Min(1)])
        .split(outer[0]);

    render_runs(f, app, main[0]);
    render_right_panel(f, app, main[1]);

    let status_mode = match app.mode {
        Mode::FeedbackInput => {
            let overhead = 15;
            let available_width = (outer[1].width as usize).saturating_sub(overhead);
            StatusBarMode::Prompt {
                input: &app.feedback_input,
                available_width,
                tick: app.tick,
            }
        }
        Mode::Normal if app.focus == Focus::Right => StatusBarMode::Normal {
            panel_hints: right_panel_hints(app),
            other_checkpoints: 0,
        },
        Mode::Normal => match app.active_checkpoint() {
            Some(cp) => StatusBarMode::Action { feedback_available: cp.feedback_available },
            None => StatusBarMode::Normal {
                panel_hints: left_panel_hints(app),
                other_checkpoints: app.other_checkpoint_count(),
            },
        },
    };

    render_status_bar(f, status_mode, outer[1]);
}

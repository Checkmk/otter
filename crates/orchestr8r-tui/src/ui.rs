use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    widgets::Paragraph,
};

use crate::app::{App, Mode};
use crate::logs_panel::{render_logs, wrap_into_chunks};
use crate::runs_panel::render_runs;
use crate::status_bar::render_status_bar;
use crate::styles::base_style;

pub fn render(f: &mut Frame, app: &App) {
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
    render_logs(f, app, main[1]);
    render_status_bar(f, app, outer[1]);
}

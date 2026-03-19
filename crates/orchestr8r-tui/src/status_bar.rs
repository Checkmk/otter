use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::input_field::InputField;
use crate::styles::{
    base_style, c_action_continue, c_action_feedback, c_action_stop, c_background, c_dim,
    c_foreground, c_notice_waiting, c_waiting_cp, panel,
};

/// A single keybinding hint to display in the status bar.
pub struct PanelHint {
    pub key: &'static str,
    pub label: &'static str,
}

impl PanelHint {
    pub const fn new(key: &'static str, label: &'static str) -> Self {
        Self { key, label }
    }
}

pub enum StatusBarMode<'a> {
    /// Feedback text input active.
    Prompt { input: &'a str, available_width: usize, tick: u64 },
    /// Normal navigation: global hints + panel-provided hints.
    Normal { panel_hints: Vec<PanelHint>, other_checkpoints: usize },
    /// Checkpoint active: always show [q]/[Tab] + checkpoint actions.
    Action { feedback_available: bool },
}

pub fn render_status_bar(f: &mut Frame, mode: StatusBarMode<'_>, area: Rect) {
    let dim = Style::default().fg(c_dim()).bg(c_background());
    let key = Style::default().fg(c_foreground()).bg(c_background()).add_modifier(Modifier::BOLD);

    let global_hints = || -> Vec<Span<'static>> {
        vec![
            Span::styled("[q]", key),
            Span::styled(" Quit", dim),
            Span::styled("  ", base_style()),
            Span::styled("[Tab]", key),
            Span::styled(" Switch panel", dim),
            Span::styled("  ", base_style()),
            Span::styled("[↑↓]", key),
            Span::styled(" Navigate", dim),
        ]
    };

    let content = match mode {
        StatusBarMode::Prompt { input, available_width, tick } => {
            InputField::render(" Feedback ", input, available_width, tick)
        }
        StatusBarMode::Normal { panel_hints, other_checkpoints } => {
            let mut spans = global_hints();
            for hint in panel_hints {
                spans.push(Span::styled("  ", base_style()));
                spans.push(Span::styled(hint.key, key));
                spans.push(Span::styled(format!(" {}", hint.label), dim));
            }
            if other_checkpoints > 0 {
                let msg = if other_checkpoints == 1 {
                    "  · 1 other workflow waiting".to_string()
                } else {
                    format!("  · {} other workflows waiting", other_checkpoints)
                };
                spans.push(Span::styled(msg, Style::default().fg(c_notice_waiting()).bg(c_background())));
            }
            vec![Line::from(spans)]
        }
        StatusBarMode::Action { feedback_available } => {
            let mut spans = global_hints();
            spans.extend([
                Span::styled("  ", base_style()),
                Span::styled(" CHECKPOINT ", Style::default().fg(c_background()).bg(c_waiting_cp()).add_modifier(Modifier::BOLD)),
                Span::styled("  ", base_style()),
                Span::styled("[c]", Style::default().fg(c_action_continue()).bg(c_background()).add_modifier(Modifier::BOLD)),
                Span::styled(" Continue", Style::default().fg(c_action_continue()).bg(c_background())),
                Span::styled("  ", base_style()),
                Span::styled("[s]", Style::default().fg(c_action_stop()).bg(c_background()).add_modifier(Modifier::BOLD)),
                Span::styled(" Stop", Style::default().fg(c_action_stop()).bg(c_background())),
            ]);
            if feedback_available {
                spans.extend([
                    Span::styled("  ", base_style()),
                    Span::styled("[f]", Style::default().fg(c_action_feedback()).bg(c_background()).add_modifier(Modifier::BOLD)),
                    Span::styled(" Feedback", Style::default().fg(c_action_feedback()).bg(c_background())),
                ]);
            }
            vec![Line::from(spans)]
        }
    };

    let para = Paragraph::new(content).block(panel(""));
    f.render_widget(para, area);
}

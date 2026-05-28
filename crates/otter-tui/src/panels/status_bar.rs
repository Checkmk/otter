use std::borrow::Cow;

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::input_field::InputField;
use crate::scroll::scroll_spans;
use crate::styles::{base_style, panel};
use crate::theme;

/// A single keybinding hint to display in the status bar.
pub struct PanelHint {
    pub key: Cow<'static, str>,
    pub label: Cow<'static, str>,
}

impl PanelHint {
    pub const fn new(key: &'static str, label: &'static str) -> Self {
        Self {
            key: Cow::Borrowed(key),
            label: Cow::Borrowed(label),
        }
    }
    pub fn owned(key: impl Into<String>) -> Self {
        Self {
            key: Cow::Owned(key.into()),
            label: Cow::Borrowed(""),
        }
    }
}

pub enum StatusBarMode<'a> {
    /// Feedback text input active.
    Prompt {
        input: &'a str,
        available_width: usize,
        tick: u64,
    },
    /// Normal navigation: [?] Help hint + panel-provided hints.
    Normal {
        panel_hints: Vec<PanelHint>,
        other_checkpoints: usize,
        tick: u64,
        update_available: Option<&'a str>,
    },
    /// Checkpoint active: show only checkpoint actions (continue/stop/feedback).
    Action { feedback_available: bool },
    /// A modal overlay is open.
    Modal {
        hints: Vec<PanelHint>,
        close: PanelHint,
        tick: u64,
    },
}

pub fn render_status_bar(f: &mut Frame, mode: StatusBarMode<'_>, area: Rect) {
    let dim = Style::default()
        .fg(theme::current().dim)
        .bg(theme::current().background);
    let key = Style::default()
        .fg(theme::current().foreground)
        .bg(theme::current().background)
        .add_modifier(Modifier::BOLD);

    let block = panel("");
    let inner = block.inner(area);
    f.render_widget(block, area);

    match mode {
        StatusBarMode::Prompt {
            input,
            available_width,
            tick,
        } => {
            let lines = InputField::render(" Feedback ", input, available_width, tick);
            f.render_widget(Paragraph::new(lines).style(base_style()), inner);
        }
        StatusBarMode::Normal {
            panel_hints,
            other_checkpoints,
            tick,
            update_available,
        } => {
            let update_label = update_available.map(|v| format!(" ▲ v{} available ", v));
            let update_width = update_label.as_ref().map(|s| s.len() as u16).unwrap_or(0);
            let [left_area, update_area, right_area] = Layout::horizontal([
                Constraint::Min(0),
                Constraint::Length(update_width),
                Constraint::Length(10), // " [?] Help "
            ])
            .areas(inner);

            let mut left_spans: Vec<Span<'static>> = vec![];
            for (i, hint) in panel_hints.iter().enumerate() {
                let spacer = if i > 0 { "  " } else { " " };
                left_spans.push(Span::styled(spacer, base_style()));
                left_spans.push(Span::styled(hint.key.clone(), key));
                if !hint.label.is_empty() {
                    left_spans.push(Span::styled(format!(" {}", hint.label), dim));
                }
            }
            if other_checkpoints > 0 {
                let msg = if other_checkpoints == 1 {
                    "  · 1 other workflow waiting".to_string()
                } else {
                    format!("  · {} other workflows waiting", other_checkpoints)
                };
                left_spans.push(Span::styled(
                    msg,
                    Style::default()
                        .fg(theme::current().notice_waiting)
                        .bg(theme::current().background),
                ));
            }
            let left_spans = scroll_spans(left_spans, left_area.width as usize, tick);
            f.render_widget(
                Paragraph::new(vec![Line::from(left_spans)]).style(base_style()),
                left_area,
            );
            if let Some(label) = update_label {
                f.render_widget(
                    Paragraph::new(vec![Line::from(vec![Span::styled(
                        label,
                        Style::default()
                            .fg(theme::current().notice_waiting)
                            .bg(theme::current().background)
                            .add_modifier(Modifier::BOLD),
                    )])])
                    .style(base_style()),
                    update_area,
                );
            }
            f.render_widget(
                Paragraph::new(vec![Line::from(vec![
                    Span::styled(" ", base_style()),
                    Span::styled("[?]", key),
                    Span::styled(" Help", dim),
                    Span::styled(" ", base_style()),
                ])])
                .style(base_style()),
                right_area,
            );
        }
        StatusBarMode::Modal { hints, close, tick } => {
            let close_width = (close.key.len() + 1 + close.label.len() + 1) as u16;
            let [left_area, right_area] =
                Layout::horizontal([Constraint::Min(0), Constraint::Length(close_width)])
                    .areas(inner);

            let mut left_spans: Vec<Span<'static>> = vec![];
            for (i, hint) in hints.iter().enumerate() {
                let spacer = if i > 0 { "  " } else { " " };
                left_spans.push(Span::styled(spacer, base_style()));
                left_spans.push(Span::styled(hint.key.clone(), key));
                if !hint.label.is_empty() {
                    left_spans.push(Span::styled(format!(" {}", hint.label), dim));
                }
            }
            let left_spans = scroll_spans(left_spans, left_area.width as usize, tick);
            f.render_widget(
                Paragraph::new(vec![Line::from(left_spans)]).style(base_style()),
                left_area,
            );
            f.render_widget(
                Paragraph::new(vec![Line::from(vec![
                    Span::styled(close.key.clone(), key),
                    Span::styled(format!(" {}", close.label), dim),
                ])])
                .style(base_style()),
                right_area,
            );
        }
        StatusBarMode::Action { feedback_available } => {
            let mut spans = vec![
                Span::styled(
                    " CHECKPOINT ",
                    Style::default()
                        .fg(theme::current().background)
                        .bg(theme::current().waiting_cp)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ", base_style()),
                Span::styled(
                    "[c]",
                    Style::default()
                        .fg(theme::current().action_continue)
                        .bg(theme::current().background)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " Continue",
                    Style::default()
                        .fg(theme::current().action_continue)
                        .bg(theme::current().background),
                ),
                Span::styled("  ", base_style()),
                Span::styled(
                    "[s]",
                    Style::default()
                        .fg(theme::current().action_stop)
                        .bg(theme::current().background)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " Stop",
                    Style::default()
                        .fg(theme::current().action_stop)
                        .bg(theme::current().background),
                ),
            ];
            if feedback_available {
                spans.extend([
                    Span::styled("  ", base_style()),
                    Span::styled(
                        "[f]",
                        Style::default()
                            .fg(theme::current().action_feedback)
                            .bg(theme::current().background)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        " Feedback",
                        Style::default()
                            .fg(theme::current().action_feedback)
                            .bg(theme::current().background),
                    ),
                ]);
            }
            f.render_widget(
                Paragraph::new(vec![Line::from(spans)]).style(base_style()),
                inner,
            );
        }
    }
}

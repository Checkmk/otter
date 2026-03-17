use orchestr8r_core::types::{WorkflowState, WorkflowType};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::app::{App, CursorTarget, Focus, Mode, RightPanelContent};
use crate::input_field::InputField;
use crate::styles::{
    base_style, c_action_continue, c_action_feedback, c_action_stop, c_background, c_dim,
    c_foreground, c_notice_waiting, c_waiting_cp, panel,
};

pub fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let dim = Style::default().fg(c_dim()).bg(c_background());
    let key = Style::default().fg(c_foreground()).bg(c_background()).add_modifier(Modifier::BOLD);

    let content = match app.mode {
        Mode::FeedbackInput => {
            let overhead = 15;
            let available_width = (area.width as usize).saturating_sub(overhead);
            InputField::render(" Feedback ", &app.feedback_input, available_width, app.tick)
        }
        Mode::Normal if app.focus == Focus::Right => {
            match app.right_panel_content {
                RightPanelContent::Contextual => vec![Line::from(vec![
                    Span::styled("[Tab]", key),
                    Span::styled(" Switch panel", dim),
                    Span::styled("  ", base_style()),
                    Span::styled("[↑↓]", key),
                    Span::styled(" Scroll", dim),
                ])],
                RightPanelContent::ConsumedTriggers => vec![Line::from(vec![
                    Span::styled("[Tab]", key),
                    Span::styled(" Back", dim),
                    Span::styled("  ", base_style()),
                    Span::styled("[↑↓]", key),
                    Span::styled(" Navigate", dim),
                    Span::styled("  ", base_style()),
                    Span::styled("[Del]", key),
                    Span::styled(" Delete trigger", dim),
                ])],
            }
        }
        Mode::Normal => {
            if let Some(cp) = app.active_checkpoint() {
                let mut spans = vec![
                    Span::styled(" CHECKPOINT ", Style::default().fg(c_background()).bg(c_waiting_cp()).add_modifier(Modifier::BOLD)),
                    Span::styled("  ", base_style()),
                    Span::styled("[c]", Style::default().fg(c_action_continue()).bg(c_background()).add_modifier(Modifier::BOLD)),
                    Span::styled(" Continue", Style::default().fg(c_action_continue()).bg(c_background())),
                    Span::styled("  ", base_style()),
                    Span::styled("[s]", Style::default().fg(c_action_stop()).bg(c_background()).add_modifier(Modifier::BOLD)),
                    Span::styled(" Stop", Style::default().fg(c_action_stop()).bg(c_background())),
                ];
                if cp.feedback_available {
                    spans.extend([
                        Span::styled("  ", base_style()),
                        Span::styled("[f]", Style::default().fg(c_action_feedback()).bg(c_background()).add_modifier(Modifier::BOLD)),
                        Span::styled(" Feedback", Style::default().fg(c_action_feedback()).bg(c_background())),
                    ]);
                }
                vec![Line::from(spans)]
            } else {
                let mut spans: Vec<Span> = vec![
                    Span::styled("[q]", key),
                    Span::styled(" Quit", dim),
                    Span::styled("  ", base_style()),
                    Span::styled("[Tab]", key),
                    Span::styled(" Switch panel", dim),
                    Span::styled("  ", base_style()),
                    Span::styled("[↑↓]", key),
                    Span::styled(" Navigate", dim),
                ];

                if let CursorTarget::Workflow(wi) = app.cursor {
                    if let Some(entry) = app.workflows.get(wi) {
                        if entry.expanded {
                            spans.extend([
                                Span::styled("  ", base_style()),
                                Span::styled("[Space]", key),
                                Span::styled(" Hide runs", dim),
                            ]);
                        } else if !entry.runs.is_empty() {
                            spans.extend([
                                Span::styled("  ", base_style()),
                                Span::styled("[Space]", key),
                                Span::styled(" Show runs", dim),
                            ]);
                        }
                    }
                }

                if matches!(app.cursor, CursorTarget::Run(_, _)) {
                    spans.extend([
                        Span::styled("  ", base_style()),
                        Span::styled("[Del]", key),
                        Span::styled(" Delete run", dim),
                    ]);
                }

                let mut enter_spans: Vec<Span> = vec![];
                if let (Some(state), Some(kind)) = (
                    app.selected_workflow_state(),
                    app.selected_workflow_kind(),
                ) {
                    match state {
                        WorkflowState::Dormant => {
                            enter_spans.extend([
                                Span::styled("  ", base_style()),
                                Span::styled("[Enter]", key),
                                Span::styled(" Start workflow", dim),
                            ]);
                        }
                        WorkflowState::Running => {
                            if matches!(kind, WorkflowType::Looping) {
                                spans.extend([
                                    Span::styled("  ", base_style()),
                                    Span::styled("[p]", key),
                                    Span::styled(" Pause workflow", dim),
                                ]);
                            }
                            enter_spans.extend([
                                Span::styled("  ", base_style()),
                                Span::styled("[Enter]", key),
                                Span::styled(" Stop workflow", dim),
                            ]);
                        }
                        WorkflowState::Paused => {
                            spans.extend([
                                Span::styled("  ", base_style()),
                                Span::styled("[x]", key),
                                Span::styled(" Stop workflow", dim),
                            ]);
                            enter_spans.extend([
                                Span::styled("  ", base_style()),
                                Span::styled("[Enter]", key),
                                Span::styled(" Resume workflow", dim),
                            ]);
                        }
                    }
                }
                if app.cursor_is_polling_workflow() {
                    spans.extend([
                        Span::styled("  ", base_style()),
                        Span::styled("[T]", key),
                        Span::styled(" Consumed triggers", dim),
                    ]);
                }

                spans.extend(enter_spans);

                let other = app.other_checkpoint_count();
                if other > 0 {
                    let msg = if other == 1 {
                        "  · 1 other workflow waiting".to_string()
                    } else {
                        format!("  · {} other workflows waiting", other)
                    };
                    spans.push(Span::styled(msg, Style::default().fg(c_notice_waiting()).bg(c_background())));
                }
                vec![Line::from(spans)]
            }
        }
    };

    let para = Paragraph::new(content).block(panel(""));
    f.render_widget(para, area);
}

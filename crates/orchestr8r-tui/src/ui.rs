use orchestr8r_core::types::{RunStatus, WorkflowKind, WorkflowState};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use crate::app::{App, Mode};

// ─── Palette ─────────────────────────────────────────────────────────────────
const BG:     Color = Color::Rgb(0x33, 0x35, 0x43);
const PEACH:  Color = Color::Rgb(0xfb, 0xc9, 0x97);
const TAN:    Color = Color::Rgb(0xb7, 0x87, 0x60);
const GOLD:   Color = Color::Rgb(0xc8, 0x99, 0x31);
const ROSE:   Color = Color::Rgb(0xec, 0x84, 0x99);
const PURPLE: Color = Color::Rgb(0xa4, 0x59, 0xb7);
const GREEN:  Color = Color::Rgb(0x61, 0x8b, 0x50);
const BLUE:   Color = Color::Rgb(0x5b, 0x64, 0xc5);
const RED:    Color = Color::Rgb(0xff, 0x56, 0x38);

// ─── Semantic color mapping ───────────────────────────────────────────────────
fn c_background()      -> Color { BG     }
fn c_foreground()      -> Color { PEACH  }
fn c_dim()             -> Color { TAN    }
fn c_border()          -> Color { BLUE   }
fn c_running()         -> Color { GREEN  }
fn c_completed()       -> Color { GREEN  }
fn c_failed()          -> Color { RED    }
fn c_paused()          -> Color { GOLD   }
fn c_dormant()         -> Color { TAN    }
fn c_waiting_cp()      -> Color { GOLD   }
fn c_action_continue() -> Color { GREEN  }
fn c_action_stop()     -> Color { RED    }
fn c_action_feedback() -> Color { ROSE   }
fn c_notice_waiting()  -> Color { GOLD   }
fn c_step_agent()      -> Color { PURPLE }
fn c_step_shell()      -> Color { BLUE   }
fn c_step_checkpoint() -> Color { GOLD   }
fn c_step_notify()     -> Color { ROSE   }
fn c_step_other()      -> Color { TAN    }

fn step_color(step_type: &str) -> Color {
    match step_type {
        "agent"      => c_step_agent(),
        "shell"      => c_step_shell(),
        "checkpoint" => c_step_checkpoint(),
        "notify"     => c_step_notify(),
        _            => c_step_other(),
    }
}

// ─── Spinner ─────────────────────────────────────────────────────────────────
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner_frame(tick: u64) -> &'static str {
    SPINNER[(tick / 6) as usize % SPINNER.len()]
}

fn base_style() -> Style {
    Style::default().fg(c_foreground()).bg(c_background())
}

fn panel(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(c_border()).bg(c_background()))
        .title_style(Style::default().fg(c_foreground()).bg(c_background()).add_modifier(Modifier::BOLD))
        .title(title.to_string())
        .style(base_style())
}

pub fn render(f: &mut Frame, app: &App) {
    f.render_widget(Paragraph::new("").style(base_style()), f.area());

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

fn workflow_state_color(state: &WorkflowState, run_status: Option<&RunStatus>, tick: u64) -> (String, Color) {
    match state {
        WorkflowState::Paused  => ("=".to_string(), c_paused()),
        WorkflowState::Dormant => ("·".to_string(), c_dormant()),
        WorkflowState::Running => match run_status {
            Some(RunStatus::WaitingCheckpoint) => ("~".to_string(), c_waiting_cp()),
            Some(RunStatus::Completed)         => ("✓".to_string(), c_completed()),
            Some(RunStatus::Failed)            => ("✗".to_string(), c_failed()),
            _                                  => (spinner_frame(tick).to_string(), c_running()),
        },
    }
}

fn render_runs(f: &mut Frame, app: &App, area: Rect) {
    // inner width = panel minus two border columns, icon occupies 1 column on the right
    let inner_width = area.width.saturating_sub(2) as usize;
    let name_width  = inner_width.saturating_sub(1);

    let make_item = |name: &str, icon: String, icon_color: Color| {
        let char_count = name.chars().count();
        let padded = if char_count >= name_width {
            let truncated: String = name.chars().take(name_width.saturating_sub(1)).collect();
            format!("{}…", truncated)
        } else {
            format!("{:<width$}", name, width = name_width)
        };
        ListItem::new(Line::from(vec![
            Span::styled(padded, Style::default().fg(c_foreground()).bg(c_background())),
            Span::styled(icon,   Style::default().fg(icon_color).bg(c_background())),
        ]))
    };

    let items: Vec<ListItem> = if !app.registered.is_empty() {
        app.registered
            .iter()
            .map(|(name, _kind, state)| {
                let run_status = app.runs.iter().rev()
                    .find(|r| &r.workflow_name == name)
                    .map(|r| &r.status);
                let (icon, color) = workflow_state_color(state, run_status, app.tick);
                make_item(name, icon, color)
            })
            .collect()
    } else {
        app.runs
            .iter()
            .map(|r| {
                let (icon, color) = workflow_state_color(
                    &WorkflowState::Running, Some(&r.status), app.tick,
                );
                make_item(&r.workflow_name, icon, color)
            })
            .collect()
    };

    let mut state = ListState::default();
    if app.workflow_count() > 0 {
        state.select(Some(app.selected_run));
    }

    let list = List::new(items)
        .block(panel("Workflows"))
        .highlight_style(
            Style::default().fg(c_background()).bg(c_foreground()).add_modifier(Modifier::BOLD),
        );

    f.render_stateful_widget(list, area, &mut state);
}

fn render_logs(f: &mut Frame, app: &App, area: Rect) {
    let logs = app.selected_logs();
    let lines: Vec<Line> = logs
        .iter()
        .flat_map(|entry| {
            let time = entry.timestamp.format("%H:%M:%S").to_string();
            let text = if !entry.stdout.is_empty() {
                &entry.stdout
            } else if !entry.stderr.is_empty() {
                &entry.stderr
            } else if let Some(ref fb) = entry.feedback {
                fb
            } else {
                ""
            };
            let first_line = text.lines().next().unwrap_or("").replace('\r', "");
            vec![Line::from(vec![
                Span::styled(format!("[{}] ", time), Style::default().fg(c_dim()).bg(c_background())),
                Span::styled(
                    format!("{}:", entry.step_type),
                    Style::default().fg(step_color(&entry.step_type)).bg(c_background()),
                ),
                Span::styled(format!(" {}", first_line), base_style()),
            ])]
        })
        .collect();

    let scroll_offset = lines.len().saturating_sub(area.height as usize - 2) as u16;
    let para = Paragraph::new(lines)
        .block(panel("Logs"))
        .scroll((scroll_offset, 0));

    f.render_widget(para, area);
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let dim = Style::default().fg(c_dim()).bg(c_background());
    let key = Style::default().fg(c_foreground()).bg(c_background()).add_modifier(Modifier::BOLD);

    let line = match app.mode {
        Mode::FeedbackInput => Line::from(vec![
            Span::styled(" Feedback ", Style::default().fg(c_background()).bg(c_action_feedback()).add_modifier(Modifier::BOLD)),
            Span::styled("  ", base_style()),
            Span::styled(app.feedback_input.clone(), base_style()),
            Span::styled("_", Style::default().fg(c_action_feedback()).bg(c_background()).add_modifier(Modifier::SLOW_BLINK)),
        ]),
        Mode::Normal => {
            if let Some(cp) = app.active_checkpoint() {
                let mut spans = vec![
                    Span::styled(" CHECKPOINT ", Style::default().fg(c_background()).bg(c_waiting_cp()).add_modifier(Modifier::BOLD)),
                    Span::styled("  ", base_style()),
                    Span::styled(cp.message.clone(), base_style()),
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
                Line::from(spans)
            } else {
                let mut spans: Vec<Span> = vec![
                    Span::styled("[q]", key),
                    Span::styled(" Quit", dim),
                    Span::styled("  ", base_style()),
                    Span::styled("[↑↓]", key),
                    Span::styled(" Navigate", dim),
                ];
                if let (Some(state), Some(kind)) = (
                    app.selected_workflow_state(),
                    app.selected_workflow_kind(),
                ) {
                    match state {
                        WorkflowState::Dormant => {
                            spans.extend([
                                Span::styled("  ", base_style()),
                                Span::styled("[Enter]", key),
                                Span::styled(" Start", dim),
                            ]);
                        }
                        WorkflowState::Running => {
                            if matches!(kind, WorkflowKind::Indefinite) {
                                spans.extend([
                                    Span::styled("  ", base_style()),
                                    Span::styled("[p]", key),
                                    Span::styled(" Pause", dim),
                                ]);
                            }
                            spans.extend([
                                Span::styled("  ", base_style()),
                                Span::styled("[x]", key),
                                Span::styled(" Stop", dim),
                            ]);
                        }
                        WorkflowState::Paused => {
                            spans.extend([
                                Span::styled("  ", base_style()),
                                Span::styled("[Enter]", key),
                                Span::styled(" Resume", dim),
                                Span::styled("  ", base_style()),
                                Span::styled("[x]", key),
                                Span::styled(" Stop", dim),
                            ]);
                        }
                    }
                }
                let other = app.other_checkpoint_count();
                if other > 0 {
                    let msg = if other == 1 {
                        "  · 1 other workflow waiting".to_string()
                    } else {
                        format!("  · {} other workflows waiting", other)
                    };
                    spans.push(Span::styled(msg, Style::default().fg(c_notice_waiting()).bg(c_background())));
                }
                Line::from(spans)
            }
        }
    };

    let para = Paragraph::new(line).block(panel(""));
    f.render_widget(para, area);
}

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

fn wrap_into_chunks(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut chunks = Vec::new();
    let mut remaining = s;
    loop {
        let mut char_count = 0;
        let mut byte_end = remaining.len();
        for (byte_idx, _) in remaining.char_indices() {
            if char_count == width {
                byte_end = byte_idx;
                break;
            }
            char_count += 1;
        }
        chunks.push(remaining[..byte_end].to_string());
        remaining = &remaining[byte_end..];
        if remaining.is_empty() {
            break;
        }
    }
    chunks
}

// ─── Log layout ──────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum WrappedLogLine {
    /// The first screen line for an entry: carries the timestamp and step type.
    Header { time: String, step_type: String, text: String },
    /// A continuation line (wrapped or from a multi-line message body).
    Continuation { text: String },
}

/// Pure function: split a single log entry into the screen lines it occupies
/// given a panel content width (borders already excluded).
fn format_log_entry(
    time: &str,
    step_type: &str,
    text: &str,
    panel_width: usize,
) -> Vec<WrappedLogLine> {
    // "[HH:MM:SS] step_type: " — chars consumed by the header prefix
    let prefix_len = 1 + time.len() + 2 + step_type.len() + 2;
    let first_width = panel_width.saturating_sub(prefix_len);
    let cont_width  = panel_width.saturating_sub(2); // 2-space indent

    let mut result: Vec<WrappedLogLine> = Vec::new();
    let mut header_emitted = false;

    for raw in text.lines() {
        let content = raw.replace('\r', "");
        if !header_emitted {
            // Take only the first `first_width` chars for the header, then
            // re-split the remainder at cont_width (not first_width).
            let first: String = content.chars().take(first_width).collect();
            let rest:  String = content.chars().skip(first_width).collect();
            result.push(WrappedLogLine::Header {
                time: time.to_string(),
                step_type: step_type.to_string(),
                text: first,
            });
            header_emitted = true;
            if !rest.is_empty() {
                for chunk in wrap_into_chunks(&rest, cont_width) {
                    result.push(WrappedLogLine::Continuation { text: chunk });
                }
            }
        } else {
            for chunk in wrap_into_chunks(&content, cont_width) {
                result.push(WrappedLogLine::Continuation { text: chunk });
            }
        }
    }

    if !header_emitted {
        result.push(WrappedLogLine::Header {
            time: time.to_string(),
            step_type: step_type.to_string(),
            text: String::new(),
        });
    }

    result
}

fn render_logs(f: &mut Frame, app: &App, area: Rect) {
    let logs = app.selected_logs();
    let inner_width = area.width.saturating_sub(2) as usize;

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

            format_log_entry(&time, &entry.step_type, text, inner_width)
                .into_iter()
                .map(|wl| match wl {
                    WrappedLogLine::Header { time, step_type, text } => Line::from(vec![
                        Span::styled(format!("[{}] ", time), Style::default().fg(c_dim()).bg(c_background())),
                        Span::styled(format!("{}:", step_type), Style::default().fg(step_color(&step_type)).bg(c_background())),
                        Span::styled(format!(" {}", text), base_style()),
                    ]),
                    WrappedLogLine::Continuation { text } => Line::from(vec![
                        Span::styled("  ", base_style()),
                        Span::styled(text, base_style()),
                    ]),
                })
                .collect::<Vec<_>>()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_message_produces_single_header_line() {
        let lines = format_log_entry("06:00:00", "agent", "hello", 80);
        assert_eq!(lines, vec![
            WrappedLogLine::Header { time: "06:00:00".into(), step_type: "agent".into(), text: "hello".into() },
        ]);
    }

    #[test]
    fn long_message_wraps_to_continuation() {
        // panel_width=40, prefix "[06:00:00] agent: " = 18 chars → first_width=22, cont_width=38
        // 30-char message: first 22 go in the header, remaining 8 re-split at 38 → one continuation
        let msg = "a".repeat(30);
        let lines = format_log_entry("06:00:00", "agent", &msg, 40);
        assert_eq!(lines.len(), 2);
        assert!(matches!(&lines[0], WrappedLogLine::Header { text, .. } if text.len() == 22));
        assert!(matches!(&lines[1], WrappedLogLine::Continuation { text } if text.len() == 8));
    }

    #[test]
    fn long_message_continuation_uses_full_width() {
        // panel_width=40, first_width=22, cont_width=38
        // 80-char message: header gets 22, remaining 58 chars → ceil(58/38) = 2 continuations
        let msg = "a".repeat(80);
        let lines = format_log_entry("06:00:00", "agent", &msg, 40);
        assert_eq!(lines.len(), 3);
        assert!(matches!(&lines[0], WrappedLogLine::Header { text, .. } if text.len() == 22));
        assert!(matches!(&lines[1], WrappedLogLine::Continuation { text } if text.len() == 38));
        assert!(matches!(&lines[2], WrappedLogLine::Continuation { text } if text.len() == 20));
    }

    #[test]
    fn multiline_body_each_line_can_wrap() {
        let msg = "line one\nline two";
        let lines = format_log_entry("06:00:00", "shell", msg, 80);
        assert_eq!(lines.len(), 2);
        assert!(matches!(&lines[0], WrappedLogLine::Header { text, .. } if text == "line one"));
        assert!(matches!(&lines[1], WrappedLogLine::Continuation { text } if text == "line two"));
    }

    #[test]
    fn empty_text_produces_header_with_empty_text() {
        let lines = format_log_entry("06:00:00", "agent", "", 80);
        assert_eq!(lines, vec![
            WrappedLogLine::Header { time: "06:00:00".into(), step_type: "agent".into(), text: String::new() },
        ]);
    }

    #[test]
    fn carriage_returns_stripped() {
        let lines = format_log_entry("06:00:00", "shell", "ok\r", 80);
        assert!(matches!(&lines[0], WrappedLogLine::Header { text, .. } if text == "ok"));
    }
}

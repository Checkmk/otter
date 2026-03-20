use chrono::Local;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use orchestr8r_core::types::ProgressChunk;

use crate::app::{App, CursorTarget, Focus, RightPanelContent};
use crate::status_bar::PanelHint;
use crate::styles::{base_style, c_background, c_dim, c_foreground, panel, panel_focused, step_color};
use crate::text::wrap_into_chunks;


#[derive(Debug, PartialEq)]
enum WrappedLogLine {
    Header { time: String, step_type: String, text: String },
    Continuation { text: String },
}

fn format_log_entry(
    time: &str,
    step_type: &str,
    text: &str,
    panel_width: usize,
) -> Vec<WrappedLogLine> {
    // "[HH:MM:SS] step_type: " — chars consumed by the header prefix
    let prefix_len = 1 + time.len() + 2 + step_type.len() + 2;
    let first_width = panel_width.saturating_sub(prefix_len);
    let cont_width  = panel_width.saturating_sub(2);

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

fn with_scroll_indicators(lines: Vec<Line>, scroll_offset: usize, inner_height: usize) -> Vec<Line> {
    let total = lines.len();
    if total == 0 {
        return lines;
    }

    let above = scroll_offset;
    let below = total.saturating_sub(scroll_offset + inner_height);
    let dim = Style::default().fg(c_dim()).bg(c_background());

    let mut visible: Vec<Line> = lines.into_iter().skip(scroll_offset).take(inner_height).collect();

    if above > 0 && !visible.is_empty() {
        visible[0] = Line::from(Span::styled(format!("  … {above} more ↑"), dim));
    }
    if below > 0 && !visible.is_empty() {
        let last = visible.len() - 1;
        visible[last] = Line::from(Span::styled(format!("  … {below} more ↓"), dim));
    }

    visible
}

pub fn render_right_panel(f: &mut Frame, app: &mut App, area: Rect) {
    match &app.right_panel_content {
        RightPanelContent::ConsumedTriggers => {
            let is_focused = app.focus == Focus::Right;
            render_consumed_triggers(f, app, area, is_focused);
        }
        RightPanelContent::Contextual => {
            let is_focused = app.focus == Focus::Right;
            match app.cursor {
                CursorTarget::Workflow(_) => render_workflow_toml(f, app, area, is_focused),
                CursorTarget::Run(_, _) => render_logs(f, app, area, is_focused),
            }
        }
    }
}

fn render_workflow_toml(f: &mut Frame, app: &mut App, area: Rect, is_focused: bool) {
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;

    let lines: Vec<Line> = match app.selected_workflow_toml() {
        None => vec![Line::from(Span::styled(
            "No config available",
            Style::default().fg(c_dim()).bg(c_background()),
        ))],
        Some(toml) => toml
            .lines()
            .flat_map(|raw_line| {
                let line = raw_line.replace('\r', "");
                if inner_width == 0 || line.len() <= inner_width {
                    vec![Line::from(Span::styled(line, base_style()))]
                } else {
                    wrap_into_chunks(&line, inner_width)
                        .into_iter()
                        .map(|chunk| Line::from(Span::styled(chunk, base_style())))
                        .collect()
                }
            })
            .collect(),
    };

    let auto_bottom = lines.len().saturating_sub(inner_height);
    // Clamp app state so pressing ↑ always has a visible effect after scrolling down.
    app.right_scroll = app.right_scroll.min(auto_bottom);
    let scroll_offset = if is_focused { app.right_scroll } else { 0 } as u16;

    let visible = with_scroll_indicators(lines, scroll_offset as usize, inner_height);
    let block = if is_focused { panel_focused("Definition") } else { panel("Definition") };
    let para = Paragraph::new(visible).block(block);

    f.render_widget(para, area);
}

fn render_progress_line<'a>(
    chunk: &ProgressChunk,
    inner_width: usize,
    dim_style: Style,
    stderr_style: Style,
) -> Line<'a> {
    let (prefix, text, style) = match chunk {
        ProgressChunk::Status(s) => ("│ ", s.as_str(), dim_style),
        ProgressChunk::Stdout(s) => ("│ ", s.as_str(), dim_style),
        ProgressChunk::Stderr(s) => ("│ ", s.as_str(), stderr_style),
    };
    let max_text_width = inner_width.saturating_sub(4);
    let display: String = text.chars().take(max_text_width).collect();
    let suffix = if text.len() > max_text_width { "…" } else { "" };
    Line::from(vec![
        Span::styled(format!("  {prefix}"), dim_style),
        Span::styled(format!("{display}{suffix}"), style),
    ])
}

fn render_logs(f: &mut Frame, app: &mut App, area: Rect, is_focused: bool) {
    let logs = app.selected_logs();
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;

    let progress = app.selected_progress();
    let dim_style = Style::default().fg(c_dim()).bg(c_background());
    let stderr_style = Style::default().fg(ratatui::style::Color::Red).bg(c_background());

    let lines: Vec<Line> = if logs.is_empty() && progress.is_empty() {
        vec![Line::from(Span::styled(
            "Select a run to view logs",
            dim_style,
        ))]
    } else {
        let mut lines: Vec<Line> = Vec::new();
        let mut progress_cursor = 0;

        for entry in logs {
            let time = entry.timestamp.with_timezone(&Local).format("%H:%M:%S").to_string();
            let text = if !entry.stdout.is_empty() {
                &entry.stdout
            } else if !entry.stderr.is_empty() {
                &entry.stderr
            } else if let Some(ref fb) = entry.feedback {
                fb
            } else {
                ""
            };

            for wl in format_log_entry(&time, &entry.step_type, text, inner_width) {
                match wl {
                    WrappedLogLine::Header { time, step_type, text } => lines.push(Line::from(vec![
                        Span::styled(format!("[{}] ", time), Style::default().fg(c_dim()).bg(c_background())),
                        Span::styled(format!("{}:", step_type), Style::default().fg(step_color(&step_type)).bg(c_background())),
                        Span::styled(format!(" {}", text), base_style()),
                    ])),
                    WrappedLogLine::Continuation { text } => lines.push(Line::from(vec![
                        Span::styled("  ", base_style()),
                        Span::styled(text, base_style()),
                    ])),
                }
            }

            while progress_cursor < progress.len() && progress[progress_cursor].0 <= entry.step_index {
                lines.push(render_progress_line(&progress[progress_cursor].1, inner_width, dim_style, stderr_style));
                progress_cursor += 1;
            }
        }

        // Remaining progress chunks (for in-flight steps)
        while progress_cursor < progress.len() {
            lines.push(render_progress_line(&progress[progress_cursor].1, inner_width, dim_style, stderr_style));
            progress_cursor += 1;
        }

        lines
    };

    let auto_bottom = lines.len().saturating_sub(inner_height);
    // Clamp app state so pressing ↓ always has a visible effect after scrolling up.
    app.right_scroll = app.right_scroll.min(auto_bottom);
    let scroll_offset = if is_focused {
        auto_bottom - app.right_scroll
    } else {
        auto_bottom
    } as u16;

    let visible = with_scroll_indicators(lines, scroll_offset as usize, inner_height);
    let block = if is_focused { panel_focused("Run log") } else { panel("Run log") };
    let para = Paragraph::new(visible).block(block);

    f.render_widget(para, area);
}

fn render_consumed_triggers(f: &mut Frame, app: &mut App, area: Rect, is_focused: bool) {
    let triggers = app.selected_consumed_triggers();
    let block = if is_focused {
        panel_focused("Consumed Triggers")
    } else {
        panel("Consumed Triggers")
    };

    let lines: Vec<Line> = if triggers.is_empty() {
        vec![Line::from(Span::styled(
            "No consumed triggers",
            Style::default().fg(c_dim()).bg(c_background()),
        ))]
    } else {
        triggers.iter().enumerate().map(|(i, hash)| {
            if is_focused && i == app.right_cursor {
                Line::from(Span::styled(
                    hash.clone(),
                    Style::default().fg(c_background()).bg(c_foreground()),
                ))
            } else {
                Line::from(Span::styled(hash.clone(), base_style()))
            }
        }).collect()
    };

    // Scroll to keep right_cursor visible
    let inner_height = area.height.saturating_sub(2) as usize;
    let scroll_offset = if triggers.is_empty() || !is_focused {
        0
    } else {
        app.right_cursor.saturating_sub(inner_height.saturating_sub(1)) as u16
    };

    let para = Paragraph::new(lines)
        .block(block)
        .scroll((scroll_offset, 0));

    f.render_widget(para, area);
}

/// Returns the keybinding hints this panel contributes to the status bar.
pub fn right_panel_hints(app: &App) -> Vec<PanelHint> {
    match app.right_panel_content {
        RightPanelContent::Contextual => vec![],
        RightPanelContent::ConsumedTriggers => vec![
            PanelHint::new("[Del]", "Delete trigger"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn make_app() -> App {
        let (tx, _rx) = mpsc::channel(32);
        App::new(tx)
    }

    #[test]
    fn scroll_indicators_no_truncation() {
        // GIVEN lines that fit exactly in the viewport
        let lines: Vec<Line> = (0..3).map(|i| Line::from(format!("line {i}"))).collect();
        // WHEN no overflow
        let visible = with_scroll_indicators(lines, 0, 3);
        // THEN all lines returned unmodified
        assert_eq!(visible.len(), 3);
        assert_eq!(visible[0].spans[0].content, "line 0");
    }

    #[test]
    fn scroll_indicators_above_only() {
        // GIVEN 5 lines, scrolled to show lines 2-4 (2 above)
        let lines: Vec<Line> = (0..5).map(|i| Line::from(format!("line {i}"))).collect();
        // WHEN 2 above, 0 below
        let visible = with_scroll_indicators(lines, 2, 3);
        // THEN first line replaced with "above" indicator
        assert_eq!(visible.len(), 3);
        assert!(visible[0].spans[0].content.contains("2 more ↑"));
        assert_eq!(visible[2].spans[0].content, "line 4");
    }

    #[test]
    fn scroll_indicators_below_only() {
        // GIVEN 5 lines, showing first 3 (2 below)
        let lines: Vec<Line> = (0..5).map(|i| Line::from(format!("line {i}"))).collect();
        // WHEN 0 above, 2 below
        let visible = with_scroll_indicators(lines, 0, 3);
        // THEN last line replaced with "below" indicator
        assert_eq!(visible.len(), 3);
        assert_eq!(visible[0].spans[0].content, "line 0");
        assert!(visible[2].spans[0].content.contains("2 more ↓"));
    }

    #[test]
    fn scroll_indicators_both_sides() {
        // GIVEN 7 lines, showing middle 3 (2 above, 2 below)
        let lines: Vec<Line> = (0..7).map(|i| Line::from(format!("line {i}"))).collect();
        // WHEN 2 above, 2 below
        let visible = with_scroll_indicators(lines, 2, 3);
        // THEN both first and last lines are indicators
        assert!(visible[0].spans[0].content.contains("2 more ↑"));
        assert!(visible[2].spans[0].content.contains("2 more ↓"));
    }

    #[test]
    fn right_panel_hints_consumed_triggers_shows_delete() {
        // GIVEN consumed triggers panel content
        let mut app = make_app();
        app.right_panel_content = RightPanelContent::ConsumedTriggers;

        // WHEN
        let hints = right_panel_hints(&app);

        // THEN delete hints
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].key, "[Del]");
    }

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

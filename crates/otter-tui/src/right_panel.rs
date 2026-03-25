use chrono::Local;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use otter_core::types::ProgressChunk;

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

pub fn with_scroll_indicators(lines: Vec<Line>, scroll_offset: usize, inner_height: usize) -> Vec<Line> {
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
            let is_focused = app.modal.is_none() && app.focus == Focus::Right;
            render_consumed_triggers(f, app, area, is_focused);
        }
        RightPanelContent::Contextual => {
            let is_focused = app.modal.is_none() && app.focus == Focus::Right;
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

fn render_progress_lines<'a>(
    chunk: &ProgressChunk,
    inner_width: usize,
    dim_style: Style,
    stderr_style: Style,
) -> Vec<Line<'a>> {
    let (prefix, text, style) = match chunk {
        ProgressChunk::Status(s) => ("│ ", s.as_str(), dim_style),
        ProgressChunk::Stdout(s) => ("│ ", s.as_str(), dim_style),
        ProgressChunk::Stderr(s) => ("│ ", s.as_str(), stderr_style),
    };
    // "  │ " = 4 chars of prefix per line
    let text_width = inner_width.saturating_sub(4);
    let mut result = Vec::new();
    for raw in text.lines() {
        let content = raw.replace('\r', "");
        if text_width == 0 || content.is_empty() {
            result.push(Line::from(vec![
                Span::styled(format!("  {prefix}"), dim_style),
                Span::styled(content, style),
            ]));
        } else {
            for chunk in wrap_into_chunks(&content, text_width) {
                result.push(Line::from(vec![
                    Span::styled(format!("  {prefix}"), dim_style),
                    Span::styled(chunk, style),
                ]));
            }
        }
    }
    if result.is_empty() {
        result.push(Line::from(Span::styled(format!("  {prefix}"), dim_style)));
    }
    result
}

fn build_log_lines<'a>(
    logs: &[otter_core::types::LogEntry],
    progress: &[(usize, ProgressChunk)],
    inner_width: usize,
    dim_style: Style,
    stderr_style: Style,
) -> Vec<Line<'a>> {
    if logs.is_empty() && progress.is_empty() {
        return vec![Line::from(Span::styled(
            "Select a run to view logs",
            dim_style,
        ))];
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut progress_cursor = 0;

    for (i, entry) in logs.iter().enumerate() {
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

        // If this is a subsequent log entry for the same step, flush pending progress
        // BEFORE emitting it so progress appears between the first and last log entries
        // of the same step (e.g. "Running agent..." → progress → final output).
        if i > 0 && logs[i - 1].step_index == entry.step_index {
            while progress_cursor < progress.len() && progress[progress_cursor].0 <= entry.step_index {
                lines.extend(render_progress_lines(&progress[progress_cursor].1, inner_width, dim_style, stderr_style));
                progress_cursor += 1;
            }
        }

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

        // Emit progress between step groups: when the next log has a higher
        // step_index, drain progress chunks that belong before it.
        // This prevents run_start (step_index usize::MAX) from absorbing all progress.
        let next_step = logs.get(i + 1).map(|e| e.step_index);
        let boundary = next_step.unwrap_or(usize::MAX);
        while progress_cursor < progress.len() && progress[progress_cursor].0 < boundary {
            lines.extend(render_progress_lines(&progress[progress_cursor].1, inner_width, dim_style, stderr_style));
            progress_cursor += 1;
        }
    }

    // Remaining progress chunks (for in-flight steps)
    while progress_cursor < progress.len() {
        lines.extend(render_progress_lines(&progress[progress_cursor].1, inner_width, dim_style, stderr_style));
        progress_cursor += 1;
    }

    lines
}

fn render_logs(f: &mut Frame, app: &mut App, area: Rect, is_focused: bool) {
    let logs = app.selected_logs();
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;

    let progress = app.selected_progress();
    let dim_style = Style::default().fg(c_dim()).bg(c_background());
    let stderr_style = Style::default().fg(ratatui::style::Color::Red).bg(c_background());

    let lines: Vec<Line> = build_log_lines(logs, progress, inner_width, dim_style, stderr_style);

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
        RightPanelContent::Contextual => vec![PanelHint::new("[↑↓]", "Scroll")],
        RightPanelContent::ConsumedTriggers => vec![
            PanelHint::new("[Del]", "Delete trigger"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use otter_core::types::LogEntry;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    fn make_app() -> App {
        let (tx, _rx) = mpsc::channel(32);
        App::new(tx)
    }

    fn make_log(step_index: usize, step_type: &str, stdout: &str) -> LogEntry {
        LogEntry {
            run_id: Uuid::nil(),
            iteration: 0,
            step_index,
            step_type: step_type.to_string(),
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: None,
            accepted: None,
            feedback: None,
            timestamp: Utc::now(),
        }
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn is_progress_line(line: &Line) -> bool {
        // Progress lines start with "  │ "
        line_text(line).starts_with("  \u{2502} ")
    }

    #[test]
    fn progress_appears_before_completion_log_not_after() {
        // GIVEN an agent step with two log entries (start + completion) and progress in between
        let dim = Style::default();
        let red = Style::default();
        let logs = vec![
            make_log(usize::MAX, "run_start", "Run started"),
            make_log(0, "agent", "Running claude agent..."),
            make_log(0, "agent", "Final output"),
            make_log(1, "shell", "Next step"),
        ];
        let progress = vec![
            (0, ProgressChunk::Status("Thinking...".to_string())),
            (0, ProgressChunk::Status("Using tool: Read".to_string())),
        ];

        // WHEN
        let lines = build_log_lines(&logs, &progress, 80, dim, red);

        // THEN progress lines appear after "Running claude agent..." and before "Final output"
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let idx_start   = texts.iter().position(|t| t.contains("Running claude agent...")).unwrap();
        let idx_prog0   = texts.iter().position(|t| t.contains("Thinking...")).unwrap();
        let idx_prog1   = texts.iter().position(|t| t.contains("Using tool: Read")).unwrap();
        let idx_final   = texts.iter().position(|t| t.contains("Final output")).unwrap();
        let idx_next    = texts.iter().position(|t| t.contains("Next step")).unwrap();

        assert!(idx_start < idx_prog0, "progress must come after agent start");
        assert!(idx_prog0 < idx_prog1, "progress chunks preserve order");
        assert!(idx_prog1 < idx_final, "progress must come before final output");
        assert!(idx_final < idx_next,  "final output before next step");
    }

    #[test]
    fn progress_appears_after_start_log_when_step_in_flight() {
        // GIVEN an agent step with only its start log (not yet completed) and live progress
        let dim = Style::default();
        let red = Style::default();
        let logs = vec![
            make_log(usize::MAX, "run_start", "Run started"),
            make_log(0, "agent", "Running claude agent..."),
        ];
        let progress = vec![
            (0, ProgressChunk::Status("Thinking...".to_string())),
        ];

        // WHEN
        let lines = build_log_lines(&logs, &progress, 80, dim, red);

        // THEN progress appears after the start log (in-flight position)
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let idx_start = texts.iter().position(|t| t.contains("Running claude agent...")).unwrap();
        let idx_prog  = texts.iter().position(|t| t.contains("Thinking...")).unwrap();

        assert!(idx_start < idx_prog);
        assert!(is_progress_line(&lines[idx_prog]));
    }

    #[test]
    fn long_progress_chunk_wraps_to_multiple_lines() {
        // GIVEN a progress chunk whose text exceeds the panel width
        let dim = Style::default();
        let red = Style::default();
        // inner_width=20, prefix="  │ "=4 chars → text_width=16
        let long_text = "a".repeat(40);
        let chunk = ProgressChunk::Status(long_text.clone());

        // WHEN
        let lines = render_progress_lines(&chunk, 20, dim, red);

        // THEN multiple lines are returned, each with the progress prefix
        assert!(lines.len() > 1, "expected wrapping but got {} line(s)", lines.len());
        for line in &lines {
            assert!(is_progress_line(line), "each wrapped line must have the progress prefix");
        }
        // All characters of the original text are preserved across lines
        let total_text: String = lines.iter()
            .map(|l| line_text(l).trim_start_matches("  │ ").to_string())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(total_text, long_text);
    }

    #[test]
    fn multiline_progress_chunk_renders_each_line_with_prefix() {
        // GIVEN a progress chunk with embedded newlines
        let dim = Style::default();
        let red = Style::default();
        let chunk = ProgressChunk::Stdout("line one\nline two".to_string());

        // WHEN
        let lines = render_progress_lines(&chunk, 80, dim, red);

        // THEN two lines, both with prefix
        assert_eq!(lines.len(), 2);
        assert!(line_text(&lines[0]).contains("line one"));
        assert!(line_text(&lines[1]).contains("line two"));
        assert!(is_progress_line(&lines[0]));
        assert!(is_progress_line(&lines[1]));
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

use chrono::Local;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::app::App;
use crate::styles::{base_style, c_background, c_dim, panel, step_color};

pub fn wrap_into_chunks(s: &str, width: usize) -> Vec<String> {
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

pub fn render_logs(f: &mut Frame, app: &App, area: Rect) {
    let logs = app.selected_logs();
    let inner_width = area.width.saturating_sub(2) as usize;

    let lines: Vec<Line> = if logs.is_empty() {
        vec![Line::from(Span::styled(
            "Select a run to view logs",
            Style::default().fg(c_dim()).bg(c_background()),
        ))]
    } else {
        logs.iter()
            .flat_map(|entry| {
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
            .collect()
    };

    let scroll_offset = lines.len().saturating_sub(area.height as usize - 2) as u16;
    let para = Paragraph::new(lines)
        .block(panel("Logs"))
        .scroll((scroll_offset, 0));

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

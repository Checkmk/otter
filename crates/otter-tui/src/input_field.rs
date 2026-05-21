use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::styles::base_style;
use crate::text::wrap_into_chunks;
use crate::theme;

#[derive(Copy, Clone)]
pub struct RenderConfig {
    pub cursor_blink_speed: u64,
}

impl Default for RenderConfig {
    fn default() -> Self {
        RenderConfig {
            cursor_blink_speed: 40, // cursor blinks every ~667ms at 60fps
        }
    }
}

pub struct InputField;

impl InputField {
    pub fn render(
        badge: &str,
        text: &str,
        available_width: usize,
        tick: u64,
    ) -> Vec<Line<'static>> {
        Self::render_with_config(badge, text, available_width, tick, RenderConfig::default())
    }

    fn render_with_config(
        badge: &str,
        text: &str,
        available_width: usize,
        tick: u64,
        config: RenderConfig,
    ) -> Vec<Line<'static>> {
        let min_width = 20;
        let actual_width = available_width.max(min_width);
        let indent = " ".repeat(badge.len() + 1);

        let wrapped = if text.is_empty() {
            vec![String::new()]
        } else {
            wrap_into_chunks(text, actual_width)
        };

        let mut lines = Vec::new();
        let cursor_visible = (tick / config.cursor_blink_speed).is_multiple_of(2);

        for (idx, line_text) in wrapped.iter().enumerate() {
            let mut spans: Vec<Span> = if idx == 0 {
                vec![
                    Span::styled(
                        badge.to_string(),
                        Style::default()
                            .fg(theme::current().background)
                            .bg(theme::current().action_feedback)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" ", base_style()),
                    Span::styled(line_text.clone(), base_style()),
                ]
            } else {
                vec![
                    Span::styled(indent.clone(), base_style()),
                    Span::styled(line_text.clone(), base_style()),
                ]
            };

            if idx == wrapped.len() - 1 {
                let cursor_char = if cursor_visible { "█" } else { " " };
                spans.push(Span::styled(
                    cursor_char,
                    Style::default()
                        .bg(theme::current().action_feedback)
                        .fg(theme::current().background),
                ));
            }

            lines.push(Line::from(spans));
        }

        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_empty_input_produces_single_line_with_badge() {
        let lines = InputField::render(" Input ", "", 65, 0);
        assert_eq!(lines.len(), 1);

        let line = &lines[0];
        assert_eq!(line.spans.len(), 4);
        assert_eq!(line.spans[0].content, " Input ");
        assert_eq!(line.spans[1].content, " ");
        assert_eq!(line.spans[2].content, "");
        assert_eq!(line.spans[3].content, "█");
    }

    #[test]
    fn render_short_input_fits_single_line() {
        let lines = InputField::render(" Input ", "hello", 65, 0);
        assert_eq!(lines.len(), 1);

        let line = &lines[0];
        assert_eq!(line.spans.len(), 4);
        assert_eq!(line.spans[2].content, "hello");
        assert_eq!(line.spans[3].content, "█");
    }

    #[test]
    fn render_long_input_wraps_to_continuation() {
        let text = "a".repeat(30);
        let lines = InputField::render(" Input ", &text, 25, 0);
        assert_eq!(lines.len(), 2);

        assert_eq!(lines[0].spans.len(), 3);
        assert_eq!(lines[0].spans[2].content.len(), 25);

        assert_eq!(lines[1].spans.len(), 3);
        assert_eq!(lines[1].spans[0].content, "        ");
        assert_eq!(lines[1].spans[1].content.len(), 5);
        assert_eq!(lines[1].spans[2].content, "█");
    }

    #[test]
    fn render_very_long_input_wraps_multiple_times() {
        let text = "a".repeat(100);
        let lines = InputField::render(" Input ", &text, 35, 0);
        assert!(lines.len() > 2, "Long input should wrap to multiple lines");

        assert_eq!(lines[0].spans.len(), 3);
        assert_eq!(lines[0].spans[2].content.len(), 35);

        for (idx, line) in lines[1..lines.len() - 1].iter().enumerate() {
            assert_eq!(
                line.spans.len(),
                2,
                "Intermediate continuation line {} should have 2 spans",
                idx + 1
            );
            assert_eq!(line.spans[0].content, "        ");
        }

        let last_line = &lines[lines.len() - 1];
        assert_eq!(last_line.spans.len(), 3);
        assert_eq!(last_line.spans[0].content, "        ");
        assert_eq!(last_line.spans[2].content, "█");
    }

    #[test]
    fn render_enforces_minimum_width_for_very_small_panels() {
        let text = "hello world".to_string();
        let lines = InputField::render(" Input ", &text, 5, 0);

        assert_eq!(lines.len(), 1);
        assert!(lines[0].spans[2].content.contains("hello"));
    }

    #[test]
    fn render_continuation_lines_have_correct_indentation() {
        let text = "a".repeat(60);
        let lines = InputField::render(" Input ", &text, 35, 0);

        for line in &lines[1..] {
            assert_eq!(line.spans[0].content, "        ");
        }
    }

    #[test]
    fn render_cursor_only_on_last_line() {
        let text = "a".repeat(100);
        let lines = InputField::render(" Input ", &text, 35, 0);

        assert_eq!(lines[0].spans.len(), 3);
        assert!(!lines[0].spans.iter().any(|s| s.content == "█"));

        for line in &lines[1..lines.len() - 1] {
            assert_eq!(
                line.spans.len(),
                2,
                "Intermediate lines should only have indent + text"
            );
        }

        let last_line = &lines[lines.len() - 1];
        assert_eq!(last_line.spans.len(), 3);
        assert_eq!(last_line.spans[2].content, "█");
    }

    #[test]
    fn render_preserves_text_exactly() {
        let text = "hello world test";
        let lines = InputField::render(" Input ", text, 65, 0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[2].content, "hello world test");
    }

    #[test]
    fn render_wraps_at_panel_width_boundary() {
        let text = "a".repeat(26);
        let lines = InputField::render(" Input ", &text, 25, 0);
        assert_eq!(lines.len(), 2);

        assert_eq!(lines[0].spans.len(), 3);
        assert_eq!(lines[0].spans[2].content.len(), 25);

        assert_eq!(lines[1].spans.len(), 3);
        assert_eq!(lines[1].spans[1].content.len(), 1);
        assert_eq!(lines[1].spans[2].content, "█");
    }

    #[test]
    fn render_exact_fit_does_not_wrap() {
        let text = "a".repeat(25);
        let lines = InputField::render(" Input ", &text, 25, 0);
        assert_eq!(lines.len(), 1);

        assert_eq!(lines[0].spans.len(), 4);
        assert_eq!(lines[0].spans[3].content, "█");
    }

    #[test]
    fn render_cursor_toggles_visibility_based_on_tick() {
        let text = "hello";
        let config = RenderConfig {
            cursor_blink_speed: 1,
        };

        let lines_visible = InputField::render_with_config(" Input ", text, 65, 0, config);
        assert_eq!(lines_visible[0].spans[3].content, "█");

        let lines_hidden = InputField::render_with_config(" Input ", text, 65, 1, config);
        assert_eq!(lines_hidden[0].spans[3].content, " ");

        let lines_visible_again = InputField::render_with_config(" Input ", text, 65, 2, config);
        assert_eq!(lines_visible_again[0].spans[3].content, "█");
    }
}

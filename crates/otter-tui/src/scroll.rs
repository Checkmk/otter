use ratatui::text::Span;

#[derive(Copy, Clone)]
pub struct ScrollConfig {
    pub scroll_speed: u64,
    pub pause_duration: u64,
}

impl Default for ScrollConfig {
    fn default() -> Self {
        ScrollConfig {
            scroll_speed: 4,
            pause_duration: 60,
        }
    }
}

/// Scrolls styled spans left and right when their total length exceeds `width`.
/// When they fit, returns them unchanged. When they don't, animates a smooth
/// scroll: pauses at start, scrolls left, pauses at end, scrolls right, repeats.
/// Truncated sides get a "…" marker that inherits the style of the nearest span.
pub fn scroll_spans(spans: Vec<Span<'static>>, width: usize, tick: u64) -> Vec<Span<'static>> {
    let text_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let (offset, has_left, has_right, content_width) =
        scroll_window_with_config(text_len, width, tick, ScrollConfig::default());

    if !has_left && !has_right {
        return spans;
    }

    let start = offset;
    let end = offset + content_width;
    let mut result: Vec<Span<'static>> = Vec::new();
    let mut char_pos: usize = 0;

    if has_left {
        let style = spans
            .iter()
            .find(|s| !s.content.is_empty())
            .map(|s| s.style)
            .unwrap_or_default();
        result.push(Span::styled("…", style));
    }

    for span in spans {
        let span_len = span.content.chars().count();
        let span_end = char_pos + span_len;

        if span_end <= start || char_pos >= end {
            char_pos = span_end;
            continue;
        }

        let slice_start = start.saturating_sub(char_pos);
        let slice_end = (end - char_pos).min(span_len);
        let sliced: String = span
            .content
            .chars()
            .skip(slice_start)
            .take(slice_end - slice_start)
            .collect();
        if !sliced.is_empty() {
            result.push(Span::styled(sliced, span.style));
        }

        char_pos = span_end;
    }

    if has_right {
        let style = result.last().map(|s| s.style).unwrap_or_default();
        result.push(Span::styled("…", style));
    }

    result
}

fn scroll_window_with_config(
    text_len: usize,
    width: usize,
    tick: u64,
    config: ScrollConfig,
) -> (usize, bool, bool, usize) {
    if text_len <= width {
        return (0, false, false, text_len);
    }

    let scroll_range = text_len - width + 1;
    let cycle_length = (config.pause_duration
        + scroll_range as u64 * config.scroll_speed
        + config.pause_duration
        + scroll_range as u64 * config.scroll_speed) as u64;
    let phase = tick % cycle_length;

    let offset = if phase < config.pause_duration {
        0
    } else if phase < config.pause_duration + scroll_range as u64 * config.scroll_speed {
        ((phase - config.pause_duration) / config.scroll_speed) as usize
    } else if phase
        < config.pause_duration + scroll_range as u64 * config.scroll_speed + config.pause_duration
    {
        scroll_range
    } else {
        let progress = (phase
            - config.pause_duration
            - scroll_range as u64 * config.scroll_speed
            - config.pause_duration)
            / config.scroll_speed;
        (scroll_range as u64 - progress) as usize
    };

    let has_left = offset > 0;
    let has_right = offset < scroll_range;
    let content_width = match (has_left, has_right) {
        (true, true) => width.saturating_sub(2),
        (true, false) | (false, true) => width.saturating_sub(1),
        (false, false) => width,
    };

    (offset, has_left, has_right, content_width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    fn spans_text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn scroll_spans_fits_without_scrolling() {
        let spans = scroll_spans(vec![Span::raw("short")], 10, 0);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "short");
    }

    #[test]
    fn scroll_spans_no_left_ellipsis_at_start() {
        let spans = scroll_spans(vec![Span::raw("FooBarBazQux")], 10, 0);
        assert_eq!(spans_text(&spans), "FooBarBaz…");
    }

    #[test]
    fn scroll_spans_preserves_span_styles() {
        let style = Style::default().fg(ratatui::style::Color::Red);
        let spans = scroll_spans(vec![Span::styled("FooBarBazQux".to_string(), style)], 10, 0);
        assert!(spans.iter().any(|s| s.style == style));
    }

    // The following tests exercise the core scroll window logic directly, since
    // scroll_window_with_config drives all scroll behaviour.

    #[test]
    fn window_pauses_at_start() {
        let config = ScrollConfig {
            scroll_speed: 3,
            pause_duration: 30,
        };
        let text_len = 23; // "very_long_workflow_name"
        for tick in 0..30 {
            let (offset, has_left, has_right, _) =
                scroll_window_with_config(text_len, 10, tick, config);
            assert_eq!(offset, 0, "tick {tick}");
            assert!(!has_left, "tick {tick}");
            assert!(has_right, "tick {tick}");
        }
    }

    #[test]
    fn window_scrolls_left() {
        let config = ScrollConfig {
            scroll_speed: 3,
            pause_duration: 30,
        };
        let text_len = 23;
        let width = 10;

        let (offset, ..) = scroll_window_with_config(text_len, width, 30, config);
        assert_eq!(offset, 0);

        let (offset, has_left, has_right, _) =
            scroll_window_with_config(text_len, width, 33, config);
        assert_eq!(offset, 1);
        assert!(has_left);
        assert!(has_right);

        let (offset, ..) = scroll_window_with_config(text_len, width, 36, config);
        assert_eq!(offset, 2);
    }

    #[test]
    fn window_pauses_at_end() {
        let config = ScrollConfig {
            scroll_speed: 3,
            pause_duration: 30,
        };
        let text_len = 23;
        let width = 10;
        let scroll_range = text_len - width + 1;
        let phase_end = 30 + scroll_range as u64 * 3;

        for tick in phase_end..(phase_end + 30) {
            let (offset, has_left, has_right, _) =
                scroll_window_with_config(text_len, width, tick, config);
            assert_eq!(offset, scroll_range, "tick {tick}");
            assert!(has_left, "tick {tick}");
            assert!(!has_right, "tick {tick}");
        }
    }

    #[test]
    fn window_scrolls_right() {
        let config = ScrollConfig {
            scroll_speed: 3,
            pause_duration: 30,
        };
        let text_len = 23;
        let width = 10;
        let scroll_range = text_len - width + 1;
        let phase_right = 30 + scroll_range as u64 * 3 + 30;

        let (offset, ..) = scroll_window_with_config(text_len, width, phase_right, config);
        assert_eq!(offset, scroll_range);

        let (offset, has_left, has_right, _) =
            scroll_window_with_config(text_len, width, phase_right + 3, config);
        assert_eq!(offset, scroll_range - 1);
        assert!(has_left);
        assert!(has_right);
    }

    #[test]
    fn window_cycles() {
        let config = ScrollConfig {
            scroll_speed: 3,
            pause_duration: 30,
        };
        let text_len = 23;
        let width = 10;
        let scroll_range = text_len - width + 1;
        let cycle_length = 30 + scroll_range as u64 * 3 + 30 + scroll_range as u64 * 3;

        let at_zero = scroll_window_with_config(text_len, width, 0, config);
        let at_cycle = scroll_window_with_config(text_len, width, cycle_length, config);
        assert_eq!(at_zero, at_cycle);
    }
}

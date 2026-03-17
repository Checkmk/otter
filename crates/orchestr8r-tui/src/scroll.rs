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

/// Scrolls text left and right if it's longer than the available width.
/// For text that fits, returns it as-is with appropriate padding.
/// For text that doesn't fit, animates a smooth scroll using the tick counter:
/// pauses at the start, scrolls left, pauses at the end, scrolls right, and repeats.
/// Shows "…" on sides that are truncated.
pub fn scroll_text(text: &str, width: usize, tick: u64) -> (String, String) {
    scroll_text_with_config(text, width, tick, ScrollConfig::default())
}

fn scroll_text_with_config(
    text: &str,
    width: usize,
    tick: u64,
    config: ScrollConfig,
) -> (String, String) {
    let text_len = text.chars().count();

    if text_len <= width {
        let padding = " ".repeat(width.saturating_sub(text_len));
        return (text.to_string(), padding);
    }

    let scroll_range = text_len - width + 1;
    let scroll_speed = config.scroll_speed;
    let pause_duration = config.pause_duration;

    let cycle_length = (pause_duration + (scroll_range as u64 * scroll_speed) + pause_duration + (scroll_range as u64 * scroll_speed)) as u64;
    let phase = tick % cycle_length;

    let offset = if phase < pause_duration {
        0
    } else if phase < pause_duration + (scroll_range as u64 * scroll_speed) {
        ((phase - pause_duration) / scroll_speed) as usize
    } else if phase < pause_duration + (scroll_range as u64 * scroll_speed) + pause_duration {
        scroll_range
    } else {
        let progress = (phase - pause_duration - (scroll_range as u64 * scroll_speed) - pause_duration) / scroll_speed;
        (scroll_range as u64 - progress) as usize
    };

    let mut displayed = String::new();
    let has_left_truncation = offset > 0;
    let has_right_truncation = offset < scroll_range;

    if has_left_truncation {
        displayed.push('…');
    }

    let content_width = match (has_left_truncation, has_right_truncation) {
        (true, true) => width.saturating_sub(2),
        (true, false) | (false, true) => width.saturating_sub(1),
        (false, false) => width,
    };

    let content: String = text.chars().skip(offset).take(content_width).collect();
    displayed.push_str(&content);

    if has_right_truncation {
        displayed.push('…');
    }

    let padding = " ".repeat(width.saturating_sub(displayed.chars().count()));
    (displayed, padding)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_text_fits_without_scrolling() {
        let text = "short";
        let (displayed, padding) = scroll_text(text, 10, 0);
        assert_eq!(displayed, "short");
        assert_eq!(padding.len(), 5);
    }

    #[test]
    fn scroll_text_shows_no_left_ellipsis_at_offset_zero() {
        let text = "FooBarBazQux";  // 12 chars
        let width = 10;
        let (displayed, _) = scroll_text(text, width, 0);
        assert_eq!(displayed, "FooBarBaz…");
        assert!(!displayed.starts_with('…'), "Should not have left ellipsis at offset 0");
    }

    #[test]
    fn scroll_text_pauses_at_start() {
        let text = "very_long_workflow_name";
        let width = 10;
        let config = ScrollConfig {
            scroll_speed: 3,
            pause_duration: 30,
        };
        for tick in 0..30 {
            let (displayed, _) = scroll_text_with_config(text, width, tick, config);
            assert_eq!(displayed, "very_long…");
        }
    }

    #[test]
    fn scroll_text_scrolls_left() {
        let text = "very_long_workflow_name";
        let width = 10;
        let config = ScrollConfig {
            scroll_speed: 3,
            pause_duration: 30,
        };

        let (displayed, _) = scroll_text_with_config(text, width, 30, config);
        assert_eq!(displayed, "very_long…");

        let (displayed, _) = scroll_text_with_config(text, width, 33, config);
        assert_eq!(displayed, "…ery_long…");

        let (displayed, _) = scroll_text_with_config(text, width, 36, config);
        assert_eq!(displayed, "…ry_long_…");
    }

    #[test]
    fn scroll_text_pauses_at_end() {
        let text = "very_long_workflow_name";
        let width = 10;
        let config = ScrollConfig {
            scroll_speed: 3,
            pause_duration: 30,
        };
        let scroll_range = text.chars().count() - width + 1;
        let phase_start = 30 + (scroll_range as u64 * 3);

        for tick in phase_start..(phase_start + 30) {
            let (displayed, _) = scroll_text_with_config(text, width, tick, config);
            assert_eq!(displayed, "…flow_name");
        }
    }

    #[test]
    fn scroll_text_scrolls_right() {
        let text = "very_long_workflow_name";
        let width = 10;
        let config = ScrollConfig {
            scroll_speed: 3,
            pause_duration: 30,
        };
        let scroll_range = text.chars().count() - width + 1;
        let phase_scroll_right = 30 + (scroll_range as u64 * 3) + 30;

        let (displayed, _) = scroll_text_with_config(text, width, phase_scroll_right, config);
        assert_eq!(displayed, "…flow_name");

        let (displayed, _) = scroll_text_with_config(text, width, phase_scroll_right + 3, config);
        assert_eq!(displayed, "…kflow_na…");
    }

    #[test]
    fn scroll_text_cycles() {
        let text = "very_long_workflow_name";
        let width = 10;
        let config = ScrollConfig {
            scroll_speed: 3,
            pause_duration: 30,
        };

        let (displayed_start, _) = scroll_text_with_config(text, width, 0, config);
        let scroll_range = text.chars().count() - width + 1;
        let cycle_length = (30 + (scroll_range as u64 * 3) + 30 + (scroll_range as u64 * 3)) as u64;

        let (displayed_end, _) = scroll_text_with_config(text, width, cycle_length, config);
        assert_eq!(displayed_start, displayed_end);
    }
}

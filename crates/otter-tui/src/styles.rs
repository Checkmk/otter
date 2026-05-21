use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders};

use crate::theme;

pub fn step_color(step_type: &str) -> Color {
    match step_type {
        "agent" => theme::current().step_agent,
        "shell" => theme::current().step_shell,
        "checkpoint" => theme::current().step_checkpoint,
        "notify" => theme::current().step_notify,
        "feedback" => theme::current().action_feedback,
        _ => theme::current().step_other,
    }
}

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_SPEED: u64 = 6;

pub fn spinner_frame(tick: u64) -> &'static str {
    spinner_frame_with_speed(tick, SPINNER_SPEED)
}

fn spinner_frame_with_speed(tick: u64, speed: u64) -> &'static str {
    SPINNER[(tick / speed) as usize % SPINNER.len()]
}

pub fn base_style() -> Style {
    Style::default()
        .fg(theme::current().foreground)
        .bg(theme::current().background)
}

pub fn panel(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(theme::current().border)
                .bg(theme::current().background),
        )
        .title_style(
            Style::default()
                .fg(theme::current().foreground)
                .bg(theme::current().background)
                .add_modifier(Modifier::BOLD),
        )
        .title(title.to_string())
        .style(base_style())
}

pub fn panel_focused(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(theme::current().foreground)
                .bg(theme::current().background)
                .add_modifier(Modifier::BOLD),
        )
        .title_style(
            Style::default()
                .fg(theme::current().foreground)
                .bg(theme::current().background)
                .add_modifier(Modifier::BOLD),
        )
        .title(title.to_string())
        .style(base_style())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_color_returns_correct_colors() {
        assert_eq!(step_color("agent"), theme::current().step_agent);
        assert_eq!(step_color("shell"), theme::current().step_shell);
        assert_eq!(step_color("checkpoint"), theme::current().step_checkpoint);
        assert_eq!(step_color("notify"), theme::current().step_notify);
        assert_eq!(step_color("unknown"), theme::current().step_other);
    }

    #[test]
    fn spinner_frame_cycles_through_frames() {
        let frame0 = spinner_frame_with_speed(0, 1);
        let frame1 = spinner_frame_with_speed(1, 1);
        let frame10 = spinner_frame_with_speed(10, 1);

        assert_eq!(frame0, "⠋");
        assert_eq!(frame1, "⠙");
        assert_eq!(frame10, "⠋");
    }

    #[test]
    fn spinner_frame_advances_at_configured_speed() {
        let speed = 4;

        for i in 0..4 {
            assert_eq!(spinner_frame_with_speed(i, speed), "⠋");
        }
        for i in 4..8 {
            assert_eq!(spinner_frame_with_speed(i, speed), "⠙");
        }
    }

    #[test]
    fn base_style_uses_foreground_and_background_colors() {
        let style = base_style();
        assert_eq!(style.fg, Some(theme::current().foreground));
        assert_eq!(style.bg, Some(theme::current().background));
    }

    #[test]
    fn semantic_colors_are_distinct() {
        let t = theme::current();
        assert_ne!(t.action_continue, t.action_stop);
        assert_ne!(t.waiting_cp, t.running);
        assert_ne!(t.background, t.foreground);
    }
}

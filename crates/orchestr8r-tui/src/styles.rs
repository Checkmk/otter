use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders};

// ─── Palette ─────────────────────────────────────────────────────────────────
const BG: Color = Color::Rgb(0x33, 0x35, 0x43);
const PEACH: Color = Color::Rgb(0xfb, 0xc9, 0x97);
const TAN: Color = Color::Rgb(0xb7, 0x87, 0x60);
const GOLD: Color = Color::Rgb(0xc8, 0x99, 0x31);
const ROSE: Color = Color::Rgb(0xec, 0x84, 0x99);
const PURPLE: Color = Color::Rgb(0xa4, 0x59, 0xb7);
const GREEN: Color = Color::Rgb(0x61, 0x8b, 0x50);
const BLUE: Color = Color::Rgb(0x5b, 0x64, 0xc5);
const RED: Color = Color::Rgb(0xff, 0x56, 0x38);

// ─── Semantic color mapping ───────────────────────────────────────────────────
pub fn c_background() -> Color {
    BG
}

pub fn c_foreground() -> Color {
    PEACH
}

pub fn c_dim() -> Color {
    TAN
}

pub fn c_border() -> Color {
    BLUE
}

pub fn c_running() -> Color {
    GREEN
}

pub fn c_completed() -> Color {
    GREEN
}

pub fn c_failed() -> Color {
    RED
}

pub fn c_paused() -> Color {
    GOLD
}

pub fn c_dormant() -> Color {
    TAN
}

pub fn c_waiting_cp() -> Color {
    GOLD
}

pub fn c_action_continue() -> Color {
    GREEN
}

pub fn c_action_stop() -> Color {
    RED
}

pub fn c_action_feedback() -> Color {
    ROSE
}

pub fn c_notice_waiting() -> Color {
    GOLD
}

pub fn c_step_agent() -> Color {
    PURPLE
}

pub fn c_step_shell() -> Color {
    BLUE
}

pub fn c_step_checkpoint() -> Color {
    GOLD
}

pub fn c_step_notify() -> Color {
    ROSE
}

pub fn c_step_other() -> Color {
    TAN
}

pub fn step_color(step_type: &str) -> Color {
    match step_type {
        "agent" => c_step_agent(),
        "shell" => c_step_shell(),
        "checkpoint" => c_step_checkpoint(),
        "notify" => c_step_notify(),
        _ => c_step_other(),
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
        .fg(c_foreground())
        .bg(c_background())
}

pub fn panel(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(c_border())
                .bg(c_background()),
        )
        .title_style(
            Style::default()
                .fg(c_foreground())
                .bg(c_background())
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
        assert_eq!(step_color("agent"), c_step_agent());
        assert_eq!(step_color("shell"), c_step_shell());
        assert_eq!(step_color("checkpoint"), c_step_checkpoint());
        assert_eq!(step_color("notify"), c_step_notify());
        assert_eq!(step_color("unknown"), c_step_other());
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
        assert_eq!(style.fg, Some(c_foreground()));
        assert_eq!(style.bg, Some(c_background()));
    }

    #[test]
    fn semantic_colors_are_distinct() {
        assert_ne!(c_success(), c_error());
        assert_ne!(c_warning(), c_running());
        assert_ne!(c_background(), c_foreground());
    }

    fn c_success() -> Color {
        c_action_continue()
    }

    fn c_error() -> Color {
        c_action_stop()
    }

    fn c_warning() -> Color {
        c_waiting_cp()
    }
}

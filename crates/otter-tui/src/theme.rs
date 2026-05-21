use std::sync::OnceLock;

use ratatui::style::Color;

use crate::theme_loader::parse_partial;

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub background: Color,
    pub foreground: Color,
    pub dim: Color,
    pub border: Color,
    pub running: Color,
    pub completed: Color,
    pub failed: Color,
    pub dormant: Color,
    pub waiting_cp: Color,
    pub action_continue: Color,
    pub action_stop: Color,
    pub action_feedback: Color,
    pub notice_waiting: Color,
    pub step_agent: Color,
    pub step_shell: Color,
    pub step_checkpoint: Color,
    pub step_notify: Color,
    pub step_other: Color,
}

pub const BUILTIN_DARK_TOML: &str = include_str!("../assets/themes/dark.toml");
pub const BUILTIN_LIGHT_TOML: &str = include_str!("../assets/themes/light.toml");

/// The bundled dark theme, parsed lazily from `assets/themes/dark.toml`. This
/// is also the fallback for custom themes that omit fields.
pub fn builtin_dark() -> &'static Theme {
    static B: OnceLock<Theme> = OnceLock::new();
    B.get_or_init(|| {
        parse_partial(BUILTIN_DARK_TOML)
            .and_then(|p| p.into_full())
            .expect("bundled dark theme must be complete and valid")
    })
}

/// The bundled light theme, parsed lazily from `assets/themes/light.toml`.
pub fn builtin_light() -> &'static Theme {
    static B: OnceLock<Theme> = OnceLock::new();
    B.get_or_init(|| {
        parse_partial(BUILTIN_LIGHT_TOML)
            .and_then(|p| p.into_full())
            .expect("bundled light theme must be complete and valid")
    })
}

static ACTIVE: OnceLock<Theme> = OnceLock::new();

pub fn set(theme: Theme) {
    let _ = ACTIVE.set(theme);
}

pub fn current() -> &'static Theme {
    ACTIVE.get().unwrap_or_else(|| builtin_dark())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_dark_and_light_parse() {
        // GIVEN the bundled built-in themes
        // WHEN accessed
        // THEN they parse fully and have distinct backgrounds
        assert_ne!(builtin_dark().background, builtin_light().background);
    }

    #[test]
    fn builtin_light_matches_assets_file() {
        // GIVEN the assets/themes/light.toml palette
        // WHEN reading individual roles via the parsed builtin_light
        // THEN they match the expected hex codes
        let l = builtin_light();
        assert_eq!(l.background, Color::Rgb(0xfd, 0xf6, 0xe3));
        assert_eq!(l.foreground, Color::Rgb(0x65, 0x7b, 0x83));
        assert_eq!(l.step_agent, Color::Rgb(0x6c, 0x71, 0xc4));
    }

    #[test]
    fn builtin_dark_matches_assets_file() {
        // GIVEN the assets/themes/dark.toml palette
        // WHEN reading individual roles
        // THEN they match the expected hex codes
        let d = builtin_dark();
        assert_eq!(d.background, Color::Rgb(0x33, 0x35, 0x43));
        assert_eq!(d.foreground, Color::Rgb(0xfb, 0xc9, 0x97));
        assert_eq!(d.step_agent, Color::Rgb(0xa4, 0x59, 0xb7));
    }
}

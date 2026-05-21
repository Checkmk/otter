use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use ratatui::style::Color;
use serde::{de::Error as _, Deserialize, Deserializer};

use crate::theme::{builtin_dark, builtin_light, Theme, BUILTIN_DARK_TOML, BUILTIN_LIGHT_TOML};

pub const THEME_SCHEMA_VERSION: u32 = 1;

const BUILTIN_DARK_NAME: &str = "dark";
const BUILTIN_LIGHT_NAME: &str = "light";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    schema: u32,
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    #[serde(default)]
    colors: PartialTheme,
}

/// A theme parsed from disk. Any field may be absent — callers supply a
/// fallback `Theme` to fill the gaps (see [`PartialTheme::apply_to`]).
///
/// Hex strings are validated at deserialize time via the `HexColor` newtype.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialTheme {
    background: Option<HexColor>,
    foreground: Option<HexColor>,
    dim: Option<HexColor>,
    border: Option<HexColor>,
    running: Option<HexColor>,
    completed: Option<HexColor>,
    failed: Option<HexColor>,
    dormant: Option<HexColor>,
    waiting_cp: Option<HexColor>,
    action_continue: Option<HexColor>,
    action_stop: Option<HexColor>,
    action_feedback: Option<HexColor>,
    notice_waiting: Option<HexColor>,
    step_agent: Option<HexColor>,
    step_shell: Option<HexColor>,
    step_checkpoint: Option<HexColor>,
    step_notify: Option<HexColor>,
    step_other: Option<HexColor>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct HexColor(Color);

impl<'de> Deserialize<'de> for HexColor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_hex(&s).map(HexColor).map_err(D::Error::custom)
    }
}

impl PartialTheme {
    /// Fill missing fields from `base` to produce a complete `Theme`.
    pub fn apply_to(&self, base: &Theme) -> Theme {
        let or = |o: Option<HexColor>, fallback: Color| o.map_or(fallback, |h| h.0);
        Theme {
            background: or(self.background, base.background),
            foreground: or(self.foreground, base.foreground),
            dim: or(self.dim, base.dim),
            border: or(self.border, base.border),
            running: or(self.running, base.running),
            completed: or(self.completed, base.completed),
            failed: or(self.failed, base.failed),
            dormant: or(self.dormant, base.dormant),
            waiting_cp: or(self.waiting_cp, base.waiting_cp),
            action_continue: or(self.action_continue, base.action_continue),
            action_stop: or(self.action_stop, base.action_stop),
            action_feedback: or(self.action_feedback, base.action_feedback),
            notice_waiting: or(self.notice_waiting, base.notice_waiting),
            step_agent: or(self.step_agent, base.step_agent),
            step_shell: or(self.step_shell, base.step_shell),
            step_checkpoint: or(self.step_checkpoint, base.step_checkpoint),
            step_notify: or(self.step_notify, base.step_notify),
            step_other: or(self.step_other, base.step_other),
        }
    }

    /// Require every field to be present; error otherwise. Used to validate
    /// the bundled built-in themes at startup.
    pub fn into_full(self) -> Result<Theme> {
        fn req(c: Option<HexColor>, name: &str) -> Result<Color> {
            c.map(|h| h.0)
                .ok_or_else(|| anyhow!("missing required color field '{}'", name))
        }
        Ok(Theme {
            background: req(self.background, "background")?,
            foreground: req(self.foreground, "foreground")?,
            dim: req(self.dim, "dim")?,
            border: req(self.border, "border")?,
            running: req(self.running, "running")?,
            completed: req(self.completed, "completed")?,
            failed: req(self.failed, "failed")?,
            dormant: req(self.dormant, "dormant")?,
            waiting_cp: req(self.waiting_cp, "waiting_cp")?,
            action_continue: req(self.action_continue, "action_continue")?,
            action_stop: req(self.action_stop, "action_stop")?,
            action_feedback: req(self.action_feedback, "action_feedback")?,
            notice_waiting: req(self.notice_waiting, "notice_waiting")?,
            step_agent: req(self.step_agent, "step_agent")?,
            step_shell: req(self.step_shell, "step_shell")?,
            step_checkpoint: req(self.step_checkpoint, "step_checkpoint")?,
            step_notify: req(self.step_notify, "step_notify")?,
            step_other: req(self.step_other, "step_other")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ThemeMode {
    Auto,
    Dark,
    Light,
    Named(String),
}

impl ThemeMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "auto" => ThemeMode::Auto,
            "dark" => ThemeMode::Dark,
            "light" => ThemeMode::Light,
            other => ThemeMode::Named(other.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThemeConfig {
    pub mode: ThemeMode,
    pub themes_dir: PathBuf,
}

/// Parse theme TOML text into a `PartialTheme`. Unknown fields are rejected;
/// any 6-digit `#rrggbb` color may be omitted.
pub fn parse_partial(s: &str) -> Result<PartialTheme> {
    let file: ThemeFile = toml::from_str(s).context("Failed to parse theme TOML")?;

    if file.schema > THEME_SCHEMA_VERSION {
        bail!(
            "theme requires schema version {} but this otter supports up to {}",
            file.schema,
            THEME_SCHEMA_VERSION
        );
    }

    Ok(file.colors)
}

/// Parse a theme TOML file from disk into a `PartialTheme`.
pub fn load_partial(path: &Path) -> Result<PartialTheme> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read theme file {:?}", path))?;
    parse_partial(&content)
}

/// Write `dark.toml` and `light.toml` into `themes_dir` if they don't exist.
/// Idempotent — never overwrites a file the user has edited. Creates
/// `themes_dir` if necessary.
pub fn ensure_default_themes_written(themes_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(themes_dir)
        .with_context(|| format!("Failed to create themes directory {:?}", themes_dir))?;

    let dark = themes_dir.join("dark.toml");
    if !dark.exists() {
        std::fs::write(&dark, BUILTIN_DARK_TOML)
            .with_context(|| format!("Failed to write {:?}", dark))?;
    }

    let light = themes_dir.join("light.toml");
    if !light.exists() {
        std::fs::write(&light, BUILTIN_LIGHT_TOML)
            .with_context(|| format!("Failed to write {:?}", light))?;
    }

    Ok(())
}

/// Resolve the active theme from configuration. Seeds `dark.toml` and
/// `light.toml` into `themes_dir` on first run as schema references.
pub fn resolve(cfg: &ThemeConfig) -> Theme {
    if let Err(e) = ensure_default_themes_written(&cfg.themes_dir) {
        tracing::warn!(error = %e, "could not seed default themes; continuing");
    }

    let (builtin_name, base): (&str, &Theme) = match &cfg.mode {
        ThemeMode::Dark => (BUILTIN_DARK_NAME, builtin_dark()),
        ThemeMode::Light => (BUILTIN_LIGHT_NAME, builtin_light()),
        ThemeMode::Auto => match dark_light::detect() {
            Ok(dark_light::Mode::Light) => (BUILTIN_LIGHT_NAME, builtin_light()),
            Ok(dark_light::Mode::Dark) => (BUILTIN_DARK_NAME, builtin_dark()),
            Ok(dark_light::Mode::Unspecified) => (BUILTIN_LIGHT_NAME, builtin_light()),
            Err(e) => {
                tracing::warn!(error = %e, "OS color-scheme detection failed; using dark theme");
                (BUILTIN_DARK_NAME, builtin_dark())
            }
        },
        ThemeMode::Named(n) => {
            let path = cfg.themes_dir.join(format!("{}.toml", n));
            return match load_partial(&path) {
                Ok(p) => p.apply_to(builtin_dark()),
                Err(e) => {
                    tracing::warn!(
                        theme = %n,
                        path = ?path,
                        error = %e,
                        "failed to load custom theme; using bundled dark"
                    );
                    builtin_dark().clone()
                }
            };
        }
    };

    let path = cfg.themes_dir.join(format!("{}.toml", builtin_name));
    if let Ok(partial) = load_partial(&path) {
        if partial.apply_to(base) != *base {
            tracing::warn!(
                theme = %builtin_name,
                path = ?path,
                "edits to {0}.toml are ignored when mode = \"{0}\"; copy this file to ~/.config/otter/themes/<your-name>.toml and set mode = \"<your-name>\" in config.toml to apply them",
                builtin_name
            );
        }
    }
    base.clone()
}

fn parse_hex(s: &str) -> Result<Color> {
    let stripped = s
        .strip_prefix('#')
        .ok_or_else(|| anyhow!("color must start with '#': got '{}'", s))?;
    if stripped.len() != 6 {
        bail!("color must be 6 hex digits: got '{}'", s);
    }
    let r = u8::from_str_radix(&stripped[0..2], 16)
        .with_context(|| format!("color has invalid red component in '{}'", s))?;
    let g = u8::from_str_radix(&stripped[2..4], 16)
        .with_context(|| format!("color has invalid green component in '{}'", s))?;
    let b = u8::from_str_radix(&stripped[4..6], 16)
        .with_context(|| format!("color has invalid blue component in '{}'", s))?;
    Ok(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    const FULL_THEME: &str = r##"
schema = 1
name = "test"

[colors]
background      = "#111111"
foreground      = "#eeeeee"
dim             = "#888888"
border          = "#5b64c5"
running         = "#618b50"
completed       = "#618b50"
failed          = "#ff5638"
dormant         = "#b78760"
waiting_cp      = "#c89931"
action_continue = "#618b50"
action_stop     = "#ff5638"
action_feedback = "#ec8499"
notice_waiting  = "#c89931"
step_agent      = "#a459b7"
step_shell      = "#5b64c5"
step_checkpoint = "#c89931"
step_notify     = "#ec8499"
step_other      = "#b78760"
"##;

    fn write_tmp(contents: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f
    }

    #[test]
    fn parses_full_theme_into_all_some() {
        // GIVEN a complete theme file
        let p = parse_partial(FULL_THEME).unwrap();
        // WHEN converting to full
        let t = p.into_full().unwrap();
        // THEN every field is populated from the file
        assert_eq!(t.background, Color::Rgb(0x11, 0x11, 0x11));
        assert_eq!(t.foreground, Color::Rgb(0xee, 0xee, 0xee));
        assert_eq!(t.step_agent, Color::Rgb(0xa4, 0x59, 0xb7));
    }

    #[test]
    fn missing_fields_fall_back_to_base() {
        // GIVEN a partial theme that only overrides background
        let partial = parse_partial(
            r##"
schema = 1
[colors]
background = "#abcdef"
"##,
        )
        .unwrap();
        // WHEN applied over the bundled dark theme
        let theme = partial.apply_to(builtin_dark());
        // THEN background is overridden but other fields come from dark
        assert_eq!(theme.background, Color::Rgb(0xab, 0xcd, 0xef));
        assert_eq!(theme.foreground, builtin_dark().foreground);
        assert_eq!(theme.step_agent, builtin_dark().step_agent);
    }

    #[test]
    fn rejects_unknown_field() {
        // GIVEN a theme file with an unknown field
        let bad = FULL_THEME.replace("step_other", "step_unknown_role");
        // WHEN parsing
        let err = parse_partial(&bad).unwrap_err();
        // THEN it surfaces a parse error
        assert!(err.to_string().contains("parse"));
    }

    #[test]
    fn rejects_bad_hex() {
        // GIVEN a theme file with a malformed hex value
        let bad = FULL_THEME.replace("#111111", "#zzz");
        // WHEN parsing
        let err = parse_partial(&bad).unwrap_err();
        // THEN the error chain mentions the offending value or its field
        let msg = format!("{err:#}");
        assert!(
            msg.contains("background") || msg.contains("hex") || msg.contains("digits"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn rejects_unsupported_schema() {
        // GIVEN a theme file declaring a future schema version
        let bad = FULL_THEME.replace("schema = 1", "schema = 999");
        // WHEN parsing
        let err = parse_partial(&bad).unwrap_err();
        // THEN the error mentions schema
        assert!(err.to_string().contains("schema"));
    }

    #[test]
    fn into_full_errors_when_field_missing() {
        // GIVEN a partial theme missing 'background'
        let p = parse_partial(
            r##"
schema = 1
[colors]
foreground = "#eeeeee"
"##,
        )
        .unwrap();
        // WHEN converting to full
        let err = p.into_full().unwrap_err();
        // THEN it names the missing field
        assert!(err.to_string().contains("background"));
    }

    #[test]
    fn load_partial_reads_file() {
        // GIVEN a theme file on disk
        let f = write_tmp(FULL_THEME);
        // WHEN load_partial reads it
        let theme = load_partial(f.path()).unwrap().into_full().unwrap();
        // THEN colors match the file
        assert_eq!(theme.background, Color::Rgb(0x11, 0x11, 0x11));
    }

    #[test]
    fn theme_mode_parse_maps_strings() {
        // GIVEN the supported mode strings
        // WHEN parsing
        // THEN they map to the expected variants
        assert_eq!(ThemeMode::parse("auto"), ThemeMode::Auto);
        assert_eq!(ThemeMode::parse("dark"), ThemeMode::Dark);
        assert_eq!(ThemeMode::parse("light"), ThemeMode::Light);
        assert_eq!(
            ThemeMode::parse("solarized"),
            ThemeMode::Named("solarized".to_string())
        );
    }

    #[test]
    fn ensure_default_themes_creates_missing_files() {
        // GIVEN an empty themes_dir
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        // WHEN seeding
        ensure_default_themes_written(&path).unwrap();
        // THEN both built-in themes are written
        assert!(path.join("dark.toml").exists());
        assert!(path.join("light.toml").exists());
    }

    #[test]
    fn ensure_default_themes_does_not_overwrite() {
        // GIVEN a themes_dir with an existing dark.toml that the user has edited
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        std::fs::write(path.join("dark.toml"), "# user-edited content").unwrap();
        // WHEN seeding
        ensure_default_themes_written(&path).unwrap();
        // THEN the existing file is preserved
        let content = std::fs::read_to_string(path.join("dark.toml")).unwrap();
        assert_eq!(content, "# user-edited content");
    }

    #[test]
    fn resolve_dark_uses_disk_dark_file_after_seeding() {
        // GIVEN a fresh themes_dir
        let dir = tempfile::tempdir().unwrap();
        let cfg = ThemeConfig {
            mode: ThemeMode::Dark,
            themes_dir: dir.path().to_path_buf(),
        };
        // WHEN resolving
        let theme = resolve(&cfg);
        // THEN the resolved theme matches the bundled dark (seed was written and loaded)
        assert_eq!(theme, *builtin_dark());
    }

    #[test]
    fn resolve_light_uses_bundled_light() {
        // GIVEN a fresh themes_dir
        let dir = tempfile::tempdir().unwrap();
        let cfg = ThemeConfig {
            mode: ThemeMode::Light,
            themes_dir: dir.path().to_path_buf(),
        };
        // WHEN resolving
        let theme = resolve(&cfg);
        // THEN the resolved theme matches the bundled light
        assert_eq!(theme, *builtin_light());
    }

    #[test]
    fn resolve_named_falls_back_to_bundled_dark_when_missing() {
        // GIVEN a fresh themes_dir with no custom theme file
        let dir = tempfile::tempdir().unwrap();
        let cfg = ThemeConfig {
            mode: ThemeMode::Named("does-not-exist".to_string()),
            themes_dir: dir.path().to_path_buf(),
        };
        // WHEN resolving
        let theme = resolve(&cfg);
        // THEN we fall back to the bundled dark theme (a warning is logged)
        assert_eq!(theme, *builtin_dark());
    }

    #[test]
    fn resolve_named_loads_custom_file_with_dark_fallback() {
        // GIVEN a custom theme that only overrides background
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("custom.toml"),
            r##"
schema = 1
[colors]
background = "#abcdef"
"##,
        )
        .unwrap();
        let cfg = ThemeConfig {
            mode: ThemeMode::Named("custom".to_string()),
            themes_dir: dir.path().to_path_buf(),
        };
        // WHEN resolving
        let theme = resolve(&cfg);
        // THEN background is overridden, other fields fall back to the bundled dark
        assert_eq!(theme.background, Color::Rgb(0xab, 0xcd, 0xef));
        assert_eq!(theme.foreground, builtin_dark().foreground);
    }

    #[test]
    fn resolve_dark_ignores_user_edits_to_dark_toml() {
        // GIVEN a user-edited dark.toml that overrides foreground
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("dark.toml"),
            r##"
schema = 1
[colors]
foreground = "#fedcba"
"##,
        )
        .unwrap();
        let cfg = ThemeConfig {
            mode: ThemeMode::Dark,
            themes_dir: dir.path().to_path_buf(),
        };
        // WHEN resolving with the built-in dark mode
        let theme = resolve(&cfg);
        // THEN the bundled dark is returned; edits to dark.toml require renaming
        // the file and switching mode to "<that-name>" to take effect.
        assert_eq!(theme, *builtin_dark());
    }
}

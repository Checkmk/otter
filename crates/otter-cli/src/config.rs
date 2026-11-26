use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

use otter_tui::theme_loader::{ThemeConfig, ThemeMode};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct OtterConfigFile {
    #[serde(default)]
    theme: ThemeSection,
    #[serde(default)]
    retention: RetentionSection,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeSection {
    #[serde(default = "default_theme_mode")]
    mode: String,
}

impl Default for ThemeSection {
    fn default() -> Self {
        Self {
            mode: default_theme_mode(),
        }
    }
}

fn default_theme_mode() -> String {
    "dark".to_string()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetentionSection {
    #[serde(default = "default_retention_days")]
    days: u32,
}

impl Default for RetentionSection {
    fn default() -> Self {
        Self {
            days: default_retention_days(),
        }
    }
}

fn default_retention_days() -> u32 {
    30
}

pub fn config_file_path(config_dir: &Path) -> PathBuf {
    config_dir.join("config.toml")
}

pub fn themes_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("themes")
}

fn load_config_file(config_dir: &Path) -> anyhow::Result<OtterConfigFile> {
    let path = config_file_path(config_dir);
    if !path.exists() {
        return Ok(OtterConfigFile::default());
    }
    let s = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    toml::from_str(&s).with_context(|| format!("Failed to parse {}", path.display()))
}

pub fn load_theme_config(config_dir: &Path) -> ThemeConfig {
    let parsed = load_config_file(config_dir).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "ignoring invalid config.toml; using defaults");
        OtterConfigFile::default()
    });

    ThemeConfig {
        mode: ThemeMode::parse(&parsed.theme.mode),
        themes_dir: themes_dir(config_dir),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    KeepForever,
    Days(u32),
}

pub fn load_retention(config_dir: &Path) -> anyhow::Result<Retention> {
    Ok(match load_config_file(config_dir)?.retention.days {
        0 => Retention::KeepForever,
        days => Retention::Days(days),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_config_file_yields_dark_mode() {
        // GIVEN a config dir with no config.toml
        let dir = TempDir::new().unwrap();
        // WHEN loading theme config
        let cfg = load_theme_config(dir.path());
        // THEN mode is Dark (the safe default; auto must be opted into) and
        // themes_dir points beneath the config dir
        assert_eq!(cfg.mode, ThemeMode::Dark);
        assert_eq!(cfg.themes_dir, dir.path().join("themes"));
    }

    #[test]
    fn loads_explicit_light_mode() {
        // GIVEN a config.toml selecting the light theme
        let dir = TempDir::new().unwrap();
        std::fs::write(config_file_path(dir.path()), "[theme]\nmode = \"light\"\n").unwrap();
        // WHEN loading
        let cfg = load_theme_config(dir.path());
        // THEN the parsed mode is Light
        assert_eq!(cfg.mode, ThemeMode::Light);
    }

    #[test]
    fn loads_named_custom_mode() {
        // GIVEN a config.toml referencing a custom theme name
        let dir = TempDir::new().unwrap();
        std::fs::write(
            config_file_path(dir.path()),
            "[theme]\nmode = \"solarized\"\n",
        )
        .unwrap();
        // WHEN loading
        let cfg = load_theme_config(dir.path());
        // THEN the mode is Named with that string
        assert_eq!(cfg.mode, ThemeMode::Named("solarized".to_string()));
    }

    #[test]
    fn malformed_config_falls_back_to_defaults() {
        // GIVEN a config.toml that fails to parse
        let dir = TempDir::new().unwrap();
        std::fs::write(config_file_path(dir.path()), "this is not toml = = =").unwrap();
        // WHEN loading
        let cfg = load_theme_config(dir.path());
        // THEN we get the defaults rather than panicking
        assert_eq!(cfg.mode, ThemeMode::Dark);
    }

    #[test]
    fn retention_defaults_to_30_days() {
        // GIVEN a config dir with no config.toml
        let dir = TempDir::new().unwrap();
        // WHEN / THEN
        assert_eq!(load_retention(dir.path()).unwrap(), Retention::Days(30));
    }

    #[test]
    fn retention_of_zero_days_keeps_everything() {
        // GIVEN
        let dir = TempDir::new().unwrap();
        std::fs::write(config_file_path(dir.path()), "[retention]\ndays = 0\n").unwrap();
        // WHEN / THEN
        assert_eq!(load_retention(dir.path()).unwrap(), Retention::KeepForever);
    }

    #[test]
    fn retention_reads_configured_days() {
        // GIVEN
        let dir = TempDir::new().unwrap();
        std::fs::write(config_file_path(dir.path()), "[retention]\ndays = 90\n").unwrap();
        // WHEN / THEN
        assert_eq!(load_retention(dir.path()).unwrap(), Retention::Days(90));
    }

    #[test]
    fn malformed_config_is_a_retention_error() {
        // GIVEN a config.toml that fails to parse
        let dir = TempDir::new().unwrap();
        std::fs::write(config_file_path(dir.path()), "[retention]\ndays = -1\n").unwrap();
        // WHEN / THEN nothing gets deleted on a guessed default
        assert!(load_retention(dir.path()).is_err());
    }
}

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

use otter_tui::theme_loader::{ThemeConfig, ThemeMode};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct OtterConfigFile {
    #[serde(default)]
    theme: ThemeSection,
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

pub fn config_file_path(config_dir: &Path) -> PathBuf {
    config_dir.join("config.toml")
}

pub fn themes_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("themes")
}

/// Load `~/.config/otter/config.toml` (if present) and derive the theme
/// configuration. A missing file is not an error — defaults are returned.
/// A malformed file is reported via tracing and defaults are used.
pub fn load_theme_config(config_dir: &Path) -> ThemeConfig {
    let path = config_file_path(config_dir);
    let parsed: OtterConfigFile = if path.exists() {
        match std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))
            .and_then(|s| {
                toml::from_str::<OtterConfigFile>(&s)
                    .with_context(|| format!("Failed to parse {}", path.display()))
            }) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(error = %e, "ignoring invalid config.toml; using defaults");
                OtterConfigFile::default()
            }
        }
    } else {
        OtterConfigFile::default()
    };

    ThemeConfig {
        mode: ThemeMode::parse(&parsed.theme.mode),
        themes_dir: themes_dir(config_dir),
    }
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
}

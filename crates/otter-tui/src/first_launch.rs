use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const STATE_FILE: &str = "tui-state.toml";

/// Tracks which one-time modals the user has already seen.
///
/// Persisted to `<config_dir>/tui-state.toml`. Read once at TUI start;
/// writes happen lazily when a modal is marked seen.
pub struct FirstLaunchState {
    seen: HashSet<String>,
    config_dir: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
struct OnDisk {
    #[serde(default)]
    seen_modals: Vec<String>,
}

impl FirstLaunchState {
    pub fn load(config_dir: &Path) -> Self {
        let path = config_dir.join(STATE_FILE);
        let seen: HashSet<String> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str::<OnDisk>(&s).ok())
            .map(|d| d.seen_modals.into_iter().collect())
            .unwrap_or_default();
        Self {
            seen,
            config_dir: config_dir.to_path_buf(),
        }
    }

    pub fn has_seen(&self, id: &str) -> bool {
        self.seen.contains(id)
    }

    /// Mark `id` as seen and persist. New IDs trigger a write; repeats are no-ops.
    pub fn mark_seen(&mut self, id: &str) {
        if !self.seen.insert(id.to_string()) {
            return;
        }
        if let Err(e) = self.persist() {
            tracing::warn!(
                "failed to persist first-launch state to {}: {e}",
                self.config_dir.join(STATE_FILE).display()
            );
        }
    }

    fn persist(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        let mut seen_modals: Vec<String> = self.seen.iter().cloned().collect();
        seen_modals.sort();
        let s = toml::to_string_pretty(&OnDisk { seen_modals })
            .expect("serializing a Vec<String> never fails");
        std::fs::write(self.config_dir.join(STATE_FILE), s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_empty_when_no_state_file_exists() {
        // GIVEN a config dir without a state file
        let dir = tempfile::tempdir().unwrap();

        // WHEN loading
        let state = FirstLaunchState::load(dir.path());

        // THEN nothing is marked seen
        assert!(!state.has_seen("help"));
    }

    #[test]
    fn mark_seen_persists_across_loads() {
        // GIVEN a fresh state with one id marked
        let dir = tempfile::tempdir().unwrap();
        let mut state = FirstLaunchState::load(dir.path());
        state.mark_seen("help");

        // WHEN reloading from disk
        let reloaded = FirstLaunchState::load(dir.path());

        // THEN the id is still marked seen
        assert!(reloaded.has_seen("help"));
    }

    #[test]
    fn mark_seen_creates_config_dir_if_missing() {
        // GIVEN a config dir path that does not yet exist on disk
        let parent = tempfile::tempdir().unwrap();
        let config_dir = parent.path().join("otter");
        assert!(!config_dir.exists());

        // WHEN marking an id
        let mut state = FirstLaunchState::load(&config_dir);
        state.mark_seen("help");

        // THEN the dir is created and state is persisted
        let reloaded = FirstLaunchState::load(&config_dir);
        assert!(reloaded.has_seen("help"));
    }

    #[test]
    fn mark_seen_is_idempotent() {
        // GIVEN an id already marked seen
        let dir = tempfile::tempdir().unwrap();
        let mut state = FirstLaunchState::load(dir.path());
        state.mark_seen("help");

        // WHEN marking the same id again
        state.mark_seen("help");

        // THEN only one entry exists on disk
        let raw = std::fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
        assert_eq!(raw.matches("help").count(), 1);
    }
}

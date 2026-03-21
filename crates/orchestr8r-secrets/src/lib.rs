use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret '{0}' not found")]
    NotFound(String),
    #[error("secret store error: {0}")]
    Other(#[from] anyhow::Error),
}

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
    fn list(&self) -> Vec<String>;
    fn set(&self, key: &str, value: &str) -> anyhow::Result<()>;
    fn delete(&self, key: &str) -> anyhow::Result<()>;

    /// Resolve a list of secret names to `(name, value)` pairs.
    /// Returns `SecretError::NotFound` if any name is absent.
    fn resolve(&self, names: &[String]) -> Result<Vec<(String, String)>, SecretError> {
        names
            .iter()
            .map(|name| {
                self.get(name)
                    .map(|val| (name.clone(), val))
                    .ok_or_else(|| SecretError::NotFound(name.clone()))
            })
            .collect()
    }
}

// ── NoOp ──────────────────────────────────────────────────────────────────────

pub struct NoOpSecretStore;

impl SecretStore for NoOpSecretStore {
    fn get(&self, _key: &str) -> Option<String> {
        None
    }
    fn list(&self) -> Vec<String> {
        vec![]
    }
    fn set(&self, _key: &str, _value: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn delete(&self, _key: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

// ── File-backed ───────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, serde::Serialize, Default)]
struct SecretsFile {
    #[serde(default)]
    secrets: HashMap<String, String>,
}

pub struct FileSecretStore {
    path: PathBuf,
}

impl FileSecretStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn load(&self) -> anyhow::Result<SecretsFile> {
        if !self.path.exists() {
            return Ok(SecretsFile::default());
        }
        let content = std::fs::read_to_string(&self.path)?;
        Ok(toml::from_str(&content)?)
    }

    fn save(&self, file: &SecretsFile) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string(file)?;
        std::fs::write(&self.path, content)?;
        Ok(())
    }
}

impl SecretStore for FileSecretStore {
    fn get(&self, key: &str) -> Option<String> {
        self.load().ok()?.secrets.remove(key)
    }

    fn list(&self) -> Vec<String> {
        let Ok(file) = self.load() else {
            return vec![];
        };
        let mut keys: Vec<String> = file.secrets.into_keys().collect();
        keys.sort();
        keys
    }

    fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let mut file = self.load()?;
        file.secrets.insert(key.to_string(), value.to_string());
        self.save(&file)
    }

    fn delete(&self, key: &str) -> anyhow::Result<()> {
        let mut file = self.load()?;
        file.secrets.remove(key);
        self.save(&file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in(dir: &std::path::Path) -> FileSecretStore {
        FileSecretStore::new(dir.join("secrets.toml"))
    }

    #[test]
    fn get_missing_returns_none() {
        // GIVEN an empty store
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());

        // WHEN / THEN
        assert!(store.get("MISSING").is_none());
    }

    #[test]
    fn set_and_get_roundtrip() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());

        // WHEN
        store.set("API_KEY", "secret123").unwrap();

        // THEN
        assert_eq!(store.get("API_KEY").as_deref(), Some("secret123"));
    }

    #[test]
    fn list_returns_sorted_keys() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.set("Z_KEY", "z").unwrap();
        store.set("A_KEY", "a").unwrap();
        store.set("M_KEY", "m").unwrap();

        // WHEN
        let keys = store.list();

        // THEN
        assert_eq!(keys, vec!["A_KEY", "M_KEY", "Z_KEY"]);
    }

    #[test]
    fn delete_removes_key() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.set("TO_DELETE", "value").unwrap();
        assert!(store.get("TO_DELETE").is_some());

        // WHEN
        store.delete("TO_DELETE").unwrap();

        // THEN
        assert!(store.get("TO_DELETE").is_none());
    }

    #[test]
    fn delete_nonexistent_is_ok() {
        // GIVEN an empty store
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());

        // WHEN / THEN — no error
        store.delete("GHOST").unwrap();
    }

    #[test]
    fn resolve_all_present_returns_pairs() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.set("KEY_A", "val_a").unwrap();
        store.set("KEY_B", "val_b").unwrap();

        // WHEN
        let pairs = store.resolve(&["KEY_A".to_string(), "KEY_B".to_string()]).unwrap();

        // THEN
        assert!(pairs.contains(&("KEY_A".to_string(), "val_a".to_string())));
        assert!(pairs.contains(&("KEY_B".to_string(), "val_b".to_string())));
    }

    #[test]
    fn resolve_missing_key_returns_error() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());

        // WHEN
        let err = store.resolve(&["MISSING".to_string()]).unwrap_err();

        // THEN
        assert!(err.to_string().contains("MISSING"));
        assert!(matches!(err, SecretError::NotFound(_)));
    }

    #[test]
    fn persists_across_store_instances() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let store1 = store_in(dir.path());
        store1.set("PERSISTENT", "yes").unwrap();

        // WHEN a new instance reads the same file
        let store2 = store_in(dir.path());

        // THEN
        assert_eq!(store2.get("PERSISTENT").as_deref(), Some("yes"));
    }
}

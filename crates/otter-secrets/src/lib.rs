use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use age::secrecy::ExposeSecret as _;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret '{0}' not found")]
    NotFound(String),
    #[error("secrets store locked: {0}")]
    Locked(String),
    #[error("secret store error: {0}")]
    Other(#[from] anyhow::Error),
}

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
    fn list(&self) -> Vec<String>;
    fn set(&self, key: &str, value: &str) -> anyhow::Result<()>;
    fn delete(&self, key: &str) -> anyhow::Result<()>;

    /// Resolve a list of secret names to `(name, value)` pairs.
    /// Returns `SecretError::NotFound` if any name is absent,
    /// or `SecretError::Locked` if the store cannot be decrypted.
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

pub trait KeyProvider: Send + Sync {
    fn encrypt(&self, plaintext: &[u8]) -> anyhow::Result<Vec<u8>>;
    fn decrypt(&self, ciphertext: &[u8]) -> anyhow::Result<Vec<u8>>;
}

// ── KeyringKeyProvider ────────────────────────────────────────────────────────

/// Stores an age x25519 identity in the OS keyring (libsecret / macOS Keychain
/// / Windows Credential Manager). Generates a new identity on first use.
///
/// The identity is cached after first retrieval so that all encrypt/decrypt
/// operations within the same process lifetime use the same key, even if the
/// keyring backend is unreliable across calls.
pub struct KeyringKeyProvider {
    cached: std::sync::Mutex<Option<age::x25519::Identity>>,
}

impl KeyringKeyProvider {
    pub fn new() -> Self {
        Self {
            cached: std::sync::Mutex::new(None),
        }
    }

    /// Returns `Ok(())` if the keyring is reachable and a key can be obtained
    /// or created. Use this to check availability before prompting a fallback.
    pub fn probe(&self) -> anyhow::Result<()> {
        self.get_or_create_identity().map(|_| ())
    }

    fn get_or_create_identity(&self) -> anyhow::Result<age::x25519::Identity> {
        let mut guard = self.cached.lock().expect("cached identity lock poisoned");
        if let Some(ref id) = *guard {
            return Ok(id.clone());
        }
        let entry = keyring::Entry::new("otter", "secrets-key")?;
        let identity = match entry.get_password() {
            Ok(s) => s
                .parse::<age::x25519::Identity>()
                .map_err(|e| anyhow::anyhow!("invalid age identity in keyring: {e}"))?,
            Err(keyring::Error::NoEntry) => {
                let identity = age::x25519::Identity::generate();
                let key_str = identity.to_string();
                entry.set_password(key_str.expose_secret())?;
                identity
            }
            Err(e) => return Err(anyhow::anyhow!("keyring error: {e}")),
        };
        *guard = Some(identity.clone());
        Ok(identity)
    }
}

impl Default for KeyringKeyProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyProvider for KeyringKeyProvider {
    fn encrypt(&self, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
        let identity = self.get_or_create_identity()?;
        let recipient = identity.to_public();
        encrypt_age(
            plaintext,
            std::iter::once(Box::new(recipient) as Box<dyn age::Recipient + Send>),
        )
    }

    fn decrypt(&self, ciphertext: &[u8]) -> anyhow::Result<Vec<u8>> {
        let identity = self.get_or_create_identity()?;
        decrypt_age_recipients(ciphertext, &identity)
    }
}

// ── age helpers ───────────────────────────────────────────────────────────────

fn encrypt_age(
    plaintext: &[u8],
    recipients: impl Iterator<Item = Box<dyn age::Recipient + Send>>,
) -> anyhow::Result<Vec<u8>> {
    let encryptor = age::Encryptor::with_recipients(recipients.collect::<Vec<_>>())
        .ok_or_else(|| anyhow::anyhow!("age encryptor: empty recipients list"))?;
    let mut output = Vec::new();
    let mut writer = encryptor.wrap_output(&mut output)?;
    writer.write_all(plaintext)?;
    writer.finish()?;
    Ok(output)
}

fn decrypt_age_recipients(
    ciphertext: &[u8],
    identity: &dyn age::Identity,
) -> anyhow::Result<Vec<u8>> {
    let decryptor = match age::Decryptor::new(ciphertext)
        .map_err(|e| anyhow::anyhow!("age decrypt init: {e}"))?
    {
        age::Decryptor::Recipients(d) => d,
        age::Decryptor::Passphrase(_) => {
            anyhow::bail!("file was not encrypted with a key")
        }
    };
    let mut reader = decryptor
        .decrypt(std::iter::once(identity))
        .map_err(|e| anyhow::anyhow!("decryption failed (wrong key?): {e}"))?;
    let mut plaintext = Vec::new();
    reader.read_to_end(&mut plaintext)?;
    Ok(plaintext)
}

// ── EncryptedSecretStore ──────────────────────────────────────────────────────

#[derive(serde::Deserialize, serde::Serialize, Default)]
struct SecretsFile {
    #[serde(default)]
    secrets: HashMap<String, String>,
}

pub struct EncryptedSecretStore {
    path: PathBuf,
    key_provider: Arc<dyn KeyProvider>,
    write_lock: std::sync::Mutex<()>,
}

impl EncryptedSecretStore {
    pub fn new(path: PathBuf, key_provider: Arc<dyn KeyProvider>) -> Self {
        Self {
            path,
            key_provider,
            write_lock: std::sync::Mutex::new(()),
        }
    }

    fn read_from_disk(&self) -> Result<HashMap<String, String>, SecretError> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }
        let ciphertext = std::fs::read(&self.path)
            .map_err(|e| SecretError::Locked(format!("read {}: {e}", self.path.display())))?;
        let plaintext = self
            .key_provider
            .decrypt(&ciphertext)
            .map_err(|e| SecretError::Locked(e.to_string()))?;
        let file: SecretsFile = toml::from_str(std::str::from_utf8(&plaintext).map_err(|e| {
            SecretError::Other(anyhow::anyhow!("invalid UTF-8 in secrets file: {e}"))
        })?)
        .map_err(|e| SecretError::Other(anyhow::anyhow!("secrets TOML parse error: {e}")))?;
        Ok(file.secrets)
    }

    /// Serialize the given map and re-encrypt to disk using an atomic rename.
    fn flush(&self, map: &HashMap<String, String>) -> anyhow::Result<()> {
        let file = SecretsFile {
            secrets: map.clone(),
        };
        let toml_bytes = toml::to_string(&file)?.into_bytes();
        let ciphertext = self.key_provider.encrypt(&toml_bytes)?;
        let parent = self.path.parent().unwrap_or(std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;

        // Atomic write: write to a temp file in the same directory, then rename.
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.write_all(&ciphertext)?;
        tmp.persist(&self.path).map_err(|e| e.error)?;
        Ok(())
    }
}

impl SecretStore for EncryptedSecretStore {
    fn get(&self, key: &str) -> Option<String> {
        match self.read_from_disk() {
            Ok(map) => map.get(key).cloned(),
            Err(e) => {
                tracing::error!("secrets store locked, returning None for '{}': {e}", key);
                None
            }
        }
    }

    fn list(&self) -> Vec<String> {
        match self.read_from_disk() {
            Ok(map) => {
                let mut keys: Vec<String> = map.keys().cloned().collect();
                keys.sort();
                keys
            }
            Err(e) => {
                tracing::error!("secrets store locked, returning empty list: {e}");
                vec![]
            }
        }
    }

    fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let mut map = self.read_from_disk().map_err(|e| anyhow::anyhow!("{e}"))?;
        map.insert(key.to_string(), value.to_string());
        self.flush(&map)
    }

    fn delete(&self, key: &str) -> anyhow::Result<()> {
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let mut map = self.read_from_disk().map_err(|e| anyhow::anyhow!("{e}"))?;
        map.remove(key);
        self.flush(&map)
    }

    fn resolve(&self, names: &[String]) -> Result<Vec<(String, String)>, SecretError> {
        let map = self.read_from_disk()?;
        names
            .iter()
            .map(|name| {
                map.get(name)
                    .map(|val| (name.clone(), val.clone()))
                    .ok_or_else(|| SecretError::NotFound(name.clone()))
            })
            .collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// No-op key provider for tests: encrypt/decrypt are identity operations.
    /// Avoids keyring or passphrase dependencies in unit tests.
    struct PlaintextKeyProvider;

    impl KeyProvider for PlaintextKeyProvider {
        fn encrypt(&self, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
            Ok(plaintext.to_vec())
        }
        fn decrypt(&self, ciphertext: &[u8]) -> anyhow::Result<Vec<u8>> {
            Ok(ciphertext.to_vec())
        }
    }

    fn store_in(dir: &std::path::Path) -> EncryptedSecretStore {
        EncryptedSecretStore::new(dir.join("secrets.age"), Arc::new(PlaintextKeyProvider))
    }

    // ── EncryptedSecretStore tests ────────────────────────────────────────────

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
        let pairs = store
            .resolve(&["KEY_A".to_string(), "KEY_B".to_string()])
            .unwrap();

        // THEN
        assert!(pairs.contains(&("KEY_A".to_string(), "val_a".to_string())));
        assert!(pairs.contains(&("KEY_B".to_string(), "val_b".to_string())));
    }

    #[test]
    fn resolve_missing_key_returns_not_found() {
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
    fn get_reflects_writes_from_another_store_instance() {
        // GIVEN a store that has already been read (so a stale cache would survive)
        let dir = tempfile::tempdir().unwrap();
        let store1 = store_in(dir.path());
        store1.set("KEY", "v1").unwrap();
        assert_eq!(store1.get("KEY").as_deref(), Some("v1"));

        // WHEN a second store instance (e.g. the `otter secret set` CLI) overwrites the value
        let store2 = store_in(dir.path());
        store2.set("KEY", "v2").unwrap();

        // THEN store1 sees the updated value rather than its previously cached copy
        assert_eq!(store1.get("KEY").as_deref(), Some("v2"));
    }

    #[test]
    fn resolve_reflects_writes_from_another_store_instance() {
        // GIVEN a store that has already resolved at least once
        let dir = tempfile::tempdir().unwrap();
        let store1 = store_in(dir.path());
        store1.set("KEY", "v1").unwrap();
        let _ = store1.resolve(&["KEY".to_string()]).unwrap();

        // WHEN a second instance updates the same key
        let store2 = store_in(dir.path());
        store2.set("KEY", "v2").unwrap();

        // THEN resolve() on store1 returns the fresh value
        let pairs = store1.resolve(&["KEY".to_string()]).unwrap();
        assert_eq!(pairs, vec![("KEY".to_string(), "v2".to_string())]);
    }

    #[test]
    fn resolve_picks_up_keys_added_after_first_read() {
        // GIVEN a store that has already been read once (priming any cache)
        let dir = tempfile::tempdir().unwrap();
        let store1 = store_in(dir.path());
        store1.set("EXISTING", "v").unwrap();
        let _ = store1.list();

        // WHEN a second instance adds a new key
        let store2 = store_in(dir.path());
        store2.set("NEW_KEY", "value").unwrap();

        // THEN resolve() on store1 finds the new key
        let pairs = store1.resolve(&["NEW_KEY".to_string()]).unwrap();
        assert_eq!(pairs, vec![("NEW_KEY".to_string(), "value".to_string())]);
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

    #[test]
    fn resolve_locked_returns_locked_error() {
        // GIVEN a store whose key provider always fails
        struct BadKeyProvider;
        impl KeyProvider for BadKeyProvider {
            fn encrypt(&self, _: &[u8]) -> anyhow::Result<Vec<u8>> {
                anyhow::bail!("no key")
            }
            fn decrypt(&self, _: &[u8]) -> anyhow::Result<Vec<u8>> {
                anyhow::bail!("no key")
            }
        }

        let dir = tempfile::tempdir().unwrap();
        // Write a dummy file so the store tries to decrypt it.
        std::fs::write(dir.path().join("secrets.age"), b"not real ciphertext").unwrap();
        let store =
            EncryptedSecretStore::new(dir.path().join("secrets.age"), Arc::new(BadKeyProvider));

        // WHEN
        let err = store.resolve(&["K".to_string()]).unwrap_err();

        // THEN
        assert!(
            matches!(err, SecretError::Locked(_)),
            "expected Locked, got {err}"
        );
    }

    #[test]
    fn get_locked_returns_none() {
        // GIVEN (same bad provider setup as above)
        struct BadKeyProvider;
        impl KeyProvider for BadKeyProvider {
            fn encrypt(&self, _: &[u8]) -> anyhow::Result<Vec<u8>> {
                anyhow::bail!("no key")
            }
            fn decrypt(&self, _: &[u8]) -> anyhow::Result<Vec<u8>> {
                anyhow::bail!("no key")
            }
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("secrets.age"), b"not real ciphertext").unwrap();
        let store =
            EncryptedSecretStore::new(dir.path().join("secrets.age"), Arc::new(BadKeyProvider));

        // WHEN / THEN — no panic
        assert!(store.get("K").is_none());
    }

    #[test]
    fn list_locked_returns_empty() {
        // GIVEN
        struct BadKeyProvider;
        impl KeyProvider for BadKeyProvider {
            fn encrypt(&self, _: &[u8]) -> anyhow::Result<Vec<u8>> {
                anyhow::bail!("no key")
            }
            fn decrypt(&self, _: &[u8]) -> anyhow::Result<Vec<u8>> {
                anyhow::bail!("no key")
            }
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("secrets.age"), b"not real ciphertext").unwrap();
        let store =
            EncryptedSecretStore::new(dir.path().join("secrets.age"), Arc::new(BadKeyProvider));

        // WHEN / THEN — no panic
        assert!(store.list().is_empty());
    }

    // ── Real age x25519 crypto tests ──────────────────────────────────────────

    struct EphemeralX25519KeyProvider {
        identity: age::x25519::Identity,
    }

    impl EphemeralX25519KeyProvider {
        fn generate() -> Self {
            Self {
                identity: age::x25519::Identity::generate(),
            }
        }
    }

    impl KeyProvider for EphemeralX25519KeyProvider {
        fn encrypt(&self, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
            encrypt_age(
                plaintext,
                std::iter::once(
                    Box::new(self.identity.to_public()) as Box<dyn age::Recipient + Send>
                ),
            )
        }
        fn decrypt(&self, ciphertext: &[u8]) -> anyhow::Result<Vec<u8>> {
            decrypt_age_recipients(ciphertext, &self.identity)
        }
    }

    #[test]
    fn x25519_encrypt_decrypt_roundtrip() {
        // GIVEN
        let kp = EphemeralX25519KeyProvider::generate();
        let plaintext = b"real secret value";

        // WHEN
        let ciphertext = kp.encrypt(plaintext).unwrap();
        let recovered = kp.decrypt(&ciphertext).unwrap();

        // THEN
        assert_eq!(recovered, plaintext);
        assert_ne!(ciphertext, plaintext, "output must not be plaintext");
    }

    #[test]
    fn x25519_wrong_key_fails_decrypt() {
        // GIVEN
        let kp1 = EphemeralX25519KeyProvider::generate();
        let kp2 = EphemeralX25519KeyProvider::generate();
        let ciphertext = kp1.encrypt(b"secret").unwrap();

        // WHEN
        let result = kp2.decrypt(&ciphertext);

        // THEN
        assert!(result.is_err());
    }

    #[test]
    fn store_file_is_not_plaintext() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let kp = Arc::new(EphemeralX25519KeyProvider::generate());
        let store = EncryptedSecretStore::new(dir.path().join("secrets.age"), kp);

        // WHEN
        store.set("TOKEN", "super-secret-value").unwrap();

        // THEN — the raw file must not contain the plaintext secret
        let raw = std::fs::read(dir.path().join("secrets.age")).unwrap();
        assert!(!raw
            .windows(b"super-secret-value".len())
            .any(|w| w == b"super-secret-value"));
    }

    #[test]
    fn store_persists_and_decrypts_with_real_crypto() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let identity = age::x25519::Identity::generate();

        let store1 = EncryptedSecretStore::new(
            dir.path().join("secrets.age"),
            Arc::new(EphemeralX25519KeyProvider {
                identity: identity.clone(),
            }),
        );
        store1.set("DB_PASS", "correct-horse").unwrap();

        // WHEN — new store instance, same key
        let store2 = EncryptedSecretStore::new(
            dir.path().join("secrets.age"),
            Arc::new(EphemeralX25519KeyProvider { identity }),
        );

        // THEN
        assert_eq!(store2.get("DB_PASS").as_deref(), Some("correct-horse"));
    }
}

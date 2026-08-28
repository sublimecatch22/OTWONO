//! Secret storage.
//!
//! Provider API keys and relay tokens never touch SQLite. They go to the
//! operating system's credential vault. Where no vault exists — a headless
//! Linux box, a container, a locked-down kiosk — the service falls back to an
//! AES-256-GCM file vault and *reports that it did so*, so the user is never
//! told their key is in the OS keychain when it is not.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const SERVICE: &str = "com.otwono.ai";

/// Which store is actually in use. Reported by the API and shown in Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretBackend {
    /// Windows Credential Manager, macOS Keychain, or Secret Service.
    OperatingSystem,
    /// Encrypted file in the data directory, used when no OS vault is present.
    EncryptedFile,
    /// Process memory only. Tests exclusively.
    Ephemeral,
}

impl SecretBackend {
    pub const fn describe(self) -> &'static str {
        match self {
            Self::OperatingSystem => {
                "Secrets are stored in your operating system's credential manager."
            }
            Self::EncryptedFile => {
                "No operating-system credential manager was available, so secrets are stored \
                 in an encrypted file in your OTWONO data folder, readable only by your user \
                 account."
            }
            Self::Ephemeral => "Secrets are held in memory for this session only.",
        }
    }
}

pub trait SecretStore: Send + Sync {
    fn backend(&self) -> SecretBackend;
    fn set(&self, key: &str, value: &str) -> Result<()>;
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn delete(&self, key: &str) -> Result<()>;
    fn list_keys(&self) -> Result<Vec<String>>;
}

/// Reject key names that could collide or escape the namespace.
fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 128 {
        bail!("secret key must be between 1 and 128 characters");
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
    {
        bail!("secret key {key:?} contains characters that are not allowed");
    }
    Ok(())
}

// ---------------------------------------------------------------- OS vault

pub struct OsSecretStore {
    /// The OS vault cannot be enumerated portably, so the *names* of stored
    /// secrets (never the values) are tracked in a small index file.
    index_path: PathBuf,
}

impl OsSecretStore {
    pub fn new(index_path: PathBuf) -> Self {
        Self { index_path }
    }

    /// Probe the vault with a throwaway entry. Returns an error if the platform
    /// has no usable credential store, which is the signal to fall back.
    pub fn probe(&self) -> Result<()> {
        let entry = keyring::Entry::new(SERVICE, "otwono.probe")?;
        entry.set_password("probe")?;
        let value = entry.get_password()?;
        entry.delete_credential().ok();
        if value != "probe" {
            bail!("credential store returned an unexpected value");
        }
        Ok(())
    }

    fn read_index(&self) -> Vec<String> {
        std::fs::read_to_string(&self.index_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn write_index(&self, keys: &[String]) -> Result<()> {
        if let Some(parent) = self.index_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.index_path, serde_json::to_vec_pretty(keys)?)?;
        crate::paths::restrict_to_owner(&self.index_path).ok();
        Ok(())
    }
}

impl SecretStore for OsSecretStore {
    fn backend(&self) -> SecretBackend {
        SecretBackend::OperatingSystem
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        validate_key(key)?;
        keyring::Entry::new(SERVICE, key)?
            .set_password(value)
            .with_context(|| format!("storing secret {key:?} in the OS credential manager"))?;
        let mut keys = self.read_index();
        if !keys.iter().any(|k| k == key) {
            keys.push(key.to_string());
            keys.sort();
            self.write_index(&keys)?;
        }
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        validate_key(key)?;
        match keyring::Entry::new(SERVICE, key)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow!(e)),
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
        validate_key(key)?;
        match keyring::Entry::new(SERVICE, key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(anyhow!(e)),
        }
        let keys: Vec<String> = self.read_index().into_iter().filter(|k| k != key).collect();
        self.write_index(&keys)
    }

    fn list_keys(&self) -> Result<Vec<String>> {
        Ok(self.read_index())
    }
}

// ------------------------------------------------------- Encrypted fallback

#[derive(Default, Serialize, Deserialize)]
struct VaultFile {
    /// key -> base64(nonce ‖ ciphertext)
    entries: BTreeMap<String, String>,
}

pub struct EncryptedFileSecretStore {
    vault_path: PathBuf,
    cipher: Aes256Gcm,
    lock: Mutex<()>,
}

impl EncryptedFileSecretStore {
    /// Open or create the vault. The 256-bit key lives in its own `0600` file
    /// so that a backup of the vault alone is useless.
    pub fn open(vault_path: PathBuf, key_path: PathBuf) -> Result<Self> {
        let key_bytes: Zeroizing<Vec<u8>> = if key_path.exists() {
            let encoded = std::fs::read_to_string(&key_path)?;
            Zeroizing::new(
                base64::engine::general_purpose::STANDARD
                    .decode(encoded.trim())
                    .context("decoding the vault key file")?,
            )
        } else {
            let mut raw = vec![0u8; 32];
            OsRng.fill_bytes(&mut raw);
            if let Some(parent) = key_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(
                &key_path,
                base64::engine::general_purpose::STANDARD.encode(&raw),
            )?;
            crate::paths::restrict_to_owner(&key_path)?;
            Zeroizing::new(raw)
        };

        if key_bytes.len() != 32 {
            bail!("vault key file is corrupt: expected 32 bytes, found {}", key_bytes.len());
        }
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
        Ok(Self { vault_path, cipher, lock: Mutex::new(()) })
    }

    fn read_vault(&self) -> Result<VaultFile> {
        if !self.vault_path.exists() {
            return Ok(VaultFile::default());
        }
        let text = std::fs::read_to_string(&self.vault_path)?;
        Ok(serde_json::from_str(&text).unwrap_or_default())
    }

    fn write_vault(&self, vault: &VaultFile) -> Result<()> {
        if let Some(parent) = self.vault_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.vault_path, serde_json::to_vec_pretty(vault)?)?;
        crate::paths::restrict_to_owner(&self.vault_path).ok();
        Ok(())
    }
}

impl SecretStore for EncryptedFileSecretStore {
    fn backend(&self) -> SecretBackend {
        SecretBackend::EncryptedFile
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        validate_key(key)?;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), value.as_bytes())
            .map_err(|_| anyhow!("failed to encrypt secret"))?;
        let mut blob = nonce_bytes.to_vec();
        blob.extend_from_slice(&ciphertext);

        let mut vault = self.read_vault()?;
        vault.entries.insert(
            key.to_string(),
            base64::engine::general_purpose::STANDARD.encode(blob),
        );
        self.write_vault(&vault)
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        validate_key(key)?;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let vault = self.read_vault()?;
        let Some(encoded) = vault.entries.get(key) else {
            return Ok(None);
        };
        let blob = base64::engine::general_purpose::STANDARD.decode(encoded)?;
        if blob.len() < 13 {
            bail!("stored secret {key:?} is corrupt");
        }
        let (nonce, ciphertext) = blob.split_at(12);
        let plaintext = self
            .cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| anyhow!("could not decrypt secret {key:?}; the vault key may have changed"))?;
        Ok(Some(String::from_utf8(plaintext)?))
    }

    fn delete(&self, key: &str) -> Result<()> {
        validate_key(key)?;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut vault = self.read_vault()?;
        vault.entries.remove(key);
        self.write_vault(&vault)
    }

    fn list_keys(&self) -> Result<Vec<String>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        Ok(self.read_vault()?.entries.keys().cloned().collect())
    }
}

// -------------------------------------------------------------- Ephemeral

/// In-memory store for tests. Never selected by `open_best`.
#[derive(Default)]
pub struct EphemeralSecretStore {
    entries: Mutex<BTreeMap<String, String>>,
}

impl SecretStore for EphemeralSecretStore {
    fn backend(&self) -> SecretBackend {
        SecretBackend::Ephemeral
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        validate_key(key)?;
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.into(), value.into());
        Ok(())
    }
    fn get(&self, key: &str) -> Result<Option<String>> {
        validate_key(key)?;
        Ok(self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned())
    }
    fn delete(&self, key: &str) -> Result<()> {
        validate_key(key)?;
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
        Ok(())
    }
    fn list_keys(&self) -> Result<Vec<String>> {
        Ok(self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect())
    }
}

/// Choose the best available store: the OS vault when it works, otherwise the
/// encrypted file. The caller is expected to surface `backend()` to the user.
pub fn open_best() -> Result<Box<dyn SecretStore>> {
    let data_dir = crate::paths::data_dir()?;
    let os_store = OsSecretStore::new(data_dir.join("secret-index.json"));
    match os_store.probe() {
        Ok(()) => {
            tracing::info!("using the operating system credential store");
            Ok(Box::new(os_store))
        }
        Err(error) => {
            tracing::warn!(%error, "no OS credential store available; using the encrypted file vault");
            Ok(Box::new(EncryptedFileSecretStore::open(
                crate::paths::vault_path()?,
                crate::paths::vault_key_path()?,
            )?))
        }
    }
}

/// Namespaced key for a provider connection's API credential.
pub fn provider_key(connection_id: &str) -> String {
    format!("provider:{connection_id}")
}

/// Namespaced key for a relay access token.
pub fn relay_token_key(link_id: &str) -> String {
    format!("relay:{link_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_store() -> (tempfile::TempDir, EncryptedFileSecretStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = EncryptedFileSecretStore::open(
            tmp.path().join("vault.bin"),
            tmp.path().join("vault.key"),
        )
        .unwrap();
        (tmp, store)
    }

    #[test]
    fn secrets_round_trip_through_the_encrypted_vault() {
        let (_tmp, store) = file_store();
        store.set("provider:conn_1", "sk-test-value").unwrap();
        assert_eq!(
            store.get("provider:conn_1").unwrap().as_deref(),
            Some("sk-test-value")
        );
        assert_eq!(store.list_keys().unwrap(), vec!["provider:conn_1"]);
        store.delete("provider:conn_1").unwrap();
        assert_eq!(store.get("provider:conn_1").unwrap(), None);
        assert!(store.list_keys().unwrap().is_empty());
    }

    #[test]
    fn the_vault_file_never_contains_the_plaintext() {
        let (tmp, store) = file_store();
        store.set("provider:conn_1", "sk-super-secret-abcdef").unwrap();
        let raw = std::fs::read_to_string(tmp.path().join("vault.bin")).unwrap();
        assert!(
            !raw.contains("sk-super-secret-abcdef"),
            "plaintext leaked into the vault file"
        );
        assert!(raw.contains("provider:conn_1"), "key names are stored in the clear by design");
    }

    #[test]
    fn a_vault_written_with_a_different_key_cannot_be_read() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault.bin");
        let key = tmp.path().join("vault.key");
        EncryptedFileSecretStore::open(vault.clone(), key.clone())
            .unwrap()
            .set("k", "v")
            .unwrap();

        // Replace the key file with a fresh one, as a restore of the vault
        // without its key would.
        std::fs::remove_file(&key).unwrap();
        let store = EncryptedFileSecretStore::open(vault, key).unwrap();
        assert!(store.get("k").is_err());
    }

    #[test]
    fn key_names_are_validated() {
        let (_tmp, store) = file_store();
        assert!(store.set("", "v").is_err());
        assert!(store.set("bad/key", "v").is_err());
        assert!(store.set("bad key", "v").is_err());
        assert!(store.set("good.key-1:2_3", "v").is_ok());
    }

    #[test]
    fn missing_secrets_read_as_none_rather_than_an_error() {
        let (_tmp, store) = file_store();
        assert_eq!(store.get("provider:absent").unwrap(), None);
        store.delete("provider:absent").unwrap();
    }

    #[test]
    fn every_backend_explains_itself_to_the_user() {
        for backend in [
            SecretBackend::OperatingSystem,
            SecretBackend::EncryptedFile,
            SecretBackend::Ephemeral,
        ] {
            assert!(backend.describe().ends_with('.'));
        }
    }

    #[test]
    fn the_ephemeral_store_behaves_like_the_others() {
        let store = EphemeralSecretStore::default();
        store.set("a", "1").unwrap();
        assert_eq!(store.get("a").unwrap().as_deref(), Some("1"));
        store.delete("a").unwrap();
        assert_eq!(store.get("a").unwrap(), None);
    }

    #[test]
    fn namespaced_keys_do_not_collide() {
        assert_ne!(provider_key("x"), relay_token_key("x"));
        assert!(provider_key("conn_1").starts_with("provider:"));
    }
}

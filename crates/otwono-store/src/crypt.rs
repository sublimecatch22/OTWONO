//! Encryption at rest for the chunk store.
//!
//! # Everything is encrypted, whatever its label
//!
//! `docs/security/DATA-VISIBILITY.md` §5 asks for `PRIVATE` to be encrypted and leaves
//! `PUBLIC` in the clear. This encrypts uniformly, which is stronger and — more
//! importantly — is the only version that is *correct*.
//!
//! A chunk is content-addressed and label-agnostic: the same chunk can be referenced by a
//! `PRIVATE` object and a `PUBLIC` one at the same time. Encryption keyed on the label would
//! have to answer "which object referenced this chunk first?", and every answer to that is a
//! bug. So the label governs **who may read an object**; it does not govern whether the
//! bytes on disk are encrypted. A stolen disk reveals nothing either way.
//!
//! # Digests are over plaintext
//!
//! A chunk's name is the BLAKE3 of its *plaintext*, so two nodes with different storage keys
//! still agree on what a chunk is called. Encryption is a property of this disk, not of the
//! content — otherwise the cluster cache could not exist.
//!
//! # The nonce
//!
//! XChaCha20-Poly1305's 192-bit nonce is why it is here rather than the AES-GCM the rest of
//! the world reaches for. A random nonce per chunk is safe at any volume this store will
//! ever reach, with no counter to persist across restarts and no chance of the
//! catastrophic-reuse bug that a counter gets wrong exactly once.
//!
//! The plaintext digest is bound in as associated data, so a chunk file moved to another
//! chunk's name fails to decrypt rather than decrypting into the wrong answer.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// Where an image keeps the storage key. Outside the store directory on purpose: the store
/// is a thing to back up, and the key is not.
pub const DEFAULT_KEY_PATH: &str = "/var/lib/otwono/storage.key";

const NONCE_BYTES: usize = 24;
const KEY_BYTES: usize = 32;
/// Poly1305's tag. Named so the overhead arithmetic below reads as arithmetic.
const TAG_BYTES: usize = 16;

/// Bytes a sealed chunk costs beyond its plaintext.
pub const SEAL_OVERHEAD: usize = NONCE_BYTES + TAG_BYTES;

/// The node's storage key, zeroized when it goes out of scope.
pub struct StorageKey(Zeroizing<[u8; KEY_BYTES]>);

#[derive(Debug)]
pub enum CryptError {
    /// The key file is unreadable, unwritable, or not there when it must be.
    Key { path: PathBuf, reason: String },
    /// The key file is present but not a key.
    Malformed { path: PathBuf, reason: String },
    /// The key file is readable by more than its owner.
    Permissions { path: PathBuf, mode: u32 },
    /// The bytes do not decrypt: wrong key, damage, or a chunk moved to another name.
    Undecryptable,
    /// Too short to contain a nonce and a tag.
    Truncated { len: usize },
}

impl std::fmt::Display for CryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptError::Key { path, reason } => write!(f, "{}: {reason}", path.display()),
            CryptError::Malformed { path, reason } => {
                write!(f, "{} is not a storage key: {reason}", path.display())
            }
            CryptError::Permissions { path, mode } => write!(
                f,
                "{} is mode {mode:04o}; a storage key must be readable only by its owner",
                path.display()
            ),
            CryptError::Undecryptable => write!(
                f,
                "the stored bytes do not decrypt: the wrong key, damage, or a chunk file \
                 moved to another chunk's name"
            ),
            CryptError::Truncated { len } => {
                write!(f, "{len} bytes is too short to be a sealed chunk")
            }
        }
    }
}

impl std::error::Error for CryptError {}

impl StorageKey {
    /// Load the key, generating one on first use.
    ///
    /// Generation is the ordinary first-boot path, so it is not an error — but it is said
    /// out loud by the caller, because from that moment the key is the only thing standing
    /// between a stolen disk and its contents, and it is not backed up anywhere.
    pub fn load_or_generate(path: &Path) -> Result<(StorageKey, bool), CryptError> {
        match std::fs::read(path) {
            Ok(bytes) => {
                check_permissions(path)?;
                let key: [u8; KEY_BYTES] =
                    bytes.as_slice().try_into().map_err(|_| CryptError::Malformed {
                        path: path.to_path_buf(),
                        reason: format!("{} bytes, expected {KEY_BYTES}", bytes.len()),
                    })?;
                Ok((StorageKey(Zeroizing::new(key)), false))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = Self::generate();
                key.write_to(path)?;
                Ok((key, true))
            }
            Err(e) => Err(CryptError::Key {
                path: path.to_path_buf(),
                reason: e.to_string(),
            }),
        }
    }

    pub fn generate() -> StorageKey {
        let mut key = Zeroizing::new([0u8; KEY_BYTES]);
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, key.as_mut());
        StorageKey(key)
    }

    /// Write the key out 0600, creating it exclusively so an existing key is never
    /// overwritten by a racing start.
    fn write_to(&self, path: &Path) -> Result<(), CryptError> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CryptError::Key {
                path: parent.to_path_buf(),
                reason: e.to_string(),
            })?;
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| CryptError::Key {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })?;
        f.write_all(self.0.as_ref()).map_err(|e| CryptError::Key {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(self.0.as_ref().into())
    }

    /// Encrypt one chunk, binding it to the name it will be stored under.
    pub fn seal(&self, plaintext: &[u8], digest: &[u8; 32]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_BYTES];
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut nonce);
        let sealed = self
            .cipher()
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: digest,
                },
            )
            // Only fails on a length that cannot occur: chunks are bounded at 256 KiB.
            .expect("XChaCha20-Poly1305 cannot fail on a bounded chunk");
        let mut out = Vec::with_capacity(NONCE_BYTES + sealed.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        out
    }

    /// Decrypt one chunk, requiring it to be the chunk that name promises.
    pub fn open(&self, sealed: &[u8], digest: &[u8; 32]) -> Result<Vec<u8>, CryptError> {
        if sealed.len() < SEAL_OVERHEAD {
            return Err(CryptError::Truncated { len: sealed.len() });
        }
        let (nonce, body) = sealed.split_at(NONCE_BYTES);
        self.cipher()
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: body,
                    aad: digest,
                },
            )
            .map_err(|_| CryptError::Undecryptable)
    }
}

impl std::fmt::Debug for StorageKey {
    /// Never print the key. A key in a log is a key on someone else's disk.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StorageKey(<redacted>)")
    }
}

fn check_permissions(path: &Path) -> Result<(), CryptError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| CryptError::Key {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(CryptError::Permissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "otwono-key-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("temp dir");
        d
    }

    fn digest(b: &[u8]) -> [u8; 32] {
        *blake3::hash(b).as_bytes()
    }

    #[test]
    fn what_goes_in_comes_back_out() {
        let k = StorageKey::generate();
        for payload in [b"".as_slice(), b"short", &[7u8; 262_144]] {
            let d = digest(payload);
            assert_eq!(k.open(&k.seal(payload, &d), &d).expect("open"), payload);
        }
    }

    #[test]
    fn the_ciphertext_does_not_contain_the_plaintext() {
        // The point of the exercise, asserted rather than assumed.
        let k = StorageKey::generate();
        let payload = b"the quick brown fox jumps over the lazy dog".repeat(100);
        let sealed = k.seal(&payload, &digest(&payload));
        assert!(
            !sealed.windows(20).any(|w| w == &payload[..20]),
            "plaintext survived into the ciphertext"
        );
    }

    #[test]
    fn a_chunk_moved_to_another_chunks_name_does_not_decrypt() {
        // Without binding the digest as associated data this would succeed, and the file
        // would decrypt into a valid chunk under the wrong name — a silent substitution
        // that only the store's own digest check would notice.
        let k = StorageKey::generate();
        let a = b"chunk a".repeat(100);
        let b = b"chunk b".repeat(100);
        let sealed_a = k.seal(&a, &digest(&a));
        assert!(matches!(
            k.open(&sealed_a, &digest(&b)),
            Err(CryptError::Undecryptable)
        ));
    }

    #[test]
    fn a_different_key_does_not_open_it() {
        let payload = b"secrets".repeat(50);
        let d = digest(&payload);
        let sealed = StorageKey::generate().seal(&payload, &d);
        assert!(matches!(
            StorageKey::generate().open(&sealed, &d),
            Err(CryptError::Undecryptable)
        ));
    }

    #[test]
    fn a_single_flipped_bit_is_detected() {
        // AEAD, not just encryption: damage is an error rather than garbage output.
        let k = StorageKey::generate();
        let payload = b"integrity matters".repeat(100);
        let d = digest(&payload);
        let mut sealed = k.seal(&payload, &d);
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert!(matches!(k.open(&sealed, &d), Err(CryptError::Undecryptable)));
    }

    #[test]
    fn truncation_is_reported_rather_than_panicking() {
        let k = StorageKey::generate();
        for len in [0, 1, NONCE_BYTES, SEAL_OVERHEAD - 1] {
            assert!(matches!(
                k.open(&vec![0u8; len], &[0u8; 32]),
                Err(CryptError::Truncated { .. })
            ));
        }
    }

    #[test]
    fn sealing_the_same_bytes_twice_gives_different_ciphertext() {
        // A fresh nonce each time. Identical ciphertext would leak that two chunks are
        // equal to anyone who can see the disk.
        let k = StorageKey::generate();
        let payload = b"same bytes".repeat(100);
        let d = digest(&payload);
        assert_ne!(k.seal(&payload, &d), k.seal(&payload, &d));
    }

    #[test]
    fn a_key_is_generated_on_first_use_and_reused_after() {
        let dir = tmp("firstuse");
        let path = dir.join("storage.key");
        let (first, generated) = StorageKey::load_or_generate(&path).expect("generate");
        assert!(generated, "the first call generates");

        let (second, generated) = StorageKey::load_or_generate(&path).expect("load");
        assert!(!generated, "the second call loads");

        // The same key, demonstrated by using it rather than by comparing secrets.
        let payload = b"round trip".repeat(20);
        let d = digest(&payload);
        assert_eq!(second.open(&first.seal(&payload, &d), &d).expect("open"), payload);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_generated_key_is_not_readable_by_anyone_else() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("mode");
        let path = dir.join("storage.key");
        StorageKey::load_or_generate(&path).expect("generate");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "generated key is mode {mode:04o}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_world_readable_key_is_refused_rather_than_used() {
        // Loading it anyway would mean the node runs believing its data is protected.
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("loose");
        let path = dir.join("storage.key");
        StorageKey::load_or_generate(&path).expect("generate");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            StorageKey::load_or_generate(&path),
            Err(CryptError::Permissions { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_file_of_the_wrong_length_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("short");
        let path = dir.join("storage.key");
        std::fs::write(&path, b"too short").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            StorageKey::load_or_generate(&path),
            Err(CryptError::Malformed { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_key_never_prints_itself() {
        assert_eq!(format!("{:?}", StorageKey::generate()), "StorageKey(<redacted>)");
    }
}

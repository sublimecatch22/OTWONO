//! On-disk keystore.
//!
//! ```text
//! /var/lib/otwono/identity/
//!   node.key        0600  signing seed, agreement seed, creation time
//!   node.pub        0644  the public identity, safe to copy anywhere
//!   succession.jsonl 0644 signed rotation records, append-only
//! ```
//!
//! # What this does not do
//!
//! **No TPM sealing.** ADR-0006 says the key should be sealed to a TPM or TrustZone
//! keystore where one exists; this stores it in a file. The metadata records
//! `hardware_backed: false` so nothing downstream can claim protection that is not there,
//! and so a later implementation has somewhere to say otherwise.
//!
//! **No encrypted backup yet.** The passphrase-derived export promised at first boot is
//! not implemented. Until it is, losing this file loses the identity, and the first-boot
//! experience must not imply otherwise.

use crate::{base64_decode, base64_encode, IdentityError, NodeId, NodeIdentity, SCHEMA_VERSION};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

pub const DEFAULT_IDENTITY_DIR: &str = "/var/lib/otwono/identity";
const KEY_FILE: &str = "node.key";
const PUB_FILE: &str = "node.pub";
const SUCCESSION_FILE: &str = "succession.jsonl";

/// The private key file's contents.
#[derive(Serialize, Deserialize)]
pub struct StoredIdentity {
    pub schema_version: String,
    pub algorithm: String,
    signing_seed: String,
    agreement_seed: String,
    pub created_at_unix_ms: u64,
    /// False until TPM/TrustZone sealing exists. Never set this optimistically.
    pub hardware_backed: bool,
}

/// A signed statement that one identity succeeds another.
///
/// Rotation without this would be indistinguishable from impersonation: peers would have
/// no way to tell a legitimate new key from an attacker's. The old key signing the new one
/// is what carries the trust across.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuccessionRecord {
    pub schema_version: String,
    pub previous_node_id: NodeId,
    pub new_node_id: NodeId,
    /// Base64 Ed25519 public key of the new identity.
    pub new_public_key: String,
    pub rotated_at_unix_ms: u64,
    /// Base64 signature by the *previous* key over the succession message.
    pub signature: String,
}

impl SuccessionRecord {
    /// Verify that the previous identity really endorsed the new one.
    pub fn verify(&self, previous_public_key: &[u8; 32]) -> Result<(), IdentityError> {
        if !self.previous_node_id.matches_public_key(previous_public_key) {
            return Err(IdentityError::NodeIdMismatch);
        }
        let new_key: [u8; 32] = base64_decode(&self.new_public_key)?
            .as_slice()
            .try_into()
            .map_err(|_| IdentityError::MalformedKey("new key must be 32 bytes".into()))?;
        if !self.new_node_id.matches_public_key(&new_key) {
            return Err(IdentityError::NodeIdMismatch);
        }
        crate::verify_signature(
            previous_public_key,
            &succession_message(&self.previous_node_id, &self.new_node_id, self.rotated_at_unix_ms),
            &base64_decode(&self.signature)?,
        )
    }
}

fn succession_message(previous: &NodeId, new: &NodeId, at: u64) -> Vec<u8> {
    format!(
        "otwono-succession-v1:{}:{}:{at}",
        previous.to_text(),
        new.to_text()
    )
    .into_bytes()
}

pub struct Keystore {
    dir: PathBuf,
}

impl Keystore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Keystore {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn key_path(&self) -> PathBuf {
        self.dir.join(KEY_FILE)
    }

    pub fn public_path(&self) -> PathBuf {
        self.dir.join(PUB_FILE)
    }

    pub fn succession_path(&self) -> PathBuf {
        self.dir.join(SUCCESSION_FILE)
    }

    pub fn exists(&self) -> bool {
        self.key_path().exists()
    }

    /// Load the identity, or generate and persist one if there is none.
    ///
    /// This is what runs at first boot. It is deliberately the only way a node gets an
    /// identity: no code path anywhere else invents one.
    pub fn load_or_generate(&self) -> Result<(NodeIdentity, bool), KeystoreError> {
        if self.exists() {
            Ok((self.load()?, false))
        } else {
            let identity = NodeIdentity::generate().map_err(KeystoreError::Identity)?;
            self.persist(&identity)?;
            Ok((identity, true))
        }
    }

    pub fn load(&self) -> Result<NodeIdentity, KeystoreError> {
        let path = self.key_path();
        let text = Zeroizing::new(
            std::fs::read_to_string(&path)
                .map_err(|e| KeystoreError::Io(format!("{}: {e}", path.display())))?,
        );

        // A key file readable by anyone is a compromised key, not a warning. Refusing is
        // the only honest response: the node cannot know who has already read it.
        let mode = std::fs::metadata(&path)
            .map_err(|e| KeystoreError::Io(format!("{}: {e}", path.display())))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Err(KeystoreError::InsecurePermissions { path, mode });
        }

        let stored: StoredIdentity = serde_json::from_str(&text)
            .map_err(|e| KeystoreError::Malformed(format!("{}: {e}", path.display())))?;
        if stored.algorithm != "ed25519" {
            return Err(KeystoreError::Malformed(format!(
                "unsupported algorithm {:?}; this build understands ed25519",
                stored.algorithm
            )));
        }

        let signing = decode_seed(&stored.signing_seed)?;
        let agreement = decode_seed(&stored.agreement_seed)?;
        Ok(NodeIdentity::from_seeds(
            &signing,
            &agreement,
            stored.created_at_unix_ms,
        ))
    }

    /// Write the identity. The key file is created 0600 *before* any bytes reach it.
    pub fn persist(&self, identity: &NodeIdentity) -> Result<(), KeystoreError> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| KeystoreError::Io(format!("{}: {e}", self.dir.display())))?;
        std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| KeystoreError::Io(format!("{}: {e}", self.dir.display())))?;

        let stored = StoredIdentity {
            schema_version: SCHEMA_VERSION.to_string(),
            algorithm: "ed25519".to_string(),
            signing_seed: base64_encode(identity.signing_seed().as_ref()),
            agreement_seed: base64_encode(identity.agreement_seed().as_ref()),
            created_at_unix_ms: identity.created_at_unix_ms(),
            hardware_backed: false,
        };
        let body = Zeroizing::new(
            serde_json::to_string_pretty(&stored).map_err(|e| KeystoreError::Malformed(e.to_string()))?,
        );

        // mode() on OpenOptions sets the permissions at creation, so there is never an
        // instant where the file exists world-readable.
        let key_path = self.key_path();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&key_path)
            .map_err(|e| KeystoreError::Io(format!("{}: {e}", key_path.display())))?;
        file.write_all(body.as_bytes())
            .map_err(|e| KeystoreError::Io(format!("{}: {e}", key_path.display())))?;
        file.sync_all()
            .map_err(|e| KeystoreError::Io(format!("{}: {e}", key_path.display())))?;
        // Re-assert the mode: an existing file keeps its old permissions through create().
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| KeystoreError::Io(format!("{}: {e}", key_path.display())))?;

        let public = serde_json::to_string_pretty(&identity.to_public())
            .map_err(|e| KeystoreError::Malformed(e.to_string()))?;
        std::fs::write(self.public_path(), public + "\n")
            .map_err(|e| KeystoreError::Io(format!("{}: {e}", self.public_path().display())))?;
        std::fs::set_permissions(self.public_path(), std::fs::Permissions::from_mode(0o644))
            .map_err(|e| KeystoreError::Io(format!("{}: {e}", self.public_path().display())))?;
        Ok(())
    }

    /// Replace the identity with a fresh one, endorsed by the outgoing key.
    pub fn rotate(&self, now_unix_ms: u64) -> Result<(NodeIdentity, SuccessionRecord), KeystoreError> {
        let previous = self.load()?;
        let new = NodeIdentity::generate().map_err(KeystoreError::Identity)?;

        let message = succession_message(previous.node_id(), new.node_id(), now_unix_ms);
        let record = SuccessionRecord {
            schema_version: SCHEMA_VERSION.to_string(),
            previous_node_id: *previous.node_id(),
            new_node_id: *new.node_id(),
            new_public_key: base64_encode(&new.public_key_bytes()),
            rotated_at_unix_ms: now_unix_ms,
            signature: base64_encode(&previous.sign(&message).to_bytes()),
        };

        // Append the record before overwriting the key: if this process dies between the
        // two, the chain still shows what was intended and the old key still works.
        let line = serde_json::to_string(&record).map_err(|e| KeystoreError::Malformed(e.to_string()))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o644)
            .open(self.succession_path())
            .map_err(|e| KeystoreError::Io(format!("{}: {e}", self.succession_path().display())))?;
        writeln!(file, "{line}")
            .map_err(|e| KeystoreError::Io(format!("{}: {e}", self.succession_path().display())))?;
        file.sync_all()
            .map_err(|e| KeystoreError::Io(format!("{}: {e}", self.succession_path().display())))?;

        self.persist(&new)?;
        Ok((new, record))
    }

    pub fn succession_records(&self) -> Result<Vec<SuccessionRecord>, KeystoreError> {
        let text = match std::fs::read_to_string(self.succession_path()) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(KeystoreError::Io(e.to_string())),
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(|e| KeystoreError::Malformed(e.to_string())))
            .collect()
    }
}

fn decode_seed(text: &str) -> Result<[u8; 32], KeystoreError> {
    base64_decode(text)
        .map_err(KeystoreError::Identity)?
        .as_slice()
        .try_into()
        .map_err(|_| KeystoreError::Malformed("a seed must be 32 bytes".into()))
}

#[derive(Debug)]
pub enum KeystoreError {
    Io(String),
    Malformed(String),
    Identity(IdentityError),
    InsecurePermissions { path: PathBuf, mode: u32 },
}

impl std::fmt::Display for KeystoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeystoreError::Io(e) => write!(f, "{e}"),
            KeystoreError::Malformed(e) => write!(f, "malformed keystore: {e}"),
            KeystoreError::Identity(e) => write!(f, "{e}"),
            KeystoreError::InsecurePermissions { path, mode } => write!(
                f,
                "{} is mode {mode:04o}; a node key readable beyond its owner must be treated as \
                 compromised. Rotate it, then re-run with mode 0600.",
                path.display()
            ),
        }
    }
}

impl std::error::Error for KeystoreError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("otwono-ks-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn first_run_generates_and_persists() {
        let ks = Keystore::new(tmpdir("gen"));
        let (identity, generated) = ks.load_or_generate().unwrap();
        assert!(generated, "the first call must generate");
        assert!(ks.exists());
        assert!(ks.public_path().exists());

        let (again, generated) = ks.load_or_generate().unwrap();
        assert!(!generated, "the second call must load, not regenerate");
        assert_eq!(
            identity.node_id(),
            again.node_id(),
            "identity must survive a reload"
        );
        let _ = std::fs::remove_dir_all(ks.dir());
    }

    #[test]
    fn the_key_file_is_owner_only_and_the_public_file_is_not() {
        let ks = Keystore::new(tmpdir("modes"));
        ks.load_or_generate().unwrap();
        let key_mode = std::fs::metadata(ks.key_path()).unwrap().permissions().mode() & 0o777;
        let pub_mode = std::fs::metadata(ks.public_path()).unwrap().permissions().mode() & 0o777;
        let dir_mode = std::fs::metadata(ks.dir()).unwrap().permissions().mode() & 0o777;
        assert_eq!(key_mode, 0o600, "the private key must be owner-only");
        assert_eq!(pub_mode, 0o644);
        assert_eq!(dir_mode, 0o700);
        let _ = std::fs::remove_dir_all(ks.dir());
    }

    #[test]
    fn a_world_readable_key_is_refused_rather_than_used() {
        let ks = Keystore::new(tmpdir("insecure"));
        ks.load_or_generate().unwrap();
        std::fs::set_permissions(ks.key_path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = ks.load().unwrap_err();
        assert!(
            matches!(err, KeystoreError::InsecurePermissions { .. }),
            "expected a permissions refusal, got {err}"
        );
        assert!(err.to_string().contains("compromised"), "{err}");
        let _ = std::fs::remove_dir_all(ks.dir());
    }

    #[test]
    fn the_stored_key_never_claims_hardware_backing() {
        let ks = Keystore::new(tmpdir("hw"));
        ks.load_or_generate().unwrap();
        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(ks.key_path()).unwrap()).unwrap();
        assert_eq!(stored["hardware_backed"], false, "TPM sealing is not implemented");
        let _ = std::fs::remove_dir_all(ks.dir());
    }

    #[test]
    fn the_published_public_file_is_self_consistent() {
        let ks = Keystore::new(tmpdir("pub"));
        let (identity, _) = ks.load_or_generate().unwrap();
        let public: crate::PublicIdentity =
            serde_json::from_str(&std::fs::read_to_string(ks.public_path()).unwrap()).unwrap();
        assert!(public.is_self_consistent());
        assert_eq!(public.node_id, *identity.node_id());
        let _ = std::fs::remove_dir_all(ks.dir());
    }

    #[test]
    fn rotation_produces_a_record_the_old_key_endorses() {
        let ks = Keystore::new(tmpdir("rotate"));
        let (old, _) = ks.load_or_generate().unwrap();
        let old_public = old.public_key_bytes();
        let old_id = *old.node_id();

        let (new, record) = ks.rotate(1_700_000_000_000).unwrap();
        assert_ne!(new.node_id(), &old_id, "rotation must produce a new identity");
        assert_eq!(record.previous_node_id, old_id);
        assert_eq!(record.new_node_id, *new.node_id());
        record
            .verify(&old_public)
            .expect("the outgoing key must endorse the new one");

        // The keystore now holds the new identity.
        assert_eq!(ks.load().unwrap().node_id(), new.node_id());
        assert_eq!(ks.succession_records().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(ks.dir());
    }

    #[test]
    fn a_succession_record_signed_by_the_wrong_key_is_rejected() {
        // Otherwise anyone could declare themselves the successor to any node.
        let ks = Keystore::new(tmpdir("forge"));
        ks.load_or_generate().unwrap();
        let (_, record) = ks.rotate(1_700_000_000_000).unwrap();
        let impostor = NodeIdentity::generate().unwrap();
        assert!(record.verify(&impostor.public_key_bytes()).is_err());
        let _ = std::fs::remove_dir_all(ks.dir());
    }

    #[test]
    fn a_tampered_successor_key_is_rejected() {
        let ks = Keystore::new(tmpdir("swap"));
        let (old, _) = ks.load_or_generate().unwrap();
        let old_public = old.public_key_bytes();
        let (_, mut record) = ks.rotate(1_700_000_000_000).unwrap();
        let attacker = NodeIdentity::generate().unwrap();
        record.new_public_key = base64_encode(&attacker.public_key_bytes());
        assert_eq!(record.verify(&old_public), Err(IdentityError::NodeIdMismatch));
        let _ = std::fs::remove_dir_all(ks.dir());
    }

    #[test]
    fn successive_rotations_append_rather_than_replace() {
        let ks = Keystore::new(tmpdir("chain"));
        ks.load_or_generate().unwrap();
        ks.rotate(1).unwrap();
        ks.rotate(2).unwrap();
        let records = ks.succession_records().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0].new_node_id, records[1].previous_node_id,
            "the chain must link"
        );
        let _ = std::fs::remove_dir_all(ks.dir());
    }

    #[test]
    fn a_malformed_key_file_is_an_error_not_a_new_identity() {
        // Silently regenerating would change the node's name and orphan every peer
        // relationship it had.
        let ks = Keystore::new(tmpdir("corrupt"));
        std::fs::create_dir_all(ks.dir()).unwrap();
        std::fs::write(ks.key_path(), "{not json").unwrap();
        std::fs::set_permissions(ks.key_path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(ks.load(), Err(KeystoreError::Malformed(_))));
        assert!(matches!(ks.load_or_generate(), Err(KeystoreError::Malformed(_))));
        let _ = std::fs::remove_dir_all(ks.dir());
    }
}

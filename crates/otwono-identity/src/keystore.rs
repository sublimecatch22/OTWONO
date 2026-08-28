//! On-disk keystore, split by who is allowed to read what.
//!
//! ```text
//! /var/lib/otwono/identity/
//!   node.key         0600  Ed25519 signing seed + the bound agreement public key   (otwono-idd)
//!   agreement.key    0600  X25519 agreement secret                                 (otwono-netd)
//!   sharing.key      0600  X25519 sharing secret                                   (otwono-idd)
//!   node.pub         0644  the public identity, safe to copy anywhere
//!   succession.jsonl 0644  signed rotation records, append-only
//! ```
//!
//! Separate files, not one, because different daemons need different halves and neither
//! should be able to read the other's (ADR-0010). `node.key` never contains an agreement
//! *secret*; it records the agreement *public* key so `otwono-idd` can say what it has
//! vouched for without holding anything it does not need.
//!
//! `sharing.key` is `otwono-idd`'s second secret (ADR-0019) and lives in its own file for
//! the same reason: two secrets held by one daemon can still be backed up, rotated and
//! eventually TPM-sealed on their own schedules, and adding it did not have to change
//! `node.key`'s schema on every node that already has one.
//!
//! # Upgrading from the single-file layout
//!
//! Phase 3 wrote both seeds into `node.key`. [`SigningKeystore::load`] still reads that
//! file, and [`migrate_combined`] splits it in place, preserving the node's existing
//! agreement key so its published `node.pub` does not change. The migration runs once, in
//! `otwono-idd`, and removes the agreement seed from `node.key` when it is done.
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

use crate::{
    base64_decode, base64_encode, AgreementKey, IdentityError, NodeId, NodeIdentity, SharingKey,
    SigningIdentity, SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

pub const DEFAULT_IDENTITY_DIR: &str = "/var/lib/otwono/identity";
pub const SIGNING_KEY_FILE: &str = "node.key";
pub const AGREEMENT_KEY_FILE: &str = "agreement.key";
pub const SHARING_KEY_FILE: &str = "sharing.key";
const PUB_FILE: &str = "node.pub";
const SUCCESSION_FILE: &str = "succession.jsonl";

/// `node.key`: what the signing daemon stores.
#[derive(Serialize, Deserialize)]
pub struct StoredSigningKey {
    pub schema_version: String,
    pub algorithm: String,
    signing_seed: String,
    /// Base64 X25519 public key this signing key has vouched for, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agreement_public_key: Option<String>,
    /// Base64 X25519 *sharing* public key this signing key has vouched for, if any
    /// (ADR-0019). Public halves only, for the same reason as above: this file records
    /// what the signing key stands behind, never a secret it has no use for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing_public_key: Option<String>,
    /// Present only in a pre-split keystore. Read for migration, never written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agreement_seed: Option<String>,
    pub created_at_unix_ms: u64,
    /// False until TPM/TrustZone sealing exists. Never set this optimistically.
    pub hardware_backed: bool,
}

/// `sharing.key`: the third key (ADR-0019), held by the identity daemon.
///
/// A separate file rather than a field in `node.key` so that the two secrets the identity
/// daemon holds can be handled, backed up and eventually TPM-sealed independently — and so
/// that adding this did not change `node.key`'s schema, which every existing node already
/// has on disk.
#[derive(Serialize, Deserialize)]
pub struct StoredSharingKey {
    pub schema_version: String,
    pub algorithm: String,
    sharing_seed: String,
    pub created_at_unix_ms: u64,
    pub hardware_backed: bool,
}

/// `agreement.key`: what the mesh daemon stores.
#[derive(Serialize, Deserialize)]
pub struct StoredAgreementKey {
    pub schema_version: String,
    pub algorithm: String,
    agreement_seed: String,
    pub created_at_unix_ms: u64,
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

/// A private key file that must be owner-only.
///
/// A key file readable by anyone is a compromised key, not a warning. Refusing is the only
/// honest response: the node cannot know who has already read it.
fn read_private(path: &Path) -> Result<Zeroizing<String>, KeystoreError> {
    let text = Zeroizing::new(
        std::fs::read_to_string(path).map_err(|e| KeystoreError::Io(format!("{}: {e}", path.display())))?,
    );
    let mode = std::fs::metadata(path)
        .map_err(|e| KeystoreError::Io(format!("{}: {e}", path.display())))?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        return Err(KeystoreError::InsecurePermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(text)
}

/// Write a private key file. Created 0600 *before* any bytes reach it.
fn write_private(path: &Path, body: &str) -> Result<(), KeystoreError> {
    // mode() on OpenOptions sets the permissions at creation, so there is never an
    // instant where the file exists world-readable.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| KeystoreError::Io(format!("{}: {e}", path.display())))?;
    file.write_all(body.as_bytes())
        .map_err(|e| KeystoreError::Io(format!("{}: {e}", path.display())))?;
    file.sync_all()
        .map_err(|e| KeystoreError::Io(format!("{}: {e}", path.display())))?;
    // Re-assert the mode: an existing file keeps its old permissions through create().
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| KeystoreError::Io(format!("{}: {e}", path.display())))
}

fn ensure_dir(dir: &Path) -> Result<(), KeystoreError> {
    std::fs::create_dir_all(dir).map_err(|e| KeystoreError::Io(format!("{}: {e}", dir.display())))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| KeystoreError::Io(format!("{}: {e}", dir.display())))
}

/// The Ed25519 half of the keystore. Only `otwono-idd` opens this.
pub struct SigningKeystore {
    dir: PathBuf,
}

impl SigningKeystore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        SigningKeystore {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn key_path(&self) -> PathBuf {
        self.dir.join(SIGNING_KEY_FILE)
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

    /// Load the signing key, or generate and persist one if there is none.
    ///
    /// This is what runs at first boot. It is deliberately the only way a node gets a
    /// name: no code path anywhere else invents one.
    pub fn load_or_generate(&self) -> Result<(SigningIdentity, bool), KeystoreError> {
        if self.exists() {
            Ok((self.load()?, false))
        } else {
            let identity = SigningIdentity::generate().map_err(KeystoreError::Identity)?;
            self.persist(&identity, None)?;
            Ok((identity, true))
        }
    }

    pub fn load(&self) -> Result<SigningIdentity, KeystoreError> {
        let stored = self.load_stored()?;
        Ok(SigningIdentity::from_seed(
            &decode_seed(&stored.signing_seed)?,
            stored.created_at_unix_ms,
        ))
    }

    fn load_stored(&self) -> Result<StoredSigningKey, KeystoreError> {
        let path = self.key_path();
        let text = read_private(&path)?;
        let stored: StoredSigningKey = serde_json::from_str(&text)
            .map_err(|e| KeystoreError::Malformed(format!("{}: {e}", path.display())))?;
        if stored.algorithm != "ed25519" {
            return Err(KeystoreError::Malformed(format!(
                "unsupported algorithm {:?}; this build understands ed25519",
                stored.algorithm
            )));
        }
        Ok(stored)
    }

    /// The sharing public key this signing key has vouched for, if any (ADR-0019).
    pub fn bound_sharing_public_key(&self) -> Result<Option<[u8; 32]>, KeystoreError> {
        match self.load_stored()?.sharing_public_key {
            Some(text) => Ok(Some(decode_seed(&text)?)),
            None => Ok(None),
        }
    }

    /// The agreement public key this signing key has vouched for, if any.
    pub fn bound_agreement_public_key(&self) -> Result<Option<[u8; 32]>, KeystoreError> {
        match self.load_stored()?.agreement_public_key {
            Some(text) => Ok(Some(decode_seed(&text)?)),
            None => Ok(None),
        }
    }

    /// Write the signing key, recording which agreement key it vouches for.
    ///
    /// Any recorded sharing key is preserved: writing the agreement binding must not
    /// silently un-vouch for the other one. Use [`persist_all`](Self::persist_all) to say
    /// what happens to both.
    pub fn persist(
        &self,
        identity: &SigningIdentity,
        agreement_public_key: Option<&[u8; 32]>,
    ) -> Result<(), KeystoreError> {
        let sharing = if self.exists() {
            self.bound_sharing_public_key()?
        } else {
            None
        };
        self.persist_all(identity, agreement_public_key, sharing.as_ref())
    }

    /// Write the signing key and say explicitly what it vouches for, in both directions.
    pub fn persist_all(
        &self,
        identity: &SigningIdentity,
        agreement_public_key: Option<&[u8; 32]>,
        sharing_public_key: Option<&[u8; 32]>,
    ) -> Result<(), KeystoreError> {
        ensure_dir(&self.dir)?;
        let stored = StoredSigningKey {
            schema_version: SCHEMA_VERSION.to_string(),
            algorithm: "ed25519".to_string(),
            signing_seed: base64_encode(identity.seed().as_ref()),
            agreement_public_key: agreement_public_key.map(|k| base64_encode(k)),
            sharing_public_key: sharing_public_key.map(|k| base64_encode(k)),
            agreement_seed: None,
            created_at_unix_ms: identity.created_at_unix_ms(),
            hardware_backed: false,
        };
        let body = Zeroizing::new(
            serde_json::to_string_pretty(&stored).map_err(|e| KeystoreError::Malformed(e.to_string()))?,
        );
        write_private(&self.key_path(), &body)?;

        // The published file can only be written once there is an agreement key to
        // publish. Before that the node has a name but nothing to handshake with.
        if let Some(agreement) = agreement_public_key {
            let mut published = identity.to_public(agreement);
            if let Some(sharing) = sharing_public_key {
                published = published.with_sharing_binding(identity.bind_sharing(sharing));
            }
            let public = serde_json::to_string_pretty(&published)
                .map_err(|e| KeystoreError::Malformed(e.to_string()))?;
            std::fs::write(self.public_path(), public + "\n")
                .map_err(|e| KeystoreError::Io(format!("{}: {e}", self.public_path().display())))?;
            std::fs::set_permissions(self.public_path(), std::fs::Permissions::from_mode(0o644))
                .map_err(|e| KeystoreError::Io(format!("{}: {e}", self.public_path().display())))?;
        } else if let Err(e) = std::fs::remove_file(self.public_path()) {
            // Nothing to publish. After a rotation there *is* a node.pub on disk, and it
            // names the identity this node no longer has — a file that answers "who are
            // you?" with a dead NodeID is worse than no file at all. Removing it is the
            // only honest state until something re-binds.
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(KeystoreError::Io(format!(
                    "{}: {e}",
                    self.public_path().display()
                )));
            }
        }
        Ok(())
    }

    /// Record which agreement key this node now uses, and republish `node.pub`.
    ///
    /// Takes the identity rather than reloading it. The caller already holds the current
    /// one, and re-reading private key material from disk on every bind would be both a
    /// pointless exposure and a race with [`rotate`](Self::rotate) — a bind that reloaded
    /// mid-rotation would vouch for the agreement key under whichever signing key happened
    /// to be on disk at that instant.
    pub fn bind_agreement(
        &self,
        identity: &SigningIdentity,
        agreement_public_key: &[u8; 32],
    ) -> Result<(), KeystoreError> {
        self.persist(identity, Some(agreement_public_key))
    }

    /// Record which sharing key this node now uses, and republish `node.pub` (ADR-0019).
    ///
    /// Unlike the agreement key, this one lives in the same daemon that holds the signing
    /// key, so nothing has to ask across the control plane to be vouched for. It is still
    /// recorded here rather than re-derived, because `node.pub` must say what the signing
    /// key stood behind, not what happens to be on disk when someone reads it.
    pub fn bind_sharing(
        &self,
        identity: &SigningIdentity,
        sharing_public_key: &[u8; 32],
    ) -> Result<(), KeystoreError> {
        let agreement = self.bound_agreement_public_key()?;
        self.persist_all(identity, agreement.as_ref(), Some(sharing_public_key))
    }

    /// Replace the signing key with a fresh one, endorsed by the outgoing key.
    ///
    /// Neither binding survives: the new key has vouched for nothing yet, so `otwono-netd`
    /// must re-bind the agreement key before the node can handshake again, and `otwono-idd`
    /// must re-bind the sharing key before anyone can seal to it. Saying that out loud is
    /// better than carrying a binding the new key never made.
    ///
    /// What happens to content keys already wrapped to the old sharing key is **OQ-27** and
    /// is not answered here. The secret itself is untouched, so nothing already shared to
    /// this node becomes unreadable — only the published vouching goes stale.
    pub fn rotate(&self, now_unix_ms: u64) -> Result<(SigningIdentity, SuccessionRecord), KeystoreError> {
        let previous = self.load()?;
        let new = SigningIdentity::generate().map_err(KeystoreError::Identity)?;

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

        self.persist_all(&new, None, None)?;
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

/// Where `otwono-idd` keeps the sharing key.
///
/// Deliberately its own type rather than a second [`AgreementKeystore`] pointed at a
/// different filename: the two keys must not be interchangeable, and a keystore that could
/// load either into either would make that a runtime question instead of a compile-time one
/// (ADR-0019).
pub struct SharingKeystore {
    dir: PathBuf,
}

impl SharingKeystore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        SharingKeystore {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn key_path(&self) -> PathBuf {
        self.dir.join(SHARING_KEY_FILE)
    }

    pub fn exists(&self) -> bool {
        self.key_path().exists()
    }

    /// Load the sharing key, or generate and persist one if there is none.
    ///
    /// A node that has never shared anything still gets one at first boot, because the key
    /// is what makes it *addressable* as a recipient — somebody else has to be able to seal
    /// to it before it knows it wants them to.
    pub fn load_or_generate(&self) -> Result<(SharingKey, bool), KeystoreError> {
        if self.exists() {
            Ok((self.load()?, false))
        } else {
            let key = SharingKey::generate().map_err(KeystoreError::Identity)?;
            self.persist(&key)?;
            Ok((key, true))
        }
    }

    pub fn load(&self) -> Result<SharingKey, KeystoreError> {
        let path = self.key_path();
        let text = read_private(&path)?;
        let stored: StoredSharingKey = serde_json::from_str(&text)
            .map_err(|e| KeystoreError::Malformed(format!("{}: {e}", path.display())))?;
        if stored.algorithm != "x25519" {
            return Err(KeystoreError::Malformed(format!(
                "unsupported algorithm {:?}; this build understands x25519",
                stored.algorithm
            )));
        }
        Ok(SharingKey::from_seed(
            &decode_seed(&stored.sharing_seed)?,
            stored.created_at_unix_ms,
        ))
    }

    pub fn persist(&self, key: &SharingKey) -> Result<(), KeystoreError> {
        ensure_dir(&self.dir)?;
        let stored = StoredSharingKey {
            schema_version: SCHEMA_VERSION.to_string(),
            algorithm: "x25519".to_string(),
            sharing_seed: base64_encode(key.secret_bytes().as_ref()),
            created_at_unix_ms: key.created_at_unix_ms(),
            hardware_backed: false,
        };
        let body = Zeroizing::new(
            serde_json::to_string_pretty(&stored).map_err(|e| KeystoreError::Malformed(e.to_string()))?,
        );
        write_private(&self.key_path(), &body)
    }
}

/// The X25519 half of the keystore. Only `otwono-netd` opens this.
pub struct AgreementKeystore {
    dir: PathBuf,
}

impl AgreementKeystore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        AgreementKeystore {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn key_path(&self) -> PathBuf {
        self.dir.join(AGREEMENT_KEY_FILE)
    }

    pub fn exists(&self) -> bool {
        self.key_path().exists()
    }

    pub fn load_or_generate(&self) -> Result<(AgreementKey, bool), KeystoreError> {
        if self.exists() {
            Ok((self.load()?, false))
        } else {
            let key = AgreementKey::generate().map_err(KeystoreError::Identity)?;
            self.persist(&key)?;
            Ok((key, true))
        }
    }

    pub fn load(&self) -> Result<AgreementKey, KeystoreError> {
        let path = self.key_path();
        let text = read_private(&path)?;
        let stored: StoredAgreementKey = serde_json::from_str(&text)
            .map_err(|e| KeystoreError::Malformed(format!("{}: {e}", path.display())))?;
        if stored.algorithm != "x25519" {
            return Err(KeystoreError::Malformed(format!(
                "unsupported algorithm {:?}; this build understands x25519",
                stored.algorithm
            )));
        }
        Ok(AgreementKey::from_seed(
            &decode_seed(&stored.agreement_seed)?,
            stored.created_at_unix_ms,
        ))
    }

    pub fn persist(&self, key: &AgreementKey) -> Result<(), KeystoreError> {
        ensure_dir(&self.dir)?;
        let stored = StoredAgreementKey {
            schema_version: SCHEMA_VERSION.to_string(),
            algorithm: "x25519".to_string(),
            agreement_seed: base64_encode(key.secret_bytes().as_ref()),
            created_at_unix_ms: key.created_at_unix_ms(),
            hardware_backed: false,
        };
        let body = Zeroizing::new(
            serde_json::to_string_pretty(&stored).map_err(|e| KeystoreError::Malformed(e.to_string()))?,
        );
        write_private(&self.key_path(), &body)
    }
}

/// Split a pre-split `node.key` that still holds both seeds.
///
/// Returns `Ok(true)` when it did something. Idempotent: a keystore that is already split
/// is left alone, and so is one that has no `node.key` at all.
///
/// The agreement seed is *preserved* rather than regenerated, so an upgraded node keeps
/// the agreement key its `node.pub` already advertises. Refusing to overwrite an existing
/// `agreement.key` matters: `otwono-netd` may already have generated one, and clobbering
/// it would invalidate the binding under a live daemon.
pub fn migrate_combined(dir: impl AsRef<Path>) -> Result<bool, KeystoreError> {
    let signing_store = SigningKeystore::new(&dir);
    if !signing_store.exists() {
        return Ok(false);
    }
    let stored = signing_store.load_stored()?;
    let Some(legacy_seed) = stored.agreement_seed.as_deref() else {
        return Ok(false);
    };

    let agreement_store = AgreementKeystore::new(&dir);
    let agreement = AgreementKey::from_seed(&decode_seed(legacy_seed)?, stored.created_at_unix_ms);
    if !agreement_store.exists() {
        agreement_store.persist(&agreement)?;
    }

    // Rewrite node.key without the agreement seed, recording the public half instead.
    let signing = SigningIdentity::from_seed(&decode_seed(&stored.signing_seed)?, stored.created_at_unix_ms);
    let bound = agreement_store.load()?.public();
    signing_store.persist(&signing, Some(&bound))?;
    Ok(true)
}

/// Write a combined identity as a pre-split `node.key`. Test support for the migration
/// path — nothing in a shipped binary writes this layout any more.
#[doc(hidden)]
pub fn write_combined_for_test(dir: impl AsRef<Path>, identity: &NodeIdentity) -> Result<(), KeystoreError> {
    let dir = dir.as_ref();
    ensure_dir(dir)?;
    let body = serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "algorithm": "ed25519",
        "signing_seed": base64_encode(identity.signing().seed().as_ref()),
        "agreement_seed": base64_encode(identity.agreement().secret_bytes().as_ref()),
        "created_at_unix_ms": identity.created_at_unix_ms(),
        "hardware_backed": false,
    });
    write_private(
        &dir.join(SIGNING_KEY_FILE),
        &serde_json::to_string_pretty(&body).map_err(|e| KeystoreError::Malformed(e.to_string()))?,
    )
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

    /// A fully provisioned keystore: signing key, agreement key, and the binding between.
    fn provisioned(dir: &Path) -> (SigningIdentity, AgreementKey) {
        let signing_store = SigningKeystore::new(dir);
        let agreement_store = AgreementKeystore::new(dir);
        let (signing, _) = signing_store.load_or_generate().unwrap();
        let (agreement, _) = agreement_store.load_or_generate().unwrap();
        signing_store
            .bind_agreement(&signing, &agreement.public())
            .unwrap();
        (signing, agreement)
    }

    /// A node fully provisioned under ADR-0019: all three keys.
    fn provisioned_with_sharing(dir: &Path) -> (SigningIdentity, AgreementKey, SharingKey) {
        let (signing, agreement) = provisioned(dir);
        let (sharing, _) = SharingKeystore::new(dir).load_or_generate().unwrap();
        (signing, agreement, sharing)
    }

    use crate::PublicIdentity;

    /// Read `node.pub` back as a peer would.
    fn published(dir: &Path) -> PublicIdentity {
        let raw = std::fs::read_to_string(SigningKeystore::new(dir).public_path()).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn a_bound_sharing_key_is_published_and_verifies() {
        let dir = tmpdir("sharing-publish");
        let (signing, _, sharing) = provisioned_with_sharing(&dir);
        SigningKeystore::new(&dir)
            .bind_sharing(&signing, &sharing.public())
            .unwrap();

        let public = published(&dir);
        assert_eq!(
            public.verified_sharing_key().unwrap(),
            Some(sharing.public()),
            "a peer must be able to get from this node's name to a key it may seal to"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_node_that_has_not_bound_a_sharing_key_publishes_no_binding() {
        // Not an error, and not a guess: a node nobody can share with yet.
        let dir = tmpdir("sharing-unbound");
        provisioned(&dir);
        let public = published(&dir);
        assert!(public.sharing_binding.is_none());
        assert_eq!(public.verified_sharing_key().unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rebinding_the_agreement_key_does_not_un_vouch_for_the_sharing_key() {
        // otwono-netd re-binds on every boot. If that dropped the sharing binding, a node
        // would stop being shareable-with after its first restart, and would say so only
        // by peers silently failing to seal to it.
        let dir = tmpdir("sharing-rebind");
        let store = SigningKeystore::new(&dir);
        let (signing, _, sharing) = provisioned_with_sharing(&dir);
        store.bind_sharing(&signing, &sharing.public()).unwrap();

        let fresh = AgreementKey::generate().unwrap();
        store.bind_agreement(&signing, &fresh.public()).unwrap();

        assert_eq!(store.bound_sharing_public_key().unwrap(), Some(sharing.public()));
        let public = published(&dir);
        assert_eq!(public.verified_sharing_key().unwrap(), Some(sharing.public()));
        assert_eq!(public.agreement_public_key, base64_encode(&fresh.public()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_drops_both_bindings_and_the_stale_published_file() {
        // A node.pub naming a NodeID this node no longer has would answer "who are you?"
        // with a dead name. No file is the honest state until something re-binds.
        let dir = tmpdir("sharing-rotate");
        let store = SigningKeystore::new(&dir);
        let (signing, _, sharing) = provisioned_with_sharing(&dir);
        store.bind_sharing(&signing, &sharing.public()).unwrap();
        assert!(store.public_path().exists());

        let (new, _) = store.rotate(1_700_000_000_000).unwrap();
        assert_ne!(new.node_id(), signing.node_id());
        assert_eq!(store.bound_sharing_public_key().unwrap(), None);
        assert_eq!(store.bound_agreement_public_key().unwrap(), None);
        assert!(
            !store.public_path().exists(),
            "node.pub still names the rotated-away identity"
        );

        // The secret itself is untouched: nothing already sealed to this node is lost.
        assert_eq!(
            SharingKeystore::new(&dir).load().unwrap().public(),
            sharing.public()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_published_file_never_contains_a_sharing_secret() {
        let dir = tmpdir("sharing-pubsecret");
        let (signing, _, sharing) = provisioned_with_sharing(&dir);
        SigningKeystore::new(&dir)
            .bind_sharing(&signing, &sharing.public())
            .unwrap();
        let raw = std::fs::read_to_string(SigningKeystore::new(&dir).public_path()).unwrap();
        assert!(
            !raw.contains(&base64_encode(sharing.secret_bytes().as_ref())),
            "{raw}"
        );
        assert!(raw.contains(&base64_encode(&sharing.public())), "{raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sharing_key_survives_a_reload() {
        let dir = tmpdir("sharing-reload");
        let store = SharingKeystore::new(&dir);
        let (key, generated) = store.load_or_generate().unwrap();
        assert!(generated, "the first call must generate");
        assert!(store.exists());

        let (again, generated) = store.load_or_generate().unwrap();
        assert!(!generated, "the second call must load, not regenerate");
        assert_eq!(
            key.public(),
            again.public(),
            "a node that regenerated this key would silently stop being able to open \
             anything already sealed to it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_was_sealed_before_a_restart_still_opens_after_one() {
        // Public-key equality is not the property that matters; being able to *open* is.
        // This goes through the real seal path so a corrupted secret half would show up.
        let dir = tmpdir("sharing-open");
        let (public, recipient) = {
            let (key, _) = SharingKeystore::new(&dir).load_or_generate().unwrap();
            (key.public(), "otwono1recipient".to_string())
        };
        let content_key = [7u8; 32];
        let sealed = crate::seal_to(&recipient, &public, &content_key).unwrap();

        let reloaded = SharingKeystore::new(&dir).load().unwrap();
        let opened = reloaded.open(&sealed).unwrap();
        assert_eq!(opened.as_ref(), &content_key);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sharing_key_is_not_the_agreement_key() {
        // ADR-0019 adds a third key rather than reusing the second. If these ever coincide,
        // a peer that can negotiate a session can also open everything shared with the node.
        let dir = tmpdir("sharing-distinct");
        let (signing, agreement, sharing) = provisioned_with_sharing(&dir);
        assert_ne!(agreement.public(), sharing.public());
        assert_ne!(agreement.secret_bytes().as_ref(), sharing.secret_bytes().as_ref());
        assert_ne!(signing.seed().as_ref(), sharing.secret_bytes().as_ref());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_key_file_contains_another_key_file_s_secret() {
        // The split only means anything if each file is useless to whoever holds the others.
        let dir = tmpdir("sharing-split");
        let (signing, agreement, sharing) = provisioned_with_sharing(&dir);
        let secrets = [
            ("signing", base64_encode(signing.seed().as_ref())),
            ("agreement", base64_encode(agreement.secret_bytes().as_ref())),
            ("sharing", base64_encode(sharing.secret_bytes().as_ref())),
        ];
        let files = [
            ("signing", SigningKeystore::new(&dir).key_path()),
            ("agreement", AgreementKeystore::new(&dir).key_path()),
            ("sharing", SharingKeystore::new(&dir).key_path()),
            ("public", SigningKeystore::new(&dir).public_path()),
        ];
        for (file_name, path) in files {
            let raw = std::fs::read_to_string(&path).unwrap();
            for (secret_name, secret) in &secrets {
                let belongs = file_name == *secret_name;
                assert_eq!(
                    raw.contains(secret.as_str()),
                    belongs,
                    "{} in {}",
                    secret_name,
                    path.display()
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sharing_key_file_is_owner_only_and_refused_when_it_is_not() {
        let dir = tmpdir("sharing-mode");
        provisioned_with_sharing(&dir);
        let path = SharingKeystore::new(&dir).key_path();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = SharingKeystore::new(&dir).load().unwrap_err();
        assert!(matches!(err, KeystoreError::InsecurePermissions { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sharing_key_of_an_unknown_algorithm_is_refused_rather_than_guessed() {
        let dir = tmpdir("sharing-algo");
        provisioned_with_sharing(&dir);
        let path = SharingKeystore::new(&dir).key_path();
        let raw = std::fs::read_to_string(&path).unwrap();
        let swapped = raw.replace("\"x25519\"", "\"kyber768\"");
        assert_ne!(raw, swapped, "the algorithm field must be there to swap");
        write_private(&path, &swapped).unwrap();

        let err = SharingKeystore::new(&dir).load().unwrap_err();
        assert!(matches!(err, KeystoreError::Malformed(_)), "{err}");
        assert!(err.to_string().contains("kyber768"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_stored_sharing_key_does_not_claim_hardware_backing() {
        let dir = tmpdir("sharing-hw");
        provisioned_with_sharing(&dir);
        let raw = std::fs::read_to_string(SharingKeystore::new(&dir).key_path()).unwrap();
        let stored: StoredSharingKey = serde_json::from_str(&raw).unwrap();
        assert!(!stored.hardware_backed, "there is no TPM sealing yet");
        assert_eq!(stored.schema_version, SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_run_generates_and_persists() {
        let dir = tmpdir("gen");
        let store = SigningKeystore::new(&dir);
        let (identity, generated) = store.load_or_generate().unwrap();
        assert!(generated, "the first call must generate");
        assert!(store.exists());

        let (again, generated) = store.load_or_generate().unwrap();
        assert!(!generated, "the second call must load, not regenerate");
        assert_eq!(
            identity.node_id(),
            again.node_id(),
            "identity must survive a reload"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_signing_key_file_never_contains_an_agreement_secret() {
        // The whole point of the split. If this ever holds a seed again, otwono-idd is
        // storing a key it has no use for and otwono-netd's isolation is decorative.
        let dir = tmpdir("nosecret");
        let (_, agreement) = provisioned(&dir);
        let raw = std::fs::read_to_string(SigningKeystore::new(&dir).key_path()).unwrap();
        let stored: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(stored.get("agreement_seed").is_none(), "{raw}");
        assert!(
            !raw.contains(&base64_encode(agreement.secret_bytes().as_ref())),
            "{raw}"
        );
        // The public half is there, because idd must know what it vouched for.
        assert_eq!(
            stored["agreement_public_key"].as_str().unwrap(),
            base64_encode(&agreement.public())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_agreement_key_file_never_contains_the_signing_seed() {
        let dir = tmpdir("nosigning");
        let (signing, _) = provisioned(&dir);
        let raw = std::fs::read_to_string(AgreementKeystore::new(&dir).key_path()).unwrap();
        assert!(!raw.contains(&base64_encode(signing.seed().as_ref())), "{raw}");
        let stored: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(stored.get("signing_seed").is_none(), "{raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn both_key_files_are_owner_only_and_the_public_file_is_not() {
        let dir = tmpdir("modes");
        provisioned(&dir);
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(SigningKeystore::new(&dir).key_path()), 0o600);
        assert_eq!(mode(AgreementKeystore::new(&dir).key_path()), 0o600);
        assert_eq!(mode(SigningKeystore::new(&dir).public_path()), 0o644);
        assert_eq!(mode(dir.clone()), 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_world_readable_key_is_refused_rather_than_used() {
        let dir = tmpdir("insecure");
        provisioned(&dir);
        for path in [
            SigningKeystore::new(&dir).key_path(),
            AgreementKeystore::new(&dir).key_path(),
        ] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let err = SigningKeystore::new(&dir).load().unwrap_err();
        assert!(matches!(err, KeystoreError::InsecurePermissions { .. }), "{err}");
        assert!(err.to_string().contains("compromised"), "{err}");
        let err = AgreementKeystore::new(&dir).load().unwrap_err();
        assert!(matches!(err, KeystoreError::InsecurePermissions { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn neither_stored_key_claims_hardware_backing() {
        let dir = tmpdir("hw");
        provisioned(&dir);
        for path in [
            SigningKeystore::new(&dir).key_path(),
            AgreementKeystore::new(&dir).key_path(),
        ] {
            let stored: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(stored["hardware_backed"], false, "TPM sealing is not implemented");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_published_public_file_is_self_consistent() {
        let dir = tmpdir("pub");
        let (signing, agreement) = provisioned(&dir);
        let public: crate::PublicIdentity =
            serde_json::from_str(&std::fs::read_to_string(SigningKeystore::new(&dir).public_path()).unwrap())
                .unwrap();
        assert!(public.is_self_consistent());
        assert_eq!(public.node_id, *signing.node_id());
        assert_eq!(public.agreement_public_key, base64_encode(&agreement.public()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_published_before_an_agreement_key_is_bound() {
        // A node.pub without an agreement key would either lie or be unusable. Not
        // writing one is the honest state for a node that cannot yet handshake.
        let dir = tmpdir("unbound");
        let store = SigningKeystore::new(&dir);
        store.load_or_generate().unwrap();
        assert!(!store.public_path().exists());
        assert_eq!(store.bound_agreement_public_key().unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn binding_records_the_agreement_key_and_survives_a_reload() {
        let dir = tmpdir("bind");
        let (_, agreement) = provisioned(&dir);
        assert_eq!(
            SigningKeystore::new(&dir).bound_agreement_public_key().unwrap(),
            Some(agreement.public())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_agreement_key_survives_a_reload() {
        let dir = tmpdir("agreload");
        let store = AgreementKeystore::new(&dir);
        let (first, generated) = store.load_or_generate().unwrap();
        assert!(generated);
        let (again, generated) = store.load_or_generate().unwrap();
        assert!(!generated, "a restart must not mint a new agreement key");
        assert_eq!(first.public(), again.public());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_produces_a_record_the_old_key_endorses() {
        let dir = tmpdir("rotate");
        let store = SigningKeystore::new(&dir);
        let (old, _) = provisioned(&dir);
        let old_public = old.public_key_bytes();
        let old_id = *old.node_id();

        let (new, record) = store.rotate(1_700_000_000_000).unwrap();
        assert_ne!(new.node_id(), &old_id, "rotation must produce a new identity");
        assert_eq!(record.previous_node_id, old_id);
        assert_eq!(record.new_node_id, *new.node_id());
        record
            .verify(&old_public)
            .expect("the outgoing key must endorse the new one");

        assert_eq!(store.load().unwrap().node_id(), new.node_id());
        assert_eq!(store.succession_records().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_drops_the_binding_the_old_key_made() {
        // The new key has vouched for nothing. Carrying the old binding forward would
        // present a signature the current key never made.
        let dir = tmpdir("rotbind");
        let store = SigningKeystore::new(&dir);
        provisioned(&dir);
        assert!(store.bound_agreement_public_key().unwrap().is_some());
        store.rotate(1_700_000_000_000).unwrap();
        assert_eq!(store.bound_agreement_public_key().unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_succession_record_signed_by_the_wrong_key_is_rejected() {
        // Otherwise anyone could declare themselves the successor to any node.
        let dir = tmpdir("forge");
        let store = SigningKeystore::new(&dir);
        store.load_or_generate().unwrap();
        let (_, record) = store.rotate(1_700_000_000_000).unwrap();
        let impostor = SigningIdentity::generate().unwrap();
        assert!(record.verify(&impostor.public_key_bytes()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tampered_successor_key_is_rejected() {
        let dir = tmpdir("swap");
        let store = SigningKeystore::new(&dir);
        let (old, _) = store.load_or_generate().unwrap();
        let old_public = old.public_key_bytes();
        let (_, mut record) = store.rotate(1_700_000_000_000).unwrap();
        let attacker = SigningIdentity::generate().unwrap();
        record.new_public_key = base64_encode(&attacker.public_key_bytes());
        assert_eq!(record.verify(&old_public), Err(IdentityError::NodeIdMismatch));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn successive_rotations_append_rather_than_replace() {
        let dir = tmpdir("chain");
        let store = SigningKeystore::new(&dir);
        store.load_or_generate().unwrap();
        store.rotate(1).unwrap();
        store.rotate(2).unwrap();
        let records = store.succession_records().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0].new_node_id, records[1].previous_node_id,
            "the chain must link"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_key_file_is_an_error_not_a_new_identity() {
        // Silently regenerating would change the node's name and orphan every peer
        // relationship it had.
        let dir = tmpdir("corrupt");
        let store = SigningKeystore::new(&dir);
        ensure_dir(&dir).unwrap();
        write_private(&store.key_path(), "{not json").unwrap();
        assert!(matches!(store.load(), Err(KeystoreError::Malformed(_))));
        assert!(matches!(
            store.load_or_generate(),
            Err(KeystoreError::Malformed(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_splits_a_combined_keystore_without_changing_the_node() {
        // An upgraded node must keep both its name and the agreement key its published
        // node.pub already advertises, or every peer that cached it is wrong.
        let dir = tmpdir("migrate");
        let combined = NodeIdentity::generate().unwrap();
        write_combined_for_test(&dir, &combined).unwrap();

        assert!(migrate_combined(&dir).unwrap(), "the first run must migrate");

        let signing_store = SigningKeystore::new(&dir);
        assert_eq!(signing_store.load().unwrap().node_id(), combined.node_id());
        assert_eq!(
            AgreementKeystore::new(&dir).load().unwrap().public(),
            combined.agreement_public().to_bytes()
        );
        assert_eq!(
            signing_store.bound_agreement_public_key().unwrap(),
            Some(combined.agreement_public().to_bytes())
        );

        // The seed is gone from node.key.
        let raw = std::fs::read_to_string(signing_store.key_path()).unwrap();
        assert!(
            !raw.contains(&base64_encode(combined.agreement().secret_bytes().as_ref())),
            "{raw}"
        );

        assert!(!migrate_combined(&dir).unwrap(), "migration must be idempotent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_does_not_clobber_an_agreement_key_that_already_exists() {
        // otwono-netd may have generated one already. Overwriting it would invalidate the
        // binding under a running daemon.
        let dir = tmpdir("noclobber");
        let combined = NodeIdentity::generate().unwrap();
        write_combined_for_test(&dir, &combined).unwrap();
        let existing = AgreementKey::generate().unwrap();
        AgreementKeystore::new(&dir).persist(&existing).unwrap();

        migrate_combined(&dir).unwrap();

        assert_eq!(
            AgreementKeystore::new(&dir).load().unwrap().public(),
            existing.public(),
            "the live agreement key must survive"
        );
        assert_eq!(
            SigningKeystore::new(&dir).bound_agreement_public_key().unwrap(),
            Some(existing.public()),
            "the binding must name the key that actually exists"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_is_a_no_op_on_an_empty_or_already_split_keystore() {
        let dir = tmpdir("nomigrate");
        assert!(!migrate_combined(&dir).unwrap(), "nothing to migrate");
        provisioned(&dir);
        assert!(!migrate_combined(&dir).unwrap(), "already split");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

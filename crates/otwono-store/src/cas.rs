//! The chunk store on disk.
//!
//! # Every read is verified
//!
//! `get` re-hashes what it read and refuses to return bytes whose digest does not match the
//! name they were asked for. That is not paranoia about our own writes — it is what makes a
//! chunk from a peer, from a damaged disk, or from a half-finished write indistinguishable
//! from absent rather than indistinguishable from correct. The cluster cache
//! (ADR-0015) rests on exactly this: a source does not have to be trusted, because the name
//! is the hash.
//!
//! # Writes are atomic
//!
//! Stage beside the destination and rename, the same discipline `ai.models.install` and the
//! fetch spool use. An interrupted write leaves a stray `.incoming-*` file and never a
//! short chunk under a name claiming to be complete — which matters because a
//! short chunk that survived would be a permanent, silent corruption of a
//! content-addressed name.

use crate::chunk::ChunkRef;
use crate::crypt::{CryptError, StorageKey};
use crate::object::{digest_from_hex, ContentId, Object};
use std::path::{Path, PathBuf};

/// Where an image keeps the content store.
pub const DEFAULT_STORE_DIR: &str = "/var/lib/otwono/store";

pub struct Store {
    root: PathBuf,
    /// Absent means chunks are written in the clear.
    ///
    /// Only tests and inspection tools have any business without a key; a daemon always
    /// supplies one, and `otwono-stored` refuses to start if it cannot get one.
    key: Option<StorageKey>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("root", &self.root)
            .field("encrypted", &self.key.is_some())
            .finish()
    }
}

#[derive(Debug)]
pub enum StoreError {
    Io {
        path: PathBuf,
        reason: String,
    },
    /// The bytes on disk are not the bytes this name promises.
    Corrupt {
        name: String,
        actual: String,
    },
    NotFound(String),
    Object(crate::object::ObjectError),
    Crypt(CryptError),
    /// Widening a label is `label.promote`, which always confirms. The store will not do
    /// it as a side effect of a relabel call.
    Promotion {
        from: crate::Visibility,
        to: crate::Visibility,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io { path, reason } => write!(f, "{}: {reason}", path.display()),
            StoreError::Corrupt { name, actual } => write!(
                f,
                "the chunk stored as {name} hashes to {actual}; it has been altered or damaged"
            ),
            StoreError::NotFound(n) => write!(f, "{n} is not in the store"),
            StoreError::Object(e) => write!(f, "{e}"),
            StoreError::Crypt(e) => write!(f, "{e}"),
            StoreError::Promotion { from, to } => write!(
                f,
                "{from} to {to} makes this object more visible, which is label.promote and \
                 needs a person; this call only makes things more restrictive"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

fn io(path: &Path, e: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        reason: e.to_string(),
    }
}

/// A sharing envelope the caller asked for something impossible with.
///
/// Spelled out at ten call sites before this existed, which made the long ones wrap in a
/// way that read as if the message mattered less than the type path around it.
fn bad_envelope(reason: impl Into<String>) -> StoreError {
    StoreError::Object(crate::object::ObjectError::BadEnvelope(reason.into()))
}

impl Store {
    /// A store that writes chunks in the clear.
    ///
    /// For tests and for offline inspection. A daemon uses [`Store::encrypted`]; a node
    /// with an unencrypted store is a node whose disk is its threat model.
    pub fn new(root: impl AsRef<Path>) -> Store {
        Store {
            root: root.as_ref().to_path_buf(),
            key: None,
        }
    }

    /// A store that seals every chunk at rest, whatever its label.
    ///
    /// Uniform rather than per-label, because a chunk can be referenced by a `Private`
    /// object and a `Public` one at once — see `crypt`'s module docs.
    pub fn encrypted(root: impl AsRef<Path>, key: StorageKey) -> Store {
        Store {
            root: root.as_ref().to_path_buf(),
            key: Some(key),
        }
    }

    pub fn is_encrypted(&self) -> bool {
        self.key.is_some()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn chunks_dir(&self) -> PathBuf {
        self.root.join("chunks")
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.root.join("objects")
    }

    /// Two levels of 256, so a store with millions of chunks never puts them all in one
    /// directory — which is slow on ext4 and worse on the SD cards these boards boot from.
    pub fn chunk_path(&self, hex: &str) -> PathBuf {
        self.chunks_dir().join(&hex[0..2]).join(&hex[2..4]).join(hex)
    }

    pub fn object_path(&self, id: &ContentId) -> PathBuf {
        let hex = id.to_hex();
        self.objects_dir().join(&hex[0..2]).join(format!("{hex}.json"))
    }

    pub fn ensure_layout(&self) -> Result<(), StoreError> {
        for d in [self.chunks_dir(), self.objects_dir()] {
            std::fs::create_dir_all(&d).map_err(|e| io(&d, e))?;
        }
        Ok(())
    }

    pub fn has_chunk(&self, r: &ChunkRef) -> bool {
        self.chunk_path(&r.hex()).exists()
    }

    /// Store one chunk. Idempotent: a chunk already present is left alone, which is what
    /// makes dedup free rather than a separate step.
    pub fn put_chunk(&self, bytes: &[u8]) -> Result<ChunkRef, StoreError> {
        let r = ChunkRef::of(bytes);
        let destination = self.chunk_path(&r.hex());
        if destination.exists() {
            return Ok(r);
        }
        let parent = destination.parent().expect("chunk paths have parents");
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;

        // Unique staging name: two threads storing the same chunk must not write over each
        // other's partial file and then rename a mixture into place.
        let staging = parent.join(format!(
            ".incoming-{}-{:?}-{}",
            std::process::id(),
            std::thread::current().id(),
            r.hex()
        ));
        let on_disk = match &self.key {
            Some(k) => k.seal(bytes, &r.digest),
            None => bytes.to_vec(),
        };
        std::fs::write(&staging, &on_disk).map_err(|e| io(&staging, e))?;
        match std::fs::rename(&staging, &destination) {
            Ok(()) => Ok(r),
            Err(e) => {
                let _ = std::fs::remove_file(&staging);
                Err(io(&destination, e))
            }
        }
    }

    /// Read one chunk, verifying it.
    ///
    /// A digest mismatch is reported as corruption rather than returned to the caller. The
    /// caller asked for particular bytes by name; anything else is not a smaller answer, it
    /// is a wrong one.
    pub fn get_chunk(&self, r: &ChunkRef) -> Result<Vec<u8>, StoreError> {
        let hex = r.hex();
        let path = self.chunk_path(&hex);
        let raw = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(StoreError::NotFound(hex)),
            Err(e) => return Err(io(&path, e)),
        };
        // Decrypt first, then verify the plaintext digest. Both checks stay: the AEAD
        // catches a swapped or damaged file, and the digest catches a store written in the
        // clear and then edited. Neither subsumes the other.
        let bytes = match &self.key {
            Some(k) => k.open(&raw, &r.digest).map_err(StoreError::Crypt)?,
            None => raw,
        };
        let actual = ChunkRef::of(&bytes);
        if actual.digest != r.digest {
            return Err(StoreError::Corrupt {
                name: hex,
                actual: actual.hex(),
            });
        }
        Ok(bytes)
    }

    /// Record an object. The record is validated before it is written, so the store never
    /// contains a record that contradicts itself.
    pub fn put_object(&self, object: &Object) -> Result<(), StoreError> {
        object.validate().map_err(StoreError::Object)?;
        let path = self.object_path(&object.content_id);
        let parent = path.parent().expect("object paths have parents");
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        let text = serde_json::to_string_pretty(object)
            .map_err(|e| io(&path, std::io::Error::other(e.to_string())))?;
        let staging = parent.join(format!(
            ".incoming-{}-{}.json",
            std::process::id(),
            object.content_id
        ));
        std::fs::write(&staging, text).map_err(|e| io(&staging, e))?;
        std::fs::rename(&staging, &path).map_err(|e| io(&path, e))
    }

    pub fn get_object(&self, id: &ContentId) -> Result<Object, StoreError> {
        let path = self.object_path(id);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(id.to_hex()))
            }
            Err(e) => return Err(io(&path, e)),
        };
        let object: Object =
            serde_json::from_str(&text).map_err(|e| io(&path, std::io::Error::other(e.to_string())))?;
        object.validate().map_err(StoreError::Object)?;
        if object.content_id != *id {
            return Err(StoreError::Corrupt {
                name: id.to_hex(),
                actual: object.content_id.to_hex(),
            });
        }
        Ok(object)
    }

    /// Are all of an object's chunks present?
    ///
    /// A file-exists check, not a hash: this answers "can I serve this" cheaply, and
    /// `get_chunk` does the verifying when the bytes are actually wanted.
    pub fn is_complete(&self, object: &Object) -> bool {
        object.chunk_refs().iter().all(|r| self.has_chunk(r))
    }

    /// Reassemble an object, verifying every chunk on the way through.
    pub fn read_object(&self, object: &Object) -> Result<Vec<u8>, StoreError> {
        let mut out = Vec::with_capacity(object.size_bytes as usize);
        for c in &object.chunks {
            let digest = digest_from_hex(&c.blake3).ok_or(StoreError::NotFound(c.blake3.clone()))?;
            out.extend_from_slice(&self.get_chunk(&ChunkRef {
                digest,
                length: c.length,
            })?);
        }
        Ok(out)
    }

    /// Change an object's label.
    ///
    /// **Demotion is always allowed; promotion is not this function's decision.** Making
    /// something more restrictive can be done by anyone at any time and needs no
    /// permission. Making it *less* restrictive is `label.promote`, which always confirms
    /// (CLAUDE.md §8), so this refuses it and the daemon routes it through the broker.
    ///
    /// The content id does not change: the bytes are the same bytes. That is the point of
    /// keeping the label out of the identity.
    ///
    /// What this cannot do, and the UI must say so: demotion stops *future* serving. It
    /// does not recall what peers already hold.
    pub fn demote(&self, id: &ContentId, to: crate::Visibility) -> Result<Object, StoreError> {
        let mut object = self.get_object(id)?;
        if !to.is_at_least_as_restrictive_as(object.visibility) {
            return Err(StoreError::Promotion {
                from: object.visibility,
                to,
            });
        }
        // Relabelling cannot turn plaintext into a shared object: its bytes would have to
        // be sealed and its chunks are not. Saying so here rather than letting put_object
        // reject the record is the difference between an explanation and an arithmetic
        // complaint (ADR-0019 §1).
        if to == crate::Visibility::Shared && object.sharing.is_none() {
            return Err(StoreError::Object(
                crate::object::ObjectError::SharedWithoutAnEnvelope,
            ));
        }
        object.visibility = to;
        self.put_object(&object)?;
        Ok(object)
    }

    /// Store bytes derived from other objects, inheriting their labels.
    ///
    /// The rule from CLAUDE.md §8 and `DATA-VISIBILITY.md`: derived content carries the
    /// **most restrictive** label among its inputs and any label the caller asked for. A
    /// summary of a private note is private; a thumbnail of a shared photo is shared.
    ///
    /// Getting this backwards would let derivation launder a label, which is the most
    /// likely way a system like this leaks without anyone deciding to. So the caller's
    /// requested label is a *ceiling*, never a floor: asking for `Public` over a `Private`
    /// input yields `Private`, silently and correctly.
    ///
    /// An input that is not in the store is an error rather than an ignored term. A missing
    /// input would otherwise make the derived label looser than it should be, which is the
    /// failure that must not be quiet.
    pub fn put_derived(
        &self,
        data: &[u8],
        requested: crate::Visibility,
        inputs: &[ContentId],
    ) -> Result<Object, StoreError> {
        let mut labels = vec![requested];
        for id in inputs {
            labels.push(self.get_object(id)?.visibility);
        }
        self.put_bytes(data, crate::Visibility::most_restrictive(labels))
    }

    /// Chunk and store an object from a reader, never holding more than one chunk.
    ///
    /// The streaming counterpart of [`Store::put_derived`], and the reason `store.import`
    /// exists: an object that arrives as a file may be far larger than this process's
    /// memory, and reading it into a `Vec` to chunk it would defeat the point.
    ///
    /// The label is resolved **first**, from the inputs, so a missing input fails before any
    /// bytes are written rather than after — the same rule as `put_derived`, applied in the
    /// order that costs less when it is broken.
    pub fn put_reader<R: std::io::Read>(
        &self,
        reader: R,
        requested: crate::Visibility,
        inputs: &[ContentId],
    ) -> Result<Object, StoreError> {
        let mut labels = vec![requested];
        for id in inputs {
            labels.push(self.get_object(id)?.visibility);
        }
        let visibility = crate::Visibility::most_restrictive(labels);
        let refs = self.chunk_from_reader(reader)?;
        let object = Object::new(&refs, visibility);
        self.put_object(&object)?;
        Ok(object)
    }

    /// Chunk a reader into the store, returning the chunk list and writing no record.
    ///
    /// Separate from [`put_reader`](Self::put_reader) because the `SHARED` path cannot
    /// write its record until the envelope exists, and writing a record without one would
    /// mean a valid-looking `Shared` object briefly existing that nobody could open.
    fn chunk_from_reader<R: std::io::Read>(&self, reader: R) -> Result<Vec<ChunkRef>, StoreError> {
        self.ensure_layout()?;
        let mut failure = None;
        let refs = crate::chunk::stream(reader, |_, bytes| {
            match self.put_chunk(bytes) {
                Ok(_) => Ok(()),
                Err(e) => {
                    // Carried out rather than flattened into a ChunkError, so a store
                    // failure is still reported as a store failure and not as bad input.
                    failure = Some(e);
                    Err(crate::chunk::ChunkError::Io(std::io::Error::other(
                        "the store could not write a chunk",
                    )))
                }
            }
        });
        if let Some(e) = failure {
            return Err(e);
        }
        refs.map_err(|e| StoreError::Io {
            path: self.chunks_dir(),
            reason: e.to_string(),
        })
    }

    /// Store an object encrypted to a set of recipients (ADR-0019).
    ///
    /// The plaintext is sealed first and the *ciphertext* is chunked, so this object's
    /// chunk digests and its [`ContentId`] are over bytes nobody without a key can
    /// interpret. It therefore does not deduplicate against the same file stored any other
    /// way, and sharing one file with two recipient sets produces two unrelated objects.
    /// Both follow from the encryption meaning anything.
    ///
    /// The ciphertext goes through a temporary file rather than memory. Sealing produces a
    /// `Write` and chunking consumes a `Read`, and the object may be larger than this
    /// process — which is the whole reason `store.import` and ADR-0018 exist. One extra
    /// pass over a disk the store is already on is the cheap way to bridge that; a lazy
    /// sealing reader would mean a second copy of the frame logic, which is where this
    /// kind of code goes wrong.
    ///
    /// An empty recipient list is refused. An object nobody can open is not a shared
    /// object, and the person who asked for it would find out much later.
    pub fn put_shared_reader<R: std::io::Read>(
        &self,
        reader: R,
        recipients: &[Recipient],
    ) -> Result<(Object, crate::shared::ContentKey), StoreError> {
        if recipients.is_empty() {
            return Err(bad_envelope("no recipients, so nobody could open it"));
        }
        self.ensure_layout()?;
        let key = crate::shared::ContentKey::generate();
        let prefix = crate::shared::nonce_prefix();

        let staging = self.chunks_dir().join(format!(
            ".sealing-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let plaintext_size_bytes = {
            let file = std::fs::File::create(&staging).map_err(|e| io(&staging, e))?;
            let mut writer = std::io::BufWriter::new(file);
            crate::shared::seal(&key, &prefix, reader, &mut writer).map_err(|e| StoreError::Io {
                path: staging.clone(),
                reason: e.to_string(),
            })?
        };

        let sealed_keys = recipients
            .iter()
            .map(|r| otwono_identity::seal_to(&r.node_id, &r.sharing_public_key, key.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| bad_envelope(e.to_string()))?;

        let result = (|| {
            let file = std::fs::File::open(&staging).map_err(|e| io(&staging, e))?;
            let refs = self.chunk_from_reader(std::io::BufReader::new(file))?;
            let object = Object::new(&refs, crate::Visibility::Shared);
            let object = object.with_sharing(crate::object::Sharing {
                encryption: crate::shared::SHARED_ENCRYPTION.to_string(),
                nonce_prefix: data_encoding::BASE64.encode(&prefix),
                plaintext_size_bytes,
                sealed_keys,
            });
            self.put_object(&object)?;
            Ok(object)
        })();

        // The staging file is ciphertext, but it is a whole copy of the object and there is
        // no reason for it to outlive this call — including when the call failed.
        let _ = std::fs::remove_file(&staging);
        result.map(|object| (object, key))
    }

    /// Seal an existing object's content key to further recipients (ADR-0019 §5).
    ///
    /// The id does not change. The chunks are the same ciphertext under the same content
    /// key; all that grows is the list of people holding a copy of that key. Keeping the id
    /// is the point — a recipient added later can be told the same name everybody else has.
    ///
    /// The caller supplies the content key, which it can only have by being a recipient
    /// itself. That is the access control: **you can only widen access to something you can
    /// already open.** The store does not check it against the ciphertext, because it cannot
    /// without decrypting — a caller that supplies the wrong key seals a useless copy and
    /// the recipient finds out when it fails to open, which is a caller error and not a
    /// disclosure.
    ///
    /// A recipient already on the list is refused rather than re-sealed. Two copies for one
    /// name is what `Sharing::validate` rejects, and quietly replacing the first would
    /// discard a key somebody may already be relying on.
    pub fn add_recipients(
        &self,
        id: &ContentId,
        content_key: &crate::shared::ContentKey,
        recipients: &[Recipient],
    ) -> Result<Object, StoreError> {
        if recipients.is_empty() {
            return Err(bad_envelope("no recipients to add"));
        }
        let mut object = self.get_object(id)?;
        let sharing = object.sharing.as_mut().ok_or(bad_envelope(
            "this object is not sealed, so there is nobody to add",
        ))?;

        for r in recipients {
            if sharing.names(&r.node_id) {
                return Err(bad_envelope(format!("{} is already a recipient", r.node_id)));
            }
        }
        for r in recipients {
            let sealed = otwono_identity::seal_to(&r.node_id, &r.sharing_public_key, content_key.as_bytes())
                .map_err(|e| bad_envelope(e.to_string()))?;
            sharing.sealed_keys.push(sealed);
        }
        self.put_object(&object)?;
        Ok(object)
    }

    /// Delete named recipients' copies of the content key (ADR-0019 §5).
    ///
    /// **This does not un-share anything.** It deletes their wrapped copy and nothing else:
    /// they may already hold the ciphertext, and they certainly still hold their own key. A
    /// recipient who fetched yesterday keeps what they fetched. What this stops is *future*
    /// serving and future discovery, exactly as `demote` stops future serving — and the
    /// caller is told so, because a UI that implies otherwise is lying.
    ///
    /// Genuinely revoking access means re-encrypting under a new content key and re-sharing,
    /// which is a different and more expensive operation and is not this one.
    ///
    /// Returns the names actually removed. A name that was not on the list is not an error:
    /// the caller asked for it to be absent and it is absent.
    ///
    /// Removing the **last** recipient is refused. An object nobody can open is not a shared
    /// object, which is the same rule that refuses creating one — and here it would silently
    /// destroy the owner's own access to their own file.
    pub fn remove_recipients(
        &self,
        id: &ContentId,
        node_ids: &[String],
    ) -> Result<(Object, Vec<String>), StoreError> {
        let mut object = self.get_object(id)?;
        let sharing = object.sharing.as_mut().ok_or(bad_envelope(
            "this object is not sealed, so there is nobody to remove",
        ))?;

        let removed: Vec<String> = node_ids.iter().filter(|n| sharing.names(n)).cloned().collect();
        if removed.is_empty() {
            return Ok((object, removed));
        }
        if removed.len() == sharing.sealed_keys.len() {
            return Err(bad_envelope(
                "removing every recipient would leave an object nobody can open, including \
                 whoever shared it; re-encrypt under a new key instead"
                    .to_string(),
            ));
        }
        sharing.sealed_keys.retain(|k| !removed.contains(&k.recipient));
        self.put_object(&object)?;
        Ok((object, removed))
    }

    /// The objects this node is willing to have copied, by content id (ADR-0026 §7).
    ///
    /// Every `REPLICATED` object, with the policy a holder needs to decide. Unlike
    /// [`Store::shared_with`] the answer does not depend on who is asking: `REPLICATED`
    /// means copying is permitted, full stop, so there is nothing to scope and nothing for
    /// a scoping bug to leak.
    ///
    /// **`target_replicas` is deliberately not returned.** A holder cannot count replicas
    /// (ADR-0026 §3), so it could not act on the number, and sending a figure nobody can
    /// use invites somebody to build a UI on it.
    ///
    /// Ordered by content id and paged like the sharing index, for the same reasons:
    /// deterministic, stable across calls, and needing no state this store does not have.
    ///
    /// A damaged record is skipped rather than raised. One bad file must not stop a peer
    /// learning what else is on offer.
    pub fn replicable(
        &self,
        after: Option<&ContentId>,
        limit: usize,
    ) -> Result<Vec<(ContentId, crate::object::Replication, u64)>, StoreError> {
        let mut found = Vec::new();
        let shards = match std::fs::read_dir(self.objects_dir()) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
            Err(e) => return Err(io(&self.objects_dir(), e)),
        };
        for shard in shards.flatten() {
            let entries = match std::fs::read_dir(shard.path()) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let Ok(text) = std::fs::read_to_string(entry.path()) else {
                    continue;
                };
                let Ok(object) = serde_json::from_str::<Object>(&text) else {
                    continue;
                };
                let Some(policy) = object.replication_policy() else {
                    continue;
                };
                if let Some(after) = after {
                    if object.content_id.to_hex() <= after.to_hex() {
                        continue;
                    }
                }
                found.push((object.content_id, policy, object.size_bytes));
            }
        }
        found.sort_by_key(|(id, _, _)| id.to_hex());
        found.truncate(limit);
        Ok(found)
    }

    /// The objects in this store sealed to `recipient`, by content id (ADR-0020).
    ///
    /// Answers the one question a recipient cannot answer for itself: a `SHARED` object's id
    /// is over ciphertext keyed by a fresh per-object key, so unlike a `PUBLIC` object it
    /// cannot be derived from the content. Without this, sharing needs a channel outside the
    /// mesh to carry the id.
    ///
    /// **Scoped by computation, not by filtering.** Nothing is assembled and then narrowed;
    /// an object is only ever added if its own envelope names this recipient. A global list
    /// with a filter over it would put the whole store one bug away from being published.
    ///
    /// Ordered by content id — deterministic, stable across calls, and needing no state this
    /// store does not already have. Deliberately not by time: sharing time is metadata the
    /// record does not carry and a recipient has not asked for.
    ///
    /// `after` continues a previous page. `limit` bounds the reply; the scan itself is over
    /// every object either way, which is why ADR-0020 has the caller do it once per session.
    ///
    /// A damaged or unreadable record is skipped, not raised: one bad file must not stop a
    /// recipient discovering everything else that is theirs.
    pub fn shared_with(
        &self,
        recipient: &str,
        after: Option<&ContentId>,
        limit: usize,
    ) -> Result<Vec<SharedEntry>, StoreError> {
        let mut found = Vec::new();
        let shards = match std::fs::read_dir(self.objects_dir()) {
            Ok(d) => d,
            // No objects directory is an empty store, not a failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
            Err(e) => return Err(io(&self.objects_dir(), e)),
        };
        for shard in shards.flatten() {
            let entries = match std::fs::read_dir(shard.path()) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(object) = serde_json::from_str::<Object>(&text) else {
                    continue;
                };
                if object.visibility != crate::Visibility::Shared {
                    continue;
                }
                let Some(sharing) = &object.sharing else {
                    continue;
                };
                if !sharing.names(recipient) {
                    continue;
                }
                // Checked last, and against the record rather than the filename: a record
                // whose chunks do not hash to its own id is damage, and advertising it would
                // send a recipient after bytes that will not verify.
                if object.validate().is_err() {
                    continue;
                }
                found.push(SharedEntry {
                    content_id: object.content_id,
                    plaintext_size_bytes: sharing.plaintext_size_bytes,
                });
            }
        }
        found.sort_by_key(|e| e.content_id);
        if let Some(after) = after {
            // Strictly after, so a caller paging with the last id it saw does not see it
            // twice.
            found.retain(|e| e.content_id > *after);
        }
        found.truncate(limit);
        Ok(found)
    }

    /// Store a sealed object this node received, keeping the key it was given.
    ///
    /// The counterpart of [`put_shared_reader`](Self::put_shared_reader) on the receiving
    /// side. The bytes are already ciphertext and must not be sealed again: re-sealing would
    /// produce a different object under a key the sender never issued, and the recipient
    /// would end up holding something its sender could not recognise.
    ///
    /// Only the copy of the content key that was given travels into the record, so a
    /// recipient's own store names one recipient — itself. It does not learn, and cannot
    /// re-serve to, anybody else on the original list.
    ///
    /// `expected` is the id the caller asked a peer for. Chunking is deterministic, so
    /// storing the same ciphertext must reproduce it; a mismatch means the bytes are not the
    /// object that was asked for and the record is not written.
    pub fn accept_shared<R: std::io::Read>(
        &self,
        ciphertext: R,
        expected: &ContentId,
        sharing: crate::object::Sharing,
    ) -> Result<Object, StoreError> {
        sharing.validate().map_err(StoreError::Object)?;
        let refs = self.chunk_from_reader(ciphertext)?;
        let object = Object::new(&refs, crate::Visibility::Shared).with_sharing(sharing);
        if object.content_id != *expected {
            return Err(StoreError::Corrupt {
                name: expected.to_hex(),
                actual: object.content_id.to_hex(),
            });
        }
        self.put_object(&object)?;
        Ok(object)
    }

    /// Reassemble a sealed object and open it, a frame at a time.
    ///
    /// Verifies each chunk against its digest on the way through, as every read does, and
    /// then authenticates each frame. A truncated or reordered object fails rather than
    /// yielding a shorter one.
    pub fn open_shared<W: std::io::Write>(
        &self,
        object: &Object,
        key: &crate::shared::ContentKey,
        out: W,
    ) -> Result<u64, StoreError> {
        let sharing = object
            .sharing
            .as_ref()
            .ok_or(bad_envelope("this object is not sealed"))?;
        sharing.validate().map_err(StoreError::Object)?;
        let prefix = data_encoding::BASE64
            .decode(sharing.nonce_prefix.as_bytes())
            .ok()
            .and_then(|b| crate::shared::decode_prefix(&b).ok())
            .ok_or(bad_envelope("the nonce prefix is not usable"))?;

        let mut failure = None;
        let reader = ChunkReader::new(self, object, &mut failure);
        let written = crate::shared::open(key, &prefix, reader, out);
        // A store failure surfaced through the reader arrives as an io::Error inside
        // SharedError; carrying it out separately keeps "a chunk is missing" from being
        // reported as "this did not decrypt", which are very different things to a user.
        if let Some(e) = failure {
            return Err(e);
        }
        written.map_err(|e| StoreError::Io {
            path: self.chunks_dir(),
            reason: e.to_string(),
        })
    }

    /// Chunk and store bytes, returning the record.
    pub fn put_bytes(&self, data: &[u8], visibility: crate::Visibility) -> Result<Object, StoreError> {
        self.ensure_layout()?;
        let mut refs = Vec::new();
        for c in crate::chunk::slice(data) {
            let start: usize = refs.iter().map(|r: &ChunkRef| r.length as usize).sum::<usize>();
            let stored = self.put_chunk(&data[start..start + c.length as usize])?;
            refs.push(stored);
        }
        let object = Object::new(&refs, visibility);
        self.put_object(&object)?;
        Ok(object)
    }
}

/// One object a recipient may ask for, in the reply to "what have you sealed to me?".
///
/// The plaintext size travels with it because it is what a recipient needs to decide whether
/// to fetch now — the object's own `size_bytes` measures ciphertext, which is larger by a
/// tag per frame and is not the number a person recognises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedEntry {
    pub content_id: ContentId,
    pub plaintext_size_bytes: u64,
}

/// Who a `SHARED` object is sealed for.
///
/// The key is the caller's responsibility to have *verified* — it comes from a peer's
/// signed `SharingBinding`, not from an unsigned field. Taking bytes here rather than a
/// binding keeps the store out of the business of checking signatures, which belongs to the
/// identity crate and to the daemon that fetched the binding.
#[derive(Debug, Clone)]
pub struct Recipient {
    /// NodeID in text form, exactly as it will appear in the sealed key.
    pub node_id: String,
    pub sharing_public_key: [u8; 32],
}

/// Reads an object's chunks in order, as one stream.
///
/// Exists so a sealed object can be opened a frame at a time instead of being reassembled
/// in memory first — the same reason `put_shared_reader` streams.
struct ChunkReader<'a> {
    store: &'a Store,
    chunks: std::vec::IntoIter<ChunkRef>,
    current: Vec<u8>,
    offset: usize,
    failure: &'a mut Option<StoreError>,
}

impl<'a> ChunkReader<'a> {
    fn new(store: &'a Store, object: &Object, failure: &'a mut Option<StoreError>) -> ChunkReader<'a> {
        ChunkReader {
            store,
            chunks: object.chunk_refs().into_iter(),
            current: Vec::new(),
            offset: 0,
            failure,
        }
    }
}

impl std::io::Read for ChunkReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.offset == self.current.len() {
            let Some(next) = self.chunks.next() else {
                return Ok(0);
            };
            match self.store.get_chunk(&next) {
                Ok(bytes) => {
                    self.current = bytes;
                    self.offset = 0;
                }
                Err(e) => {
                    // Reported as an io::Error so the caller's loop unwinds, and carried
                    // out through `failure` so it can be told apart from a decryption
                    // failure by whoever asked.
                    let message = e.to_string();
                    *self.failure = Some(e);
                    return Err(std::io::Error::other(message));
                }
            }
        }
        let n = (self.current.len() - self.offset).min(buf.len());
        buf[..n].copy_from_slice(&self.current[self.offset..self.offset + n]);
        self.offset += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {

    // --- replicable offers (ADR-0026 §7) -------------------------------------------------

    #[test]
    fn only_replicated_content_is_offered_for_copying() {
        // The whole filter. PUBLIC serves on request but is never offered as a copy, and
        // PRIVATE must not appear in an answer that crosses a link at all.
        let d = tmp("replicable");
        let s = Store::new(&d);
        let mut want = Vec::new();
        for (i, v) in [
            Visibility::Private,
            Visibility::Public,
            Visibility::Replicated,
            Visibility::Replicated,
        ]
        .into_iter()
        .enumerate()
        {
            let o = s.put_bytes(&data(2_000 + i, 40 + i as u64), v).unwrap();
            if v == Visibility::Replicated {
                want.push(o.content_id.to_hex());
            }
        }
        let got: Vec<String> = s
            .replicable(None, 100)
            .unwrap()
            .into_iter()
            .map(|(id, _, _)| id.to_hex())
            .collect();
        want.sort();
        assert_eq!(got, want);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_offer_carries_the_policy_and_the_size_a_holder_needs() {
        let d = tmp("replicable-policy");
        let s = Store::new(&d);
        let bytes = data(5_000, 71);
        let o = s.put_bytes(&bytes, Visibility::Replicated).unwrap();
        let (id, policy, size) = s.replicable(None, 10).unwrap().into_iter().next().unwrap();
        assert_eq!(id, o.content_id);
        assert_eq!(size, bytes.len() as u64);
        // No explicit policy was attached, so the default travels -- otherwise REPLICATED
        // would silently mean "not replicable" whenever somebody omitted the block.
        assert_eq!(policy, crate::object::Replication::default());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn offers_are_ordered_by_content_id_and_page_without_repeating() {
        // Pagination has to be stable or a holder walking the list sees an object twice and
        // misses another. Same requirement as ADR-0020's index, same reason.
        let d = tmp("replicable-page");
        let s = Store::new(&d);
        for i in 0..7 {
            s.put_bytes(&data(1_000 + i, 90 + i as u64), Visibility::Replicated)
                .unwrap();
        }
        let all: Vec<String> = s
            .replicable(None, 100)
            .unwrap()
            .into_iter()
            .map(|(id, _, _)| id.to_hex())
            .collect();
        assert_eq!(all.len(), 7);
        let mut sorted = all.clone();
        sorted.sort();
        assert_eq!(all, sorted, "not ordered by content id");

        let mut walked = Vec::new();
        let mut cursor: Option<ContentId> = None;
        loop {
            let page = s.replicable(cursor.as_ref(), 3).unwrap();
            if page.is_empty() {
                break;
            }
            cursor = Some(page.last().unwrap().0);
            walked.extend(page.into_iter().map(|(id, _, _)| id.to_hex()));
        }
        assert_eq!(
            walked, all,
            "paging did not reproduce the whole list exactly once"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_empty_store_offers_nothing_rather_than_failing() {
        let d = tmp("replicable-empty");
        let s = Store::new(&d);
        assert!(s.replicable(None, 10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
    use super::*;
    use crate::Visibility;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "otwono-store-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn data(len: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        // Mixed, not `seed | 1`: that maps 2 and 3 (and every other adjacent pair) to the
        // same stream, so two "different" fixtures come out byte-identical. Cost one
        // debugging round in the cache's LRU test.
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        while out.len() < len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn bytes_go_in_and_come_back_identical() {
        let d = tmp("roundtrip");
        let s = Store::new(&d);
        let payload = data(1 << 20, 3);
        let object = s.put_bytes(&payload, Visibility::Public).expect("put");
        assert_eq!(object.size_bytes, payload.len() as u64);
        assert!(s.is_complete(&object));
        assert_eq!(s.read_object(&object).expect("read"), payload);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn storing_the_same_bytes_twice_stores_them_once() {
        // Dedup falls out of content addressing rather than being a separate pass.
        let d = tmp("dedup");
        let s = Store::new(&d);
        let payload = data(1 << 20, 5);
        let a = s.put_bytes(&payload, Visibility::Public).expect("first");
        let count = |root: &Path| -> usize { walk(root).len() };
        let after_first = count(&s.chunks_dir());
        let b = s.put_bytes(&payload, Visibility::Public).expect("second");
        assert_eq!(a.content_id, b.content_id);
        assert_eq!(count(&s.chunks_dir()), after_first, "no chunk was duplicated");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_files_sharing_a_region_share_its_chunks() {
        // The reason to chunk at all: an edited file costs the edit, not the file.
        let d = tmp("share");
        let s = Store::new(&d);
        let base = data(2 << 20, 7);
        let mut edited = base[..4096].to_vec();
        edited.extend_from_slice(b"an insertion that shifts everything after it by 48 bytes ");
        edited.extend_from_slice(&base[4096..]);

        s.put_bytes(&base, Visibility::Public).expect("base");
        let before = walk(&s.chunks_dir()).len();
        s.put_bytes(&edited, Visibility::Public).expect("edited");
        let after = walk(&s.chunks_dir()).len();
        let added = after - before;
        assert!(
            added * 4 < before,
            "an insertion added {added} chunks on top of {before}; sharing is not working"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_tampered_chunk_is_reported_rather_than_returned() {
        // The property the cluster cache depends on: a source does not have to be
        // trusted, because the name is the hash.
        let d = tmp("tamper");
        let s = Store::new(&d);
        let payload = data(100_000, 11);
        let object = s.put_bytes(&payload, Visibility::Public).expect("put");

        let first = &object.chunks[0];
        std::fs::write(s.chunk_path(&first.blake3), b"not the bytes you asked for").expect("tamper");

        let err = s
            .read_object(&object)
            .expect_err("a swapped chunk must not be returned");
        assert!(matches!(err, StoreError::Corrupt { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_chunk_is_not_found_rather_than_a_short_read() {
        let d = tmp("missing");
        let s = Store::new(&d);
        let payload = data(200_000, 13);
        let object = s.put_bytes(&payload, Visibility::Public).expect("put");
        std::fs::remove_file(s.chunk_path(&object.chunks[0].blake3)).expect("remove");

        assert!(!s.is_complete(&object));
        assert!(matches!(
            s.read_object(&object).expect_err("must fail"),
            StoreError::NotFound(_)
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A recipient with a real sharing key, and the key itself so a test can open.
    fn recipient(name: &str, seed: u8) -> (Recipient, otwono_identity::SharingKey) {
        let key = otwono_identity::SharingKey::from_seed(&[seed; 32], 1_700_000_000_000);
        (
            Recipient {
                node_id: name.to_string(),
                sharing_public_key: key.public(),
            },
            key,
        )
    }

    #[test]
    fn adding_a_recipient_keeps_the_objects_name_and_lets_them_open_it() {
        // The id must not change: a recipient added later has to be told the same name
        // everybody else already has, and the chunks are the same ciphertext regardless.
        let d = tmp("add-recip");
        let s = Store::new(&d);
        let plaintext = data(50_000, 120);
        let (alice, alice_key) = recipient("otw1alice", 40);
        let (bob, bob_key) = recipient("otw1bob", 41);

        let (object, content_key) = s
            .put_shared_reader(plaintext.as_slice(), std::slice::from_ref(&alice))
            .unwrap();
        assert!(bob_key
            .open(&object.sharing.as_ref().unwrap().sealed_keys[0])
            .is_err());

        let grown = s
            .add_recipients(&object.content_id, &content_key, &[bob])
            .unwrap();
        assert_eq!(grown.content_id, object.content_id, "adding must not rename it");
        assert_eq!(grown.chunks, object.chunks, "and must not touch the bytes");

        let sharing = grown.sharing.as_ref().unwrap();
        assert_eq!(sharing.authorized_nodes(), vec!["otw1alice", "otw1bob"]);
        for (name, key) in [("otw1alice", &alice_key), ("otw1bob", &bob_key)] {
            let copy = sharing.copy_for(name).unwrap();
            let opened = crate::shared::ContentKey::from_bytes(*key.open(copy).unwrap());
            let mut out = Vec::new();
            s.open_shared(&grown, &opened, &mut out).unwrap();
            assert_eq!(out, plaintext, "{name} could not open it");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn adding_somebody_who_is_already_a_recipient_is_refused() {
        // Quietly re-sealing would discard a key somebody may already be relying on, and two
        // copies under one name is what the envelope's own validation rejects.
        let d = tmp("add-dup");
        let s = Store::new(&d);
        let (alice, _) = recipient("otw1alice", 42);
        let (object, key) = s
            .put_shared_reader(data(1_000, 121).as_slice(), std::slice::from_ref(&alice))
            .unwrap();

        let err = s.add_recipients(&object.content_id, &key, &[alice]).unwrap_err();
        assert!(err.to_string().contains("already a recipient"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn removing_a_recipient_stops_future_serving_and_nothing_else() {
        // The honest half of ADR-0019 §5: their copy of the key is gone from this record,
        // and everything they already fetched is still theirs.
        let d = tmp("remove-recip");
        let s = Store::new(&d);
        let plaintext = data(30_000, 122);
        let (alice, alice_key) = recipient("otw1alice", 43);
        let (bob, bob_key) = recipient("otw1bob", 44);
        let (object, _) = s.put_shared_reader(plaintext.as_slice(), &[alice, bob]).unwrap();

        // Bob takes his copy before he is removed, as a real recipient would have.
        let bobs_copy = object
            .sharing
            .as_ref()
            .unwrap()
            .copy_for("otw1bob")
            .unwrap()
            .clone();

        let (after, removed) = s
            .remove_recipients(&object.content_id, &["otw1bob".to_string()])
            .unwrap();
        assert_eq!(removed, vec!["otw1bob".to_string()]);
        assert_eq!(after.content_id, object.content_id, "removing must not rename it");
        let sharing = after.sharing.as_ref().unwrap();
        assert_eq!(sharing.authorized_nodes(), vec!["otw1alice"]);
        assert!(sharing.copy_for("otw1bob").is_none());

        // Alice is untouched.
        let hers = sharing.copy_for("otw1alice").unwrap();
        let key = crate::shared::ContentKey::from_bytes(*alice_key.open(hers).unwrap());
        let mut out = Vec::new();
        s.open_shared(&after, &key, &mut out).unwrap();
        assert_eq!(out, plaintext);

        // And the point the API has to be honest about: the copy Bob already took still
        // opens the bytes he already has. Removal recalls nothing.
        let still = crate::shared::ContentKey::from_bytes(*bob_key.open(&bobs_copy).unwrap());
        let mut theirs = Vec::new();
        s.open_shared(&after, &still, &mut theirs).unwrap();
        assert_eq!(
            theirs, plaintext,
            "removal cannot un-share what was already taken"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn removing_the_last_recipient_is_refused() {
        // It would leave an object nobody can open -- including whoever shared it, who since
        // ADR-0019 §5a is always on the list. The same rule that refuses creating one.
        let d = tmp("remove-last");
        let s = Store::new(&d);
        let (alice, _) = recipient("otw1alice", 45);
        let (object, _) = s
            .put_shared_reader(data(1_000, 123).as_slice(), &[alice])
            .unwrap();

        let err = s
            .remove_recipients(&object.content_id, &["otw1alice".to_string()])
            .unwrap_err();
        assert!(err.to_string().contains("nobody can open"), "{err}");
        // And it really did not write the change.
        let reread = s.get_object(&object.content_id).unwrap();
        assert_eq!(
            reread.sharing.as_ref().unwrap().authorized_nodes(),
            vec!["otw1alice"]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn removing_somebody_who_was_never_a_recipient_is_not_an_error() {
        // The caller asked for them to be absent and they are absent. Reporting what was
        // actually removed is how a UI can say "nothing changed" without guessing.
        let d = tmp("remove-absent");
        let s = Store::new(&d);
        let (alice, _) = recipient("otw1alice", 46);
        let (object, _) = s
            .put_shared_reader(data(1_000, 124).as_slice(), &[alice])
            .unwrap();

        let (after, removed) = s
            .remove_recipients(&object.content_id, &["otw1nobody".to_string()])
            .unwrap();
        assert!(removed.is_empty());
        assert_eq!(
            after.sharing.as_ref().unwrap().authorized_nodes(),
            vec!["otw1alice"]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_removed_recipient_stops_being_offered_the_object() {
        // Removal has to reach ADR-0020's index too, or the node would go on advertising
        // something the serve path then refuses.
        let d = tmp("remove-index");
        let s = Store::new(&d);
        let (alice, _) = recipient("otw1alice", 47);
        let (bob, _) = recipient("otw1bob", 48);
        let (object, _) = s
            .put_shared_reader(data(1_000, 125).as_slice(), &[alice, bob])
            .unwrap();
        assert_eq!(s.shared_with("otw1bob", None, 10).unwrap().len(), 1);

        s.remove_recipients(&object.content_id, &["otw1bob".to_string()])
            .unwrap();
        assert_eq!(s.shared_with("otw1bob", None, 10).unwrap(), vec![]);
        assert_eq!(s.shared_with("otw1alice", None, 10).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_recipient_is_told_only_what_was_sealed_to_it() {
        // The whole point of ADR-0020, and the property that has to hold before anything
        // else about it matters.
        let d = tmp("shared-index");
        let s = Store::new(&d);
        let (alice, _) = recipient("otw1alice", 20);
        let (bob, _) = recipient("otw1bob", 21);

        let (for_alice, _) = s
            .put_shared_reader(data(1_000, 80).as_slice(), std::slice::from_ref(&alice))
            .unwrap();
        let (for_bob, _) = s
            .put_shared_reader(data(1_000, 81).as_slice(), std::slice::from_ref(&bob))
            .unwrap();
        let (for_both, _) = s
            .put_shared_reader(data(1_000, 82).as_slice(), &[alice.clone(), bob.clone()])
            .unwrap();
        // Two objects that are not shared at all, which must never appear.
        let public = s.put_bytes(&data(1_000, 83), Visibility::Public).unwrap();
        let private = s.put_bytes(&data(1_000, 84), Visibility::Private).unwrap();

        let hers: Vec<ContentId> = s
            .shared_with("otw1alice", None, 100)
            .unwrap()
            .into_iter()
            .map(|e| e.content_id)
            .collect();
        assert!(hers.contains(&for_alice.content_id));
        assert!(hers.contains(&for_both.content_id));
        assert!(!hers.contains(&for_bob.content_id), "alice was told about bob's");
        assert!(!hers.contains(&public.content_id));
        assert!(!hers.contains(&private.content_id));
        assert_eq!(hers.len(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_stranger_is_told_nothing_and_it_looks_like_an_empty_store() {
        // "Nothing for you" and "nothing for anybody" must be the same answer, or asking
        // becomes a way to find out whether a node shares with people at all.
        let d = tmp("shared-index-stranger");
        let s = Store::new(&d);
        let (alice, _) = recipient("otw1alice", 22);
        s.put_shared_reader(data(1_000, 85).as_slice(), &[alice]).unwrap();

        assert_eq!(s.shared_with("otw1stranger", None, 100).unwrap(), vec![]);
        let empty = Store::new(tmp("shared-index-empty"));
        assert_eq!(empty.shared_with("otw1stranger", None, 100).unwrap(), vec![]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_index_pages_without_repeating_or_skipping() {
        // Ordered by content id, strictly after the cursor. Getting the boundary wrong
        // either loops forever or silently loses an object a recipient was told about.
        let d = tmp("shared-index-page");
        let s = Store::new(&d);
        let (alice, _) = recipient("otw1alice", 23);
        let mut all = Vec::new();
        for seed in 0..7u64 {
            let (o, _) = s
                .put_shared_reader(data(1_000, 90 + seed).as_slice(), std::slice::from_ref(&alice))
                .unwrap();
            all.push(o.content_id);
        }
        all.sort();

        let mut paged = Vec::new();
        let mut cursor = None;
        loop {
            let page = s.shared_with("otw1alice", cursor.as_ref(), 3).unwrap();
            if page.is_empty() {
                break;
            }
            cursor = Some(page.last().unwrap().content_id);
            paged.extend(page.into_iter().map(|e| e.content_id));
        }
        assert_eq!(paged, all, "paging must visit every object exactly once");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_index_reports_the_plaintext_size_not_the_ciphertext_size() {
        // What a recipient needs to decide whether to fetch. size_bytes is the ciphertext,
        // which is larger by a tag per frame and is not the number a person recognises.
        let d = tmp("shared-index-size");
        let s = Store::new(&d);
        let (alice, _) = recipient("otw1alice", 24);
        let plaintext = data(200_000, 95);
        let (object, _) = s.put_shared_reader(plaintext.as_slice(), &[alice]).unwrap();

        let entry = &s.shared_with("otw1alice", None, 10).unwrap()[0];
        assert_eq!(entry.plaintext_size_bytes, plaintext.len() as u64);
        assert!(object.size_bytes > entry.plaintext_size_bytes);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_damaged_record_is_skipped_rather_than_stopping_the_answer() {
        // One bad file must not stop a recipient discovering everything else that is theirs,
        // and a record whose chunks do not hash to its own id must not be advertised at all.
        let d = tmp("shared-index-damaged");
        let s = Store::new(&d);
        let (alice, _) = recipient("otw1alice", 25);
        let (good, _) = s
            .put_shared_reader(data(1_000, 96).as_slice(), std::slice::from_ref(&alice))
            .unwrap();
        let (doomed, _) = s
            .put_shared_reader(data(1_000, 97).as_slice(), std::slice::from_ref(&alice))
            .unwrap();

        // Not JSON at all.
        let (garbage, _) = s
            .put_shared_reader(data(1_000, 98).as_slice(), std::slice::from_ref(&alice))
            .unwrap();
        std::fs::write(s.object_path(&garbage.content_id), b"{ not json").unwrap();

        // Well-formed, but its size no longer matches its chunks.
        let mut lying = s.get_object(&doomed.content_id).unwrap();
        lying.size_bytes += 1;
        std::fs::write(
            s.object_path(&doomed.content_id),
            serde_json::to_string(&lying).unwrap(),
        )
        .unwrap();

        let ids: Vec<ContentId> = s
            .shared_with("otw1alice", None, 100)
            .unwrap()
            .into_iter()
            .map(|e| e.content_id)
            .collect();
        assert_eq!(ids, vec![good.content_id], "{ids:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_demoted_object_is_no_longer_offered_to_its_recipient() {
        // Demotion stops future serving, so it must stop future advertising too -- an index
        // naming something the serve path then refuses would send a recipient after bytes
        // it cannot have.
        let d = tmp("shared-index-demote");
        let s = Store::new(&d);
        let (alice, _) = recipient("otw1alice", 26);
        let (object, _) = s.put_shared_reader(data(1_000, 99).as_slice(), &[alice]).unwrap();
        assert_eq!(s.shared_with("otw1alice", None, 10).unwrap().len(), 1);

        s.demote(&object.content_id, Visibility::Private).unwrap();
        assert_eq!(s.shared_with("otw1alice", None, 10).unwrap(), vec![]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_shared_object_round_trips_through_a_recipients_key() {
        // The whole path: seal, chunk the ciphertext, store, then unwrap one recipient's
        // copy and open it back to the original bytes.
        let d = tmp("shared-round");
        let s = Store::new(&d);
        let plaintext = data(300_000, 71);
        let (alice, alice_key) = recipient("otw1alice", 1);

        let (object, _) = s
            .put_shared_reader(plaintext.as_slice(), &[alice])
            .expect("put_shared_reader");
        assert_eq!(object.visibility, Visibility::Shared);
        let sharing = object.sharing.as_ref().expect("a shared object has an envelope");
        assert_eq!(sharing.plaintext_size_bytes, plaintext.len() as u64);

        let sealed_copy = sharing.copy_for("otw1alice").expect("alice has a copy");
        let content_key = crate::shared::ContentKey::from_bytes(*alice_key.open(sealed_copy).unwrap());

        let mut opened = Vec::new();
        let written = s.open_shared(&object, &content_key, &mut opened).unwrap();
        assert_eq!(written, plaintext.len() as u64);
        assert_eq!(opened, plaintext);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn every_recipient_gets_their_own_copy_and_nobody_elses_opens() {
        let d = tmp("shared-many");
        let s = Store::new(&d);
        let plaintext = data(80_000, 72);
        let (alice, alice_key) = recipient("otw1alice", 2);
        let (bob, bob_key) = recipient("otw1bob", 3);
        let (_, stranger_key) = recipient("otw1stranger", 4);

        let (object, _) = s.put_shared_reader(plaintext.as_slice(), &[alice, bob]).unwrap();
        let sharing = object.sharing.as_ref().unwrap();
        assert_eq!(sharing.authorized_nodes(), vec!["otw1alice", "otw1bob"]);
        assert!(sharing.names("otw1bob"));
        assert!(!sharing.names("otw1stranger"));

        for key in [&alice_key, &bob_key] {
            let copy = sharing.copy_for(&recipient_name(key, sharing)).unwrap();
            let content_key = crate::shared::ContentKey::from_bytes(*key.open(copy).unwrap());
            let mut opened = Vec::new();
            s.open_shared(&object, &content_key, &mut opened).unwrap();
            assert_eq!(opened, plaintext);
        }

        // A stranger holds no copy at all, and neither copy opens with their key.
        assert!(sharing.copy_for("otw1stranger").is_none());
        for copy in &sharing.sealed_keys {
            assert!(stranger_key.open(copy).is_err(), "{copy:?}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Which of the envelope's copies belongs to this key, found by trying to open them.
    fn recipient_name(key: &otwono_identity::SharingKey, sharing: &crate::object::Sharing) -> String {
        sharing
            .sealed_keys
            .iter()
            .find(|c| key.open(c).is_ok())
            .expect("one of these must be theirs")
            .recipient
            .clone()
    }

    #[test]
    fn the_stored_chunks_are_ciphertext() {
        // If this ever fails, the encryption is decorative: the plaintext would be sitting
        // in the chunk files under a digest anyone can compute from a guess.
        let d = tmp("shared-cipher");
        let s = Store::new(&d);
        let plaintext = b"the quarterly figures".repeat(3000);
        let (alice, _) = recipient("otw1alice", 5);
        let (object, _) = s.put_shared_reader(plaintext.as_slice(), &[alice]).unwrap();

        let reassembled = s.read_object(&object).unwrap();
        assert!(!reassembled.windows(21).any(|w| w == b"the quarterly figures"));

        // And the plaintext's own object id is nowhere near this one.
        let plain = Store::new(tmp("shared-cipher-plain"));
        let plain_object = plain.put_bytes(&plaintext, Visibility::Public).unwrap();
        assert_ne!(plain_object.content_id, object.content_id);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sharing_one_file_twice_produces_two_unrelated_objects() {
        // The cost ADR-0019 names, demonstrated: no deduplication, and no way for a holder
        // to tell that two shares are the same document.
        let d = tmp("shared-nodedup");
        let s = Store::new(&d);
        let plaintext = data(60_000, 73);
        let (alice, _) = recipient("otw1alice", 6);
        let (bob, _) = recipient("otw1bob", 7);

        let (first, _) = s.put_shared_reader(plaintext.as_slice(), &[alice]).unwrap();
        let (second, _) = s.put_shared_reader(plaintext.as_slice(), &[bob]).unwrap();
        assert_ne!(first.content_id, second.content_id);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_shared_object_with_no_recipients_is_refused_at_the_call() {
        let d = tmp("shared-norecip");
        let s = Store::new(&d);
        let err = s
            .put_shared_reader(b"anything".as_slice(), &[])
            .expect_err("an object nobody can open is not a shared object");
        assert!(err.to_string().contains("nobody"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn relabelling_cannot_turn_plaintext_into_a_shared_object() {
        // Otherwise the label would claim a protection the bytes do not have.
        let d = tmp("shared-relabel");
        let s = Store::new(&d);
        let o = s.put_bytes(&data(9_000, 74), Visibility::Replicated).unwrap();
        let err = s
            .demote(&o.content_id, Visibility::Shared)
            .expect_err("plaintext must not become shared by relabelling");
        assert!(err.to_string().contains("stored again sealed"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn demoting_a_shared_object_keeps_its_envelope() {
        // Dropping the envelope on demotion would leave the owner unable to read their own
        // object -- a data-loss bug wearing the costume of a privacy improvement.
        let d = tmp("shared-demote");
        let s = Store::new(&d);
        let plaintext = data(40_000, 75);
        let (alice, alice_key) = recipient("otw1alice", 8);
        let (object, _) = s.put_shared_reader(plaintext.as_slice(), &[alice]).unwrap();

        let after = s.demote(&object.content_id, Visibility::Private).unwrap();
        assert_eq!(after.visibility, Visibility::Private);
        let sharing = after.sharing.as_ref().expect("the bytes are still sealed");
        let copy = sharing.copy_for("otw1alice").unwrap();
        let content_key = crate::shared::ContentKey::from_bytes(*alice_key.open(copy).unwrap());
        let mut opened = Vec::new();
        s.open_shared(&after, &content_key, &mut opened).unwrap();
        assert_eq!(opened, plaintext);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_chunk_is_reported_as_missing_not_as_a_decryption_failure() {
        // These are very different things to whoever is looking at the error: one is
        // damage, the other is the wrong key.
        let d = tmp("shared-gap");
        let s = Store::new(&d);
        let plaintext = data(400_000, 76);
        let (alice, alice_key) = recipient("otw1alice", 9);
        let (object, _) = s.put_shared_reader(plaintext.as_slice(), &[alice]).unwrap();
        let copy = object.sharing.as_ref().unwrap().copy_for("otw1alice").unwrap();
        let content_key = crate::shared::ContentKey::from_bytes(*alice_key.open(copy).unwrap());

        let victim = object.chunk_refs()[0];
        std::fs::remove_file(s.chunk_path(&victim.hex())).unwrap();

        let mut opened = Vec::new();
        let err = s.open_shared(&object, &content_key, &mut opened).unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_wrong_content_key_does_not_open_a_shared_object() {
        let d = tmp("shared-wrongkey");
        let s = Store::new(&d);
        let (alice, _) = recipient("otw1alice", 10);
        let (object, _) = s
            .put_shared_reader(data(50_000, 77).as_slice(), &[alice])
            .unwrap();

        let mut opened = Vec::new();
        let err = s
            .open_shared(&object, &crate::shared::ContentKey::generate(), &mut opened)
            .expect_err("a key that was never sealed must not open it");
        assert!(err.to_string().contains("did not authenticate"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_object_record_survives_a_round_trip_and_is_validated_on_the_way_out() {
        let d = tmp("record");
        let s = Store::new(&d);
        let object = s
            .put_bytes(&data(300_000, 17), Visibility::Replicated)
            .expect("put");
        let back = s.get_object(&object.content_id).expect("get");
        assert_eq!(back, object);
        assert_eq!(back.visibility, Visibility::Replicated);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_record_edited_on_disk_is_refused_on_read() {
        // Someone with write access changes the label. The record no longer validates
        // against its own id, so the store refuses it rather than serving a relabelled file.
        let d = tmp("edited");
        let s = Store::new(&d);
        let object = s.put_bytes(&data(50_000, 19), Visibility::Private).expect("put");

        let path = s.object_path(&object.content_id);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["chunks"].as_array_mut().unwrap().pop();
        std::fs::write(&path, value.to_string()).unwrap();

        assert!(s.get_object(&object.content_id).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_empty_object_is_stored_and_read_back() {
        let d = tmp("empty");
        let s = Store::new(&d);
        let object = s.put_bytes(b"", Visibility::Public).expect("put");
        assert_eq!(object.size_bytes, 0);
        assert!(s.is_complete(&object), "an object with no chunks is complete");
        assert!(s.read_object(&object).expect("read").is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn chunks_are_sharded_rather_than_piled_into_one_directory() {
        // A store with millions of chunks in one directory is slow on ext4 and worse on
        // the SD cards these boards boot from.
        let d = tmp("shard");
        let s = Store::new(&d);
        let object = s.put_bytes(&data(1 << 20, 23), Visibility::Public).expect("put");
        let hex = &object.chunks[0].blake3;
        let path = s.chunk_path(hex);
        assert!(path.ends_with(hex));
        assert_eq!(
            path.parent().unwrap().file_name().unwrap().to_str().unwrap(),
            &hex[2..4]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_interrupted_write_leaves_no_chunk_under_a_finished_name() {
        // Staging files must not be mistaken for chunks: a short chunk that survived would
        // be a permanent, silent corruption of a content-addressed name.
        let d = tmp("staging");
        let s = Store::new(&d);
        s.ensure_layout().expect("layout");
        let object = s.put_bytes(&data(100_000, 29), Visibility::Public).expect("put");
        for p in walk(&s.chunks_dir()) {
            let name = p.file_name().unwrap().to_str().unwrap().to_string();
            assert!(!name.starts_with(".incoming-"), "left a staging file: {name}");
            assert_eq!(name.len(), 64, "a chunk file is named by its digest: {name}");
        }
        assert!(s.is_complete(&object));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_encrypted_store_round_trips() {
        let d = tmp("enc-roundtrip");
        let s = Store::encrypted(&d, crate::StorageKey::generate());
        assert!(s.is_encrypted());
        let payload = data(1 << 20, 31);
        let object = s.put_bytes(&payload, Visibility::Private).expect("put");
        assert_eq!(s.read_object(&object).expect("read"), payload);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_encrypted_store_leaves_no_plaintext_on_disk() {
        // What encryption at rest is for, asserted against the actual files rather than
        // against the API. A distinctive marker, searched for across every chunk file.
        let d = tmp("enc-plaintext");
        let s = Store::encrypted(&d, crate::StorageKey::generate());
        let marker = b"CANARY-a7f3-this-must-not-appear-on-disk";
        let mut payload = Vec::new();
        while payload.len() < 300_000 {
            payload.extend_from_slice(marker);
        }
        s.put_bytes(&payload, Visibility::Private).expect("put");

        for p in walk(&s.chunks_dir()) {
            let raw = std::fs::read(&p).expect("read chunk file");
            assert!(
                !raw.windows(marker.len()).any(|w| w == marker),
                "plaintext found in {}",
                p.display()
            );
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn public_data_is_encrypted_at_rest_too() {
        // Deliberately stronger than DATA-VISIBILITY.md Section 5, and the only correct
        // version: a chunk can belong to a Private object and a Public one at once, so
        // keying encryption on the label would have to answer "which object first?".
        let d = tmp("enc-public");
        let s = Store::encrypted(&d, crate::StorageKey::generate());
        let marker = b"PUBLIC-CANARY-also-not-on-disk-in-the-clear";
        let mut payload = Vec::new();
        while payload.len() < 200_000 {
            payload.extend_from_slice(marker);
        }
        s.put_bytes(&payload, Visibility::Public).expect("put");
        for p in walk(&s.chunks_dir()) {
            let raw = std::fs::read(&p).expect("read");
            assert!(!raw.windows(marker.len()).any(|w| w == marker));
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_store_written_under_another_key_does_not_open() {
        // The stolen-disk property. The chunks are there and unreadable.
        let d = tmp("enc-wrongkey");
        let payload = data(200_000, 37);
        let object = Store::encrypted(&d, crate::StorageKey::generate())
            .put_bytes(&payload, Visibility::Private)
            .expect("put");

        let stranger = Store::encrypted(&d, crate::StorageKey::generate());
        assert!(stranger.is_complete(&object), "the chunks are present");
        assert!(
            matches!(stranger.read_object(&object), Err(StoreError::Crypt(_))),
            "another key must not read them"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn dedup_still_works_when_encrypted() {
        // Names are over plaintext, so identical content still collapses even though each
        // sealing uses a fresh nonce and produces different ciphertext.
        let d = tmp("enc-dedup");
        let s = Store::encrypted(&d, crate::StorageKey::generate());
        let payload = data(1 << 20, 41);
        let a = s.put_bytes(&payload, Visibility::Public).expect("first");
        let after_first = walk(&s.chunks_dir()).len();
        let b = s.put_bytes(&payload, Visibility::Public).expect("second");
        assert_eq!(a.content_id, b.content_id);
        assert_eq!(walk(&s.chunks_dir()).len(), after_first);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_content_id_does_not_depend_on_the_storage_key() {
        // Two nodes with different keys must agree on what a file is called, or the
        // cluster cache cannot exist.
        let d1 = tmp("enc-id1");
        let d2 = tmp("enc-id2");
        let payload = data(500_000, 43);
        let a = Store::encrypted(&d1, crate::StorageKey::generate())
            .put_bytes(&payload, Visibility::Public)
            .expect("a");
        let b = Store::encrypted(&d2, crate::StorageKey::generate())
            .put_bytes(&payload, Visibility::Public)
            .expect("b");
        assert_eq!(a.content_id, b.content_id);
        assert_eq!(a.chunks, b.chunks, "and on its chunk names");
        let _ = std::fs::remove_dir_all(&d1);
        let _ = std::fs::remove_dir_all(&d2);
    }

    #[test]
    fn a_damaged_encrypted_chunk_is_reported_not_returned() {
        let d = tmp("enc-damage");
        let s = Store::encrypted(&d, crate::StorageKey::generate());
        let object = s.put_bytes(&data(100_000, 47), Visibility::Public).expect("put");
        let path = s.chunk_path(&object.chunks[0].blake3);
        let mut raw = std::fs::read(&path).expect("read");
        let last = raw.len() - 1;
        raw[last] ^= 1;
        std::fs::write(&path, raw).expect("damage");
        assert!(matches!(s.read_object(&object), Err(StoreError::Crypt(_))));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn derived_content_inherits_the_most_restrictive_input() {
        // The DATA-VISIBILITY.md Section 6 property, through the store rather than the
        // arithmetic: a summary of a private note is private, whatever the caller asked for.
        let d = tmp("derive");
        let s = Store::new(&d);
        let private = s
            .put_bytes(b"a private note".repeat(100).as_slice(), Visibility::Private)
            .unwrap();
        let public = s
            .put_bytes(b"a public page".repeat(100).as_slice(), Visibility::Public)
            .unwrap();

        let summary = s
            .put_derived(
                b"a summary of both",
                Visibility::Public,
                &[private.content_id, public.content_id],
            )
            .expect("derive");
        assert_eq!(
            summary.visibility,
            Visibility::Private,
            "a public request over a private input must not launder the label"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_requested_label_is_a_ceiling_not_a_floor() {
        // Asking for something more restrictive than the inputs is honoured; asking for
        // something looser is not.
        let d = tmp("ceiling");
        let s = Store::new(&d);
        let public = s
            .put_bytes(b"public source".repeat(100).as_slice(), Visibility::Public)
            .unwrap();

        let tighter = s
            .put_derived(b"kept private", Visibility::Private, &[public.content_id])
            .expect("derive");
        assert_eq!(tighter.visibility, Visibility::Private);

        let looser = s
            .put_derived(b"still public", Visibility::Replicated, &[public.content_id])
            .expect("derive");
        assert_eq!(looser.visibility, Visibility::Public, "capped by the input");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn deriving_from_an_input_that_is_not_here_is_an_error() {
        // A missing input silently dropped would make the derived label looser than it
        // should be, which is exactly the failure that must not be quiet.
        let d = tmp("derive-missing");
        let s = Store::new(&d);
        s.ensure_layout().unwrap();
        let absent = crate::ContentId::of(&[]);
        assert!(matches!(
            s.put_derived(b"x", Visibility::Public, &[absent]),
            Err(StoreError::NotFound(_))
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn deriving_from_nothing_honours_the_request() {
        let d = tmp("derive-none");
        let s = Store::new(&d);
        let o = s
            .put_derived(b"original work", Visibility::Public, &[])
            .expect("derive");
        assert_eq!(o.visibility, Visibility::Public);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn demotion_is_allowed_and_stops_future_serving() {
        // The fourth Section 6 criterion. Demotion needs no permission; what it cannot do
        // is recall what peers already hold, which the UI has to say.
        let d = tmp("demote");
        let s = Store::new(&d);
        let o = s.put_bytes(&data(50_000, 53), Visibility::Public).unwrap();
        assert!(o.visibility.may_leave_the_node_unattended());

        let after = s.demote(&o.content_id, Visibility::Private).expect("demote");
        assert_eq!(after.visibility, Visibility::Private);
        assert!(!after.visibility.may_leave_the_node_unattended());

        // Re-read from disk: the change is durable, not just in the returned value.
        let reread = s.get_object(&o.content_id).expect("re-read");
        assert_eq!(reread.visibility, Visibility::Private);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn demotion_does_not_rename_the_object() {
        // The bytes are the same bytes. Keeping the label out of the identity is what makes
        // this true, and a rename would break every reference to it.
        let d = tmp("demote-id");
        let s = Store::new(&d);
        let o = s.put_bytes(&data(30_000, 59), Visibility::Replicated).unwrap();
        let after = s.demote(&o.content_id, Visibility::Private).expect("demote");
        assert_eq!(after.content_id, o.content_id);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn widening_a_label_is_refused_here_and_left_to_the_broker() {
        // label.promote always confirms. The store must not do it as a side effect.
        let d = tmp("promote");
        let s = Store::new(&d);
        let o = s.put_bytes(&data(20_000, 61), Visibility::Private).unwrap();
        for wider in [Visibility::Shared, Visibility::Public, Visibility::Replicated] {
            assert!(
                matches!(s.demote(&o.content_id, wider), Err(StoreError::Promotion { .. })),
                "{wider} is wider than private and must be refused"
            );
        }
        // Re-labelling to the same label is a no-op rather than an error.
        assert!(s.demote(&o.content_id, Visibility::Private).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Every regular file under `root`.
    fn walk(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.filter_map(Result::ok) {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p);
                }
            }
        }
        out
    }
}

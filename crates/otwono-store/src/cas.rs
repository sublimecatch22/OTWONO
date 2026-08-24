//! The chunk store on disk.
//!
//! # Every read is verified
//!
//! `get` re-hashes what it read and refuses to return bytes whose digest does not match the
//! name they were asked for. That is not paranoia about our own writes — it is what makes a
//! chunk from a peer, from a damaged disk, or from a half-finished write indistinguishable
//! from absent rather than indistinguishable from correct. The neighbourhood cache
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

#[cfg(test)]
mod tests {
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
        let mut x = seed | 1;
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
        // The property the neighbourhood cache depends on: a source does not have to be
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

    #[test]
    fn an_object_record_survives_a_round_trip_and_is_validated_on_the_way_out() {
        let d = tmp("record");
        let s = Store::new(&d);
        let object = s.put_bytes(&data(300_000, 17), Visibility::Shared).expect("put");
        let back = s.get_object(&object.content_id).expect("get");
        assert_eq!(back, object);
        assert_eq!(back.visibility, Visibility::Shared);
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
        // neighbourhood cache cannot exist.
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

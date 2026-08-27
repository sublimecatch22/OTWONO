//! Where a node keeps the pointers it publishes and the ones it has read (ADR-0027).
//!
//! # Names from the wire never become paths
//!
//! A pointer's `name` is chosen by whoever publishes it and arrives here from a stranger's
//! request. `../../etc/otwono/policy.d/10-default.toml` is a valid pointer name — 512 bytes
//! of anything is — so the on-disk filename is **the hash of the key**, never the name
//! itself. Sanitising instead would mean maintaining a list of what is dangerous on every
//! filesystem this ever runs on, and the first entry missing from that list is a file the
//! node overwrites.
//!
//! Hashing also makes the layout flat and case-exact, which matters on a filesystem that
//! would otherwise fold `Home` and `home` into one file and quietly serve the wrong record.

use otwono_pointer::{Accepted, Pointer, PointerError, PointerKey, SequenceLog};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const DEFAULT_POINTER_DIR: &str = "/var/lib/otwono/pointers";

/// The pointers this node publishes, and what it has seen from others.
pub struct PointerStore {
    root: PathBuf,
    /// The rollback defence for records read from peers. Held in memory and persisted
    /// alongside the records, because a log that does not survive a restart drops every
    /// reader back to first-use trust on the next boot (ADR-0027).
    seen: Mutex<SequenceLog>,
}

impl std::fmt::Debug for PointerStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PointerStore").field("root", &self.root).finish()
    }
}

impl PointerStore {
    pub fn at(root: impl AsRef<Path>) -> Result<PointerStore, PointerStoreError> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(root.join("mine"))?;
        std::fs::create_dir_all(root.join("seen"))?;
        let seen = match std::fs::read(root.join("sequences.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                // A corrupt log is not a reason to refuse to start, but it *is* a reason to
                // say so: the node continues with no rollback memory, which is a real loss
                // of protection and must not happen silently.
                eprintln!(
                    "otwono-store: the pointer sequence log at {} is unreadable; \
                     rollback protection restarts from first use",
                    root.join("sequences.json").display()
                );
                SequenceLog::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => SequenceLog::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(PointerStore {
            root,
            seen: Mutex::new(seen),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The next sequence this node should use for one of its own pointers.
    ///
    /// Derived from what is already published rather than taken from the caller. A caller
    /// that chose its own could regress — either by mistake or because it lost track — and
    /// a pointer that goes backwards is unreadable to every peer that saw the higher one,
    /// permanently. The store knows the last number; the caller does not have to.
    pub fn next_sequence(&self, service: &str, name: &str) -> Result<u64, PointerStoreError> {
        Ok(match self.mine(service, name)? {
            Some(p) => p.sequence.saturating_add(1),
            None => 1,
        })
    }

    /// Store one of this node's own pointers.
    ///
    /// The record must already be signed; this does not sign, because signing needs the node
    /// key and this crate must never hold it (ADR-0010). It checks the sequence advances,
    /// which is the one thing a store can do that a caller might forget.
    pub fn publish(&self, pointer: &Pointer) -> Result<(), PointerStoreError> {
        if let Some(existing) = self.mine(&pointer.service, &pointer.name)? {
            if pointer.sequence <= existing.sequence {
                return Err(PointerStoreError::WouldRegress {
                    published: existing.sequence,
                    offered: pointer.sequence,
                });
            }
        }
        let path = self.path_for("mine", &pointer.service, &pointer.name);
        write_atomically(&path, &serde_json::to_vec(pointer)?)?;
        Ok(())
    }

    /// One of this node's own pointers, if it has published that name.
    pub fn mine(&self, service: &str, name: &str) -> Result<Option<Pointer>, PointerStoreError> {
        read_record(&self.path_for("mine", service, name))
    }

    /// Everything this node publishes, for an operator to look at.
    pub fn published(&self) -> Result<Vec<Pointer>, PointerStoreError> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(self.root.join("mine"))? {
            let entry = entry?;
            // A damaged record is skipped rather than failing the listing: one unreadable
            // file must not make a node unable to say what else it publishes.
            if let Ok(Some(p)) = read_record(&entry.path()) {
                out.push(p);
            }
        }
        out.sort_by(|a, b| (&a.service, &a.name).cmp(&(&b.service, &b.name)));
        Ok(out)
    }

    /// Take a record read from a peer, if it is genuinely newer than what we have.
    ///
    /// The verification and the rollback rules are `otwono-pointer`'s; this adds durability,
    /// so the protection survives a restart.
    pub fn accept_from_peer(
        &self,
        pointer: &Pointer,
        public_key: &[u8; 32],
        expected: &PointerKey,
    ) -> Result<Accepted, PointerStoreError> {
        let outcome = {
            let mut seen = self.seen.lock().expect("pointer log poisoned");
            seen.accept(pointer, public_key, expected)?
        };
        let path = self.seen_path(expected);
        write_atomically(&path, &serde_json::to_vec(pointer)?)?;
        self.persist_sequences()?;
        Ok(outcome)
    }

    /// The most recent record read from a peer for this pointer.
    pub fn from_peer(&self, key: &PointerKey) -> Result<Option<Pointer>, PointerStoreError> {
        read_record(&self.seen_path(key))
    }

    /// The highest sequence seen for a peer's pointer, or `None` if it is new to us.
    pub fn highest_seen(&self, key: &PointerKey) -> Option<u64> {
        self.seen.lock().expect("pointer log poisoned").highest_seen(key)
    }

    fn persist_sequences(&self) -> Result<(), PointerStoreError> {
        let seen = self.seen.lock().expect("pointer log poisoned");
        write_atomically(&self.root.join("sequences.json"), &serde_json::to_vec(&*seen)?)
    }

    /// Where a peer's record for `key` is kept.
    fn seen_path(&self, key: &PointerKey) -> PathBuf {
        self.hashed_path("seen", &[&key.node_id, &key.service, &key.name])
    }

    fn path_for(&self, kind: &str, service: &str, name: &str) -> PathBuf {
        self.hashed_path(kind, &[service, name])
    }

    /// The file a set of key parts maps to: `<root>/<kind>/<hash>.json`.
    ///
    /// Each part is followed by a zero byte, which cannot appear in any of them — they are
    /// all UTF-8 strings from a validated record. That makes the encoding unambiguous:
    /// `("wiki", "a/b")` and `("wiki/a", "b")` hash differently, where joining the parts with
    /// a separator that *could* occur inside one would let a chosen name collide with a
    /// different pointer and overwrite it.
    ///
    /// An earlier version pre-joined the node id and service with a slash before hashing,
    /// which happened to be safe only because neither may contain one — a property held
    /// somewhere else entirely, and exactly the kind of accident this comment exists to
    /// stop being relied on.
    fn hashed_path(&self, kind: &str, parts: &[&str]) -> PathBuf {
        let mut hasher = blake3::Hasher::new();
        for part in parts {
            hasher.update(part.as_bytes());
            hasher.update(&[0]);
        }
        // Hex of the digest directly, not a ContentId: that type means "the id of this
        // object", and this is a hash of a lookup key. Borrowing it would make a filename
        // look like something a peer could ask for.
        let digest: String = hasher
            .finalize()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        self.root.join(kind).join(format!("{digest}.json"))
    }
}

fn read_record(path: &Path) -> Result<Option<Pointer>, PointerStoreError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write through a temporary file and rename.
///
/// A pointer that is half-written is a pointer that cannot be read, and the moment a node is
/// most likely to be interrupted is while it is publishing. Rename within a directory is
/// atomic on every filesystem this targets.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), PointerStoreError> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Debug)]
pub enum PointerStoreError {
    Io(String),
    Encoding(String),
    /// The record offered is not newer than what this node already published under that
    /// name. Its own doing, not a peer's — a separate case from [`PointerError::Rollback`],
    /// which is about a record from somewhere else.
    WouldRegress {
        published: u64,
        offered: u64,
    },
    Pointer(PointerError),
}

impl From<std::io::Error> for PointerStoreError {
    fn from(e: std::io::Error) -> Self {
        PointerStoreError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for PointerStoreError {
    fn from(e: serde_json::Error) -> Self {
        PointerStoreError::Encoding(e.to_string())
    }
}

impl From<PointerError> for PointerStoreError {
    fn from(e: PointerError) -> Self {
        PointerStoreError::Pointer(e)
    }
}

impl std::fmt::Display for PointerStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PointerStoreError::Io(m) => write!(f, "pointer store: {m}"),
            PointerStoreError::Encoding(m) => write!(f, "pointer record: {m}"),
            PointerStoreError::WouldRegress { published, offered } => write!(
                f,
                "this node already published sequence {published} for that name; {offered} would go backwards"
            ),
            PointerStoreError::Pointer(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PointerStoreError {}

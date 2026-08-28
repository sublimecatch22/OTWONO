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

use otwono_pointer::{Accepted, Pointer, PointerError, PointerKey, SequenceLog, SequenceMemory};
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

/// Write through a temporary file and rename, and make it survive losing power.
///
/// A pointer that is half-written is a pointer that cannot be read, and the moment a node is
/// most likely to be interrupted is while it is publishing. Rename within a directory is
/// atomic on every filesystem this targets.
///
/// # Atomic is not durable, and this file needs both
///
/// Rename alone means a reader never sees half a record. It does **not** mean the record is
/// on the disk: without the fsyncs, both the bytes and the directory entry can still be in
/// the page cache when the power goes, and the file comes back empty, stale, or absent.
///
/// For most files that would be an annoyance. For `sequences.json` it is the rollback
/// defence disappearing (ADR-0027 §1) — a reader that loses its log drops back to first-use
/// trust, which is the whole protection gone, and gone *silently*, since a node with no
/// memory cannot tell that it used to have one. An attacker who can arrange a power cut
/// should not be handed that.
///
/// So: fsync the data before the rename, and fsync the **directory** after it, because the
/// rename itself is a directory modification and is no more durable than the bytes were.
///
/// The temporary name carries a counter. Two threads writing the same key would otherwise
/// share one temp path and interleave into it — `otwono-stored` serves each connection on
/// its own thread, so "two at once for one pointer" is a scheduling accident away, and the
/// file it produces would be neither record.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), PointerStoreError> {
    use std::io::Write;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "tmp.{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));

    let write_and_sync = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    };
    if let Err(e) = write_and_sync() {
        // A temp file left behind is invisible to every reader here (they open the hashed
        // name), but it is still litter, and litter in a directory that gets listed.
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }

    // The rename is what makes the new bytes reachable, so it is the part that has to
    // survive. Not fatal if the directory cannot be opened -- the record is written and
    // correct either way, and refusing a publish that succeeded would be the worse failure.
    if let Some(dir) = path.parent() {
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }
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

impl SequenceMemory for PointerStore {
    /// The durable half of the rollback defence.
    ///
    /// Errors from the store itself are mapped onto `PointerError` so a caller sees one kind
    /// of failure. A record that verified and advanced but could not be written is reported
    /// as a malformed store rather than silently accepted — the in-memory log has already
    /// moved, and returning success would promise a protection that will not survive a
    /// restart.
    fn accept(
        &self,
        pointer: &Pointer,
        public_key: &[u8; 32],
        expected: &PointerKey,
    ) -> Result<Accepted, PointerError> {
        match self.accept_from_peer(pointer, public_key, expected) {
            Ok(accepted) => Ok(accepted),
            Err(PointerStoreError::Pointer(e)) => Err(e),
            Err(other) => Err(PointerError::Malformed(other.to_string())),
        }
    }
}

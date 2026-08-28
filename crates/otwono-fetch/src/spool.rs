//! The spool: where a fetch accumulates, and how a resumed one finds its partial.
//!
//! Nothing here writes into a subsystem's store. `otwono-fetchd` owns this directory and
//! hands back a path; the caller verifies the bytes and installs them with its own code
//! (ADR-0014). The separation is the point — the process that talks to a stranger is not
//! the process that decides what is trustworthy.
//!
//! # Why a fetch is resumable by construction
//!
//! The control plane's client sets a read timeout, and a 4 GB model on a rural link does
//! not fit inside one. So `fetch.get` is bounded — some bytes, then a return — and the
//! caller calls again. That makes resumption the normal path rather than an error path,
//! which is the only way it ever gets tested.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where an image spools partial downloads.
pub const DEFAULT_SPOOL_DIR: &str = "/var/lib/otwono/fetch";

pub const META_SCHEMA_VERSION: &str = "1.0.0";

/// Remote-supplied strings are capped before they are stored. An ETag is whatever the
/// server said, which makes it exactly the sort of thing that should not be unbounded.
pub const MAX_ETAG_BYTES: usize = 256;

/// Hex characters of the spool key. 128 bits of BLAKE3 is far more than a per-node
/// filename namespace needs, and a short name keeps the directory readable.
const KEY_HEX_CHARS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub schema_version: String,
    pub source: String,
    pub path: String,
    pub url: String,
    /// Server's entity tag, if it gave one. Used only to notice that the remote object
    /// changed under a resumed download; the caller's digest is what decides correctness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpoolError {
    Io(String),
    /// The spool holds something we cannot make sense of.
    Corrupt(String),
    /// There is not enough room to finish this.
    NoSpace {
        need: u64,
        free: u64,
    },
}

impl std::fmt::Display for SpoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpoolError::Io(m) => write!(f, "{m}"),
            SpoolError::Corrupt(m) => write!(f, "spool entry is unusable: {m}"),
            SpoolError::NoSpace { need, free } => write!(
                f,
                "not enough room in the spool: {need} bytes needed, {free} free"
            ),
        }
    }
}

impl std::error::Error for SpoolError {}

/// One object being fetched, identified by where it came from rather than by what it is.
#[derive(Debug, Clone)]
pub struct SpoolEntry {
    dir: PathBuf,
    key: String,
    source: String,
    path: String,
}

impl SpoolEntry {
    /// The key is a hash of the source and path, never the path itself: a filename derived
    /// from caller-influenced text is a directory traversal waiting to be found, and this
    /// way the spool's layout owes nothing to what a remote object is called.
    pub fn new(dir: impl AsRef<Path>, source: &str, path: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(source.as_bytes());
        hasher.update(b"\0");
        hasher.update(path.as_bytes());
        let key: String = hasher.finalize().to_hex().chars().take(KEY_HEX_CHARS).collect();
        SpoolEntry {
            dir: dir.as_ref().to_path_buf(),
            key,
            source: source.to_string(),
            path: path.to_string(),
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn part_path(&self) -> PathBuf {
        self.dir.join(format!("{}.part", self.key))
    }

    pub fn blob_path(&self) -> PathBuf {
        self.dir.join(format!("{}.blob", self.key))
    }

    pub fn meta_path(&self) -> PathBuf {
        self.dir.join(format!("{}.meta", self.key))
    }

    /// How much of this object is already on disk. Zero when nothing is.
    pub fn have_bytes(&self) -> u64 {
        std::fs::metadata(self.part_path()).map(|m| m.len()).unwrap_or(0)
    }

    /// A finished blob is one that was renamed into place, so this is never true of a
    /// truncated file — the same property `ai.models.install` relies on.
    pub fn is_complete(&self) -> bool {
        self.blob_path().exists()
    }

    pub fn read_meta(&self) -> Result<Option<Meta>, SpoolError> {
        match std::fs::read_to_string(self.meta_path()) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| SpoolError::Corrupt(format!("{}: {e}", self.meta_path().display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SpoolError::Io(format!("{}: {e}", self.meta_path().display()))),
        }
    }

    pub fn write_meta(&self, meta: &Meta) -> Result<(), SpoolError> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| SpoolError::Io(format!("{}: {e}", self.dir.display())))?;
        let text = serde_json::to_string_pretty(meta)
            .map_err(|e| SpoolError::Io(format!("cannot serialise spool metadata: {e}")))?;
        std::fs::write(self.meta_path(), text)
            .map_err(|e| SpoolError::Io(format!("{}: {e}", self.meta_path().display())))
    }

    pub fn meta_for(&self, url: &str, etag: Option<&str>, total_bytes: Option<u64>) -> Meta {
        Meta {
            schema_version: META_SCHEMA_VERSION.to_string(),
            source: self.source.clone(),
            path: self.path.clone(),
            url: url.to_string(),
            etag: etag.map(sanitize_etag),
            total_bytes,
        }
    }

    /// Throw away a partial download. Called when the remote object changed under us, and
    /// by `fetch.discard`.
    pub fn reset(&self) -> Result<(), SpoolError> {
        for p in [self.part_path(), self.meta_path()] {
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(SpoolError::Io(format!("{}: {e}", p.display()))),
            }
        }
        Ok(())
    }

    /// Everything this entry has, partial or finished.
    pub fn discard(&self) -> Result<(), SpoolError> {
        self.reset()?;
        match std::fs::remove_file(self.blob_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SpoolError::Io(format!("{}: {e}", self.blob_path().display()))),
        }
    }

    /// Promote a complete partial to a blob, atomically.
    ///
    /// Rename rather than a flag file, for the same reason installs use one: an interrupted
    /// finish leaves a `.part`, never a `.blob` that is short of its last megabyte.
    pub fn finish(&self) -> Result<PathBuf, SpoolError> {
        let part = self.part_path();
        let blob = self.blob_path();
        std::fs::rename(&part, &blob)
            .map_err(|e| SpoolError::Io(format!("{} -> {}: {e}", part.display(), blob.display())))?;
        Ok(blob)
    }
}

/// Refuse to start a download the disk cannot hold.
///
/// The target hardware is an SBC with an 8 GB eMMC and a model is measured in gigabytes,
/// so "fill the disk, then fail" is the default outcome unless something checks first.
/// `slack` is held back so that filling the spool cannot also stop the node logging.
pub fn ensure_room(dir: &Path, need: u64, slack: u64) -> Result<(), SpoolError> {
    let free = free_bytes(dir)?;
    if free < need.saturating_add(slack) {
        return Err(SpoolError::NoSpace { need, free });
    }
    Ok(())
}

pub fn free_bytes(dir: &Path) -> Result<u64, SpoolError> {
    let stat = rustix::fs::statvfs(dir).map_err(|e| SpoolError::Io(format!("{}: {e}", dir.display())))?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

/// An ETag is whatever a remote server chose to send. Bound it and strip anything that
/// would make a log line lie about its own structure.
fn sanitize_etag(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .take(MAX_ETAG_BYTES)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "otwono-spool-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn the_key_owes_nothing_to_what_the_remote_object_is_called() {
        // The whole reason for hashing: a path suffix is caller-influenced text, and a
        // filename built from it is a traversal waiting to happen.
        let e = SpoolEntry::new("/var/lib/otwono/fetch", "hf", "a/../../etc/passwd");
        assert!(e.key().chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(e.key().len(), KEY_HEX_CHARS);
        assert_eq!(
            e.part_path().parent().unwrap(),
            Path::new("/var/lib/otwono/fetch")
        );
    }

    #[test]
    fn the_same_request_resumes_the_same_entry() {
        let a = SpoolEntry::new("/spool", "hf", "m.gguf");
        let b = SpoolEntry::new("/spool", "hf", "m.gguf");
        assert_eq!(a.key(), b.key());
    }

    #[test]
    fn a_different_source_is_a_different_entry_even_for_the_same_path() {
        let a = SpoolEntry::new("/spool", "hf", "m.gguf");
        let b = SpoolEntry::new("/spool", "mirror", "m.gguf");
        assert_ne!(a.key(), b.key());
    }

    #[test]
    fn the_source_and_path_are_separated_so_they_cannot_be_confused() {
        // Without the separator, ("ab","c") and ("a","bc") would be the same entry.
        let a = SpoolEntry::new("/spool", "ab", "c");
        let b = SpoolEntry::new("/spool", "a", "bc");
        assert_ne!(a.key(), b.key());
    }

    #[test]
    fn an_unfinished_fetch_reports_what_it_has() {
        let dir = tmpdir("partial");
        let e = SpoolEntry::new(&dir, "hf", "m.gguf");
        assert_eq!(e.have_bytes(), 0);
        assert!(!e.is_complete());
        std::fs::write(e.part_path(), b"0123456789").expect("write part");
        assert_eq!(e.have_bytes(), 10);
        assert!(!e.is_complete());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finishing_is_a_rename_so_a_blob_is_never_short() {
        let dir = tmpdir("finish");
        let e = SpoolEntry::new(&dir, "hf", "m.gguf");
        std::fs::write(e.part_path(), b"payload").expect("write part");
        let blob = e.finish().expect("finish");
        assert!(e.is_complete());
        assert!(!e.part_path().exists());
        assert_eq!(std::fs::read(blob).unwrap(), b"payload");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reset_drops_the_partial_and_keeps_nothing_behind() {
        let dir = tmpdir("reset");
        let e = SpoolEntry::new(&dir, "hf", "m.gguf");
        std::fs::write(e.part_path(), b"half").expect("write part");
        e.write_meta(&e.meta_for("https://h/x", Some("\"v1\""), Some(99)))
            .expect("meta");
        e.reset().expect("reset");
        assert_eq!(e.have_bytes(), 0);
        assert!(e.read_meta().expect("meta read").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resetting_something_that_was_never_started_is_not_an_error() {
        let dir = tmpdir("reset-empty");
        let e = SpoolEntry::new(&dir, "hf", "never");
        e.reset().expect("reset of nothing");
        e.discard().expect("discard of nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn metadata_round_trips() {
        let dir = tmpdir("meta");
        let e = SpoolEntry::new(&dir, "hf", "m.gguf");
        let meta = e.meta_for("https://huggingface.co/m.gguf", Some("\"abc\""), Some(1234));
        e.write_meta(&meta).expect("write");
        assert_eq!(e.read_meta().expect("read"), Some(meta));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hostile_etag_cannot_smuggle_control_characters_into_the_record() {
        let e = SpoolEntry::new("/spool", "hf", "m.gguf");
        let meta = e.meta_for("https://h/x", Some("\"a\n\r\0b\""), None);
        let etag = meta.etag.expect("etag");
        assert!(!etag.contains('\n') && !etag.contains('\r') && !etag.contains('\0'));
    }

    #[test]
    fn an_enormous_etag_is_truncated_rather_than_stored() {
        let e = SpoolEntry::new("/spool", "hf", "m.gguf");
        let meta = e.meta_for("https://h/x", Some(&"x".repeat(10_000)), None);
        assert_eq!(meta.etag.expect("etag").len(), MAX_ETAG_BYTES);
    }

    #[test]
    fn corrupt_metadata_is_reported_rather_than_ignored() {
        let dir = tmpdir("corrupt");
        let e = SpoolEntry::new(&dir, "hf", "m.gguf");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(e.meta_path(), b"{not json").expect("write");
        assert!(matches!(e.read_meta(), Err(SpoolError::Corrupt(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_download_larger_than_the_disk_is_refused_before_it_starts() {
        let dir = tmpdir("space");
        let free = free_bytes(&dir).expect("statvfs");
        assert!(
            ensure_room(&dir, 1, 0).is_ok(),
            "one byte should fit in {free} free"
        );
        let err = ensure_room(&dir, u64::MAX / 2, 0).expect_err("should not fit");
        assert!(matches!(err, SpoolError::NoSpace { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_slack_reserve_is_held_back_from_a_fetch() {
        let dir = tmpdir("slack");
        let free = free_bytes(&dir).expect("statvfs");
        // Asking for everything free must fail once any slack is demanded on top.
        assert!(matches!(
            ensure_room(&dir, free, 1 << 30),
            Err(SpoolError::NoSpace { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

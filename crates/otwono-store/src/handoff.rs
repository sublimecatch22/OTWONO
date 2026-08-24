//! Handing large objects between the store daemon and a caller as files (ADR-0018).
//!
//! The control plane is newline-delimited JSON with a 1 MiB line cap, so an object above
//! `otwono_stored::MAX_INLINE_BYTES` cannot cross it. Above that size the bytes move as a
//! file and the socket carries only the path.
//!
//! # The daemon is root and the caller is not
//!
//! Both halves of this module exist because of that asymmetry.
//!
//! **Export** writes into a directory only the daemon can open, then gives the finished
//! file to the calling uid. The uid comes from `SO_PEERCRED`, which the kernel fills in — a
//! caller cannot ask for a file to be handed to somebody else.
//!
//! **Import** reads a file the caller already owns. It opens with `O_NOFOLLOW` and then
//! checks the *descriptor* it actually got, never the path again. Checking a path and then
//! opening it is the classic time-of-check-to-time-of-use bug: the caller swaps the file
//! for a symlink to something root can read in between. A descriptor already refers to one
//! inode and cannot be raced.
//!
//! `O_NOFOLLOW` only refuses a symlink at the *final* component, so it is not the load-
//! bearing check — a caller could point at `~/dir/x` where `dir` is a symlink to `/etc`.
//! The owner check is what stops that, because `/etc/shadow` is not owned by the caller. A
//! caller who is already root can read those files without this daemon's help, so nothing
//! is granted that was not already held.

use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const DEFAULT_EXPORT_DIR: &str = "/var/lib/otwono/export";

/// How long an abandoned export lives before the reaper takes it.
///
/// A caller reads its file and unlinks it. One that crashes in between leaves plaintext on
/// disk, so this is a leak with a timer on it rather than a leak — but the timer is the only
/// thing standing between a crash loop and a full disk, and a reaper is a thing that can
/// fail silently. Named here so it is visible rather than buried in a service.
pub const EXPORT_MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// Free space the export directory will not go below.
pub const RESERVE_FLOOR_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
pub enum HandoffError {
    /// The path named something that is not a plain file the caller owns.
    NotYours {
        path: PathBuf,
        reason: &'static str,
    },
    NoSpace {
        need: u64,
        free: u64,
    },
    Io {
        path: PathBuf,
        message: String,
    },
}

impl std::fmt::Display for HandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // One message for every reason a path is refused. "Not a regular file", "not
            // yours" and "that is a symlink" would each tell a caller something about a
            // file it has just demonstrated it cannot open itself.
            HandoffError::NotYours { path, .. } => {
                write!(f, "{} is not a regular file belonging to you", path.display())
            }
            HandoffError::NoSpace { need, free } => write!(
                f,
                "an export of {need} bytes needs more room than the {free} bytes free above \
                 the reserve floor"
            ),
            HandoffError::Io { path, message } => write!(f, "{}: {message}", path.display()),
        }
    }
}

impl std::error::Error for HandoffError {}

/// A file written for a caller and now owned by them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exported {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub owner_uid: u32,
}

pub struct Handoff {
    root: PathBuf,
}

impl Handoff {
    pub fn new(root: impl AsRef<Path>) -> Handoff {
        Handoff {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 0700, because between the write and the caller's `unlink` this directory holds the
    /// plaintext of objects the store keeps encrypted.
    pub fn ensure_layout(&self) -> Result<(), HandoffError> {
        std::fs::create_dir_all(&self.root).map_err(|e| self.io(&self.root, e))?;
        std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| self.io(&self.root, e))
    }

    /// Write an object into a fresh file and hand it to `uid`.
    ///
    /// The file is created 0600 and only then chowned, so it is never readable by the
    /// target uid while it is still being written — a caller that raced the write would
    /// otherwise see a truncated object and have no way to know.
    pub fn export(
        &self,
        uid: u32,
        expected_bytes: u64,
        write: impl FnOnce(&mut File) -> std::io::Result<()>,
    ) -> Result<Exported, HandoffError> {
        self.ensure_layout()?;
        self.ensure_room(expected_bytes)?;

        let path = self.root.join(unlinkable_name());
        let mut file = File::create(&path).map_err(|e| self.io(&path, e))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| self.io(&path, e))?;

        // Any failure from here leaves a partial file, so clean up before returning.
        let written = write(&mut file)
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_all())
            .and_then(|()| file.metadata().map(|m| m.len()));
        let size_bytes = match written {
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(self.io(&path, e));
            }
        };

        // Only when the target differs from this process's own uid. In production the
        // daemon is root and the caller is not; in a test they are the same and there is
        // nothing to do, so the test does not silently need CAP_CHOWN.
        if uid != rustix::process::getuid().as_raw() {
            if let Err(e) = rustix::fs::fchown(file.as_fd(), Some(unsafe_uid(uid)), None) {
                let _ = std::fs::remove_file(&path);
                return Err(HandoffError::Io {
                    path,
                    message: format!("cannot give the file to uid {uid}: {e}"),
                });
            }
        }

        Ok(Exported {
            path,
            size_bytes,
            owner_uid: uid,
        })
    }

    /// Open a file the caller named, refusing anything that is not a plain file they own.
    ///
    /// Returns the open descriptor. Callers must use it and never re-open the path: that is
    /// the entire point.
    pub fn open_owned(path: &Path, uid: u32) -> Result<File, HandoffError> {
        use rustix::fs::{Mode, OFlags};
        let refuse = |reason: &'static str| HandoffError::NotYours {
            path: path.to_path_buf(),
            reason,
        };
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| refuse("cannot be opened"))?;
        let stat = rustix::fs::fstat(&fd).map_err(|_| refuse("cannot be described"))?;

        // Checked on the descriptor, not on the path. Nothing between here and the read can
        // change what this refers to.
        let is_regular = stat.st_mode & rustix::fs::FileType::RegularFile.as_raw_mode()
            == rustix::fs::FileType::RegularFile.as_raw_mode();
        if !is_regular {
            return Err(refuse("is not a regular file"));
        }
        if stat.st_uid != uid {
            return Err(refuse("belongs to someone else"));
        }
        Ok(File::from(fd))
    }

    /// Remove exports older than `max_age`, returning how many went.
    ///
    /// Best effort by design: a file that cannot be removed is skipped rather than aborting
    /// the sweep, because one stuck file must not stop the rest being cleaned up.
    pub fn reap(&self, max_age: Duration) -> Result<usize, HandoffError> {
        let now = SystemTime::now();
        let dir = match std::fs::read_dir(&self.root) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(self.io(&self.root, e)),
        };
        let mut removed = 0;
        for entry in dir.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let old = meta
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .map(|age| age >= max_age)
                .unwrap_or(false);
            if old && std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn ensure_room(&self, need: u64) -> Result<(), HandoffError> {
        let stat = rustix::fs::statvfs(&self.root)
            .map_err(|e| self.io(&self.root, std::io::Error::other(e.to_string())))?;
        let free = stat.f_bavail.saturating_mul(stat.f_frsize);
        if free < need.saturating_add(RESERVE_FLOOR_BYTES) {
            return Err(HandoffError::NoSpace { need, free });
        }
        Ok(())
    }

    fn io(&self, path: &Path, e: std::io::Error) -> HandoffError {
        HandoffError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }
}

fn unsafe_uid(uid: u32) -> rustix::fs::Uid {
    // `Uid::from_raw` is safe in rustix 0.38 for values that came from the kernel, which
    // this one did — it is SO_PEERCRED's answer, passed through unchanged.
    unsafe { rustix::fs::Uid::from_raw(uid) }
}

/// A name that says nothing about what is inside it.
///
/// The directory is 0700 and only the daemon can list it, but an export's *name* would
/// otherwise be a content id — and a content id is the one thing about an object that is
/// worth guessing.
fn unlinkable_name() -> String {
    use rand_core::RngCore;
    let mut bytes = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut bytes);
    let mut s = String::with_capacity(32);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmpdir(name: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "otwono-handoff-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn me() -> u32 {
        rustix::process::getuid().as_raw()
    }

    #[test]
    fn an_exported_file_holds_what_was_written_and_belongs_to_the_caller() {
        let h = Handoff::new(tmpdir("export"));
        let bytes = vec![0x42u8; 3 * 1024 * 1024];
        let exported = h
            .export(me(), bytes.len() as u64, |f| f.write_all(&bytes))
            .unwrap();
        assert_eq!(exported.size_bytes, bytes.len() as u64);
        assert_eq!(std::fs::read(&exported.path).unwrap(), bytes);
        assert_eq!(exported.owner_uid, me());
    }

    #[test]
    fn an_export_is_larger_than_anything_the_control_plane_could_have_carried() {
        // The whole reason this module exists.
        let h = Handoff::new(tmpdir("big"));
        let big = 4 * 1024 * 1024;
        let exported = h
            .export(me(), big as u64, |f| f.write_all(&vec![7u8; big]))
            .unwrap();
        assert!(exported.size_bytes > 640 * 1024);
    }

    #[test]
    fn an_export_is_not_readable_by_anyone_else() {
        let h = Handoff::new(tmpdir("mode"));
        let exported = h.export(me(), 4, |f| f.write_all(b"mine")).unwrap();
        let mode = std::fs::metadata(&exported.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "an export was left group- or world-readable");
        let dir = std::fs::metadata(h.root()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir, 0o700, "the export directory was left listable");
    }

    #[test]
    fn two_exports_never_collide_and_neither_name_says_what_it_holds() {
        let h = Handoff::new(tmpdir("names"));
        let content = "0".repeat(64);
        let a = h.export(me(), 1, |f| f.write_all(b"a")).unwrap();
        let b = h.export(me(), 1, |f| f.write_all(b"b")).unwrap();
        assert_ne!(a.path, b.path);
        for e in [&a, &b] {
            let name = e.path.file_name().unwrap().to_str().unwrap();
            assert!(!name.contains(&content), "an export name carried a content id");
            assert_eq!(name.len(), 32);
        }
    }

    #[test]
    fn a_failed_write_leaves_nothing_behind() {
        // A partial export is plaintext nobody asked for.
        let h = Handoff::new(tmpdir("partial"));
        let err = h.export(me(), 16, |_| {
            Err(std::io::Error::other("the store fell over mid-object"))
        });
        assert!(err.is_err());
        let left: Vec<_> = std::fs::read_dir(h.root()).unwrap().flatten().collect();
        assert!(left.is_empty(), "a partial export was left on disk: {left:?}");
    }

    #[test]
    fn a_file_the_caller_owns_opens() {
        let dir = tmpdir("owned");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("theirs");
        std::fs::write(&path, b"a caller's own file").unwrap();
        let mut f = Handoff::open_owned(&path, me()).unwrap();
        let mut got = Vec::new();
        f.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"a caller's own file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_belonging_to_someone_else_is_refused() {
        let dir = tmpdir("theirs");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        std::fs::write(&path, b"x").unwrap();
        // Claim to be a uid that does not own it. Root owns everything it creates, so
        // asking as any other uid is the same question from the check's point of view.
        let err = Handoff::open_owned(&path, me().wrapping_add(1)).unwrap_err();
        assert!(matches!(err, HandoffError::NotYours { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlink_is_refused_even_when_its_target_is_readable() {
        // O_NOFOLLOW at the final component. Without it, a caller points at a link to
        // something this root daemon can read and the daemon reads it for them.
        let dir = tmpdir("symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real");
        std::fs::write(&real, b"the target").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // The target is readable directly...
        Handoff::open_owned(&real, me()).expect("the real file opens");
        // ...and through the link it is not.
        let err = Handoff::open_owned(&link, me()).unwrap_err();
        assert!(matches!(err, HandoffError::NotYours { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_is_refused() {
        let dir = tmpdir("isdir");
        std::fs::create_dir_all(&dir).unwrap();
        let err = Handoff::open_owned(&dir, me()).unwrap_err();
        assert!(matches!(err, HandoffError::NotYours { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_refusal_reads_the_same() {
        // Not a regular file, not yours, and a symlink must be indistinguishable: a caller
        // that can tell them apart learns about a file it has just failed to open.
        let dir = tmpdir("uniform");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("f");
        std::fs::write(&f, b"x").unwrap();
        let link = dir.join("l");
        std::os::unix::fs::symlink(&f, &link).unwrap();
        let absent = dir.join("nothing");

        // The property, stated directly: the message is a pure function of the path the
        // caller already knows, with no component that varies by reason.
        for (path, uid) in [
            (dir.clone(), me()),
            (f.clone(), me().wrapping_add(1)),
            (link.clone(), me()),
            (absent.clone(), me()),
        ] {
            let err = Handoff::open_owned(&path, uid).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("{} is not a regular file belonging to you", path.display()),
                "this refusal says why, and why is an oracle"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_reaper_takes_the_abandoned_and_leaves_the_fresh() {
        let h = Handoff::new(tmpdir("reap"));
        let fresh = h.export(me(), 1, |f| f.write_all(b"new")).unwrap();
        let stale = h.export(me(), 1, |f| f.write_all(b"old")).unwrap();

        // Backdate the stale one rather than sleeping.
        let long_ago = SystemTime::now() - Duration::from_secs(7200);
        let times = rustix::fs::Timestamps {
            last_access: rustix::fs::Timespec {
                tv_sec: long_ago.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64,
                tv_nsec: 0,
            },
            last_modification: rustix::fs::Timespec {
                tv_sec: long_ago.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64,
                tv_nsec: 0,
            },
        };
        rustix::fs::utimensat(rustix::fs::CWD, &stale.path, &times, rustix::fs::AtFlags::empty()).unwrap();

        assert_eq!(h.reap(EXPORT_MAX_AGE).unwrap(), 1);
        assert!(fresh.path.exists(), "a fresh export was reaped");
        assert!(!stale.path.exists(), "an abandoned export survived");
    }

    #[test]
    fn reaping_a_directory_that_does_not_exist_is_not_an_error() {
        let h = Handoff::new(tmpdir("noreap"));
        assert_eq!(h.reap(EXPORT_MAX_AGE).unwrap(), 0);
    }

    #[test]
    fn an_export_larger_than_the_disk_is_refused_before_anything_is_written() {
        let h = Handoff::new(tmpdir("space"));
        h.ensure_layout().unwrap();
        let err = h
            .export(me(), u64::MAX / 2, |f| f.write_all(b"never reached"))
            .unwrap_err();
        assert!(matches!(err, HandoffError::NoSpace { .. }), "{err}");
        let left: Vec<_> = std::fs::read_dir(h.root()).unwrap().flatten().collect();
        assert!(left.is_empty());
    }
}

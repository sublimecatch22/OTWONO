//! Kernel-enforced confinement for the inference engine.
//!
//! `llama-server` is a large C++ program whose whole job is parsing files that arrived
//! from somewhere else. It runs in this adapter's process tree, and until now the only
//! thing standing between a malicious GGUF and the rest of the node was
//! `otwono-aid.service`'s systemd hardening — which is real, but is written for a daemon
//! that reads a catalog, not for a parser of untrusted binary blobs.
//!
//! So the adapter restricts itself with Landlock before it ever starts an engine
//! (ADR-0012). Landlock is inherited by descendants and cannot be undone, so the engine is
//! confined by construction rather than by remembering to confine it.
//!
//! # Why the adapter restricts *itself*
//!
//! The obvious place to apply this is in the child, between fork and exec — which in Rust
//! means `Command::pre_exec`, which is `unsafe`. Putting `unsafe` in the process that
//! handles untrusted model files to make it safer is a poor trade. Restricting the adapter
//! at startup avoids it entirely, and confines the adapter too, which is strictly better:
//! the adapter has no more business reading the node's private key than the engine does.
//!
//! The cost is that the policy has to be known at startup, before any `backend.load` names
//! a model. That is why the adapter is told a *model directory* rather than being handed
//! arbitrary paths: the catalog's blob store is the set of files it may ever read.
//!
//! # What this is and is not
//!
//! It is a filesystem boundary. An engine that is compromised while parsing a model cannot
//! read the node identity key, the audit log, the policy store, or the user's files.
//!
//! It is **not** a complete sandbox. There is no PID or mount namespace, no seccomp filter,
//! and `/proc` and `/sys` are readable because ggml's CPU detection needs them. A
//! compromised engine can still exhaust CPU, and can see what any process can see through
//! procfs. Those are worth closing later; this closes the one that leaks secrets.

use std::path::{Path, PathBuf};

use landlock::{
    Access, AccessFs, PathBeneath, PathFd, RulesetAttr, RulesetCreatedAttr, RulesetError, RulesetStatus, ABI,
};

/// The Landlock ABI this policy is written against.
///
/// V1 is the 5.13 filesystem interface, and everything here is filesystem. Asking for more
/// would gain nothing and would refuse to run on kernels that are perfectly capable of
/// enforcing what we actually need.
const POLICY_ABI: ABI = ABI::V1;

/// What the engine is allowed to touch.
///
/// Built from the three paths the adapter is told about plus the system directories any
/// dynamically linked program needs. Everything else is denied, and the list of what
/// "everything else" includes is the point: `/etc/otwono`, `/var/lib/otwono/identity`,
/// `/var/log/otwono`, `/root`, `/home`.
#[derive(Debug, Clone)]
pub struct Policy {
    /// The engine binary. Read and execute, on its parent directory — a wrapper script
    /// and the binary it execs usually live together.
    pub engine: PathBuf,
    /// The model blob store. Read only: the engine never writes a model.
    pub model_dir: PathBuf,
    /// Where the engine's socket is created. The only writable path in the policy.
    pub runtime_dir: PathBuf,
}

/// Read-only system directories a dynamically linked program needs.
///
/// Absent entries are skipped rather than fatal: `/lib64` does not exist everywhere, and
/// on a merged-`/usr` system `/lib` and `/bin` are symlinks into `/usr`.
const SYSTEM_READ_EXEC: &[&str] = &["/usr", "/lib", "/lib64", "/bin", "/sbin"];

/// Read-only paths that are not directories of code.
///
/// `/proc` and `/sys` are here because ggml reads CPU topology from them at startup and
/// refuses to run well without it. That is a real widening of the boundary and it is
/// called out in ADR-0012 rather than hidden: procfs exposes plenty. It does not expose
/// the node's keys, which is what this exists to protect.
const SYSTEM_READ_ONLY: &[&str] = &["/proc", "/sys", "/etc/ld.so.cache", "/etc/localtime"];

/// Character devices the C++ runtime opens.
const DEVICES: &[&str] = &["/dev/null", "/dev/urandom", "/dev/random", "/dev/zero"];

/// One entry in the policy: a path and what may be done beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub path: PathBuf,
    pub access: BitFlags,
    /// A path that is allowed to be missing. System layouts differ.
    pub optional: bool,
}

/// The access classes this policy uses, kept as our own type so the rule table can be
/// asserted on in tests without depending on Landlock's flag representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitFlags {
    /// Read files and directories, and execute programs.
    ReadExecute,
    /// Read files and directories.
    ReadOnly,
    /// Everything needed to create, use and remove a Unix socket.
    ReadWrite,
}

impl BitFlags {
    fn to_access(self) -> landlock::BitFlags<AccessFs> {
        match self {
            BitFlags::ReadExecute => AccessFs::ReadFile | AccessFs::ReadDir | AccessFs::Execute,
            BitFlags::ReadOnly => AccessFs::ReadFile | AccessFs::ReadDir,
            BitFlags::ReadWrite => {
                AccessFs::ReadFile
                    | AccessFs::ReadDir
                    | AccessFs::WriteFile
                    | AccessFs::MakeSock
                    | AccessFs::MakeReg
                    | AccessFs::MakeDir
                    | AccessFs::RemoveFile
                    | AccessFs::RemoveDir
            }
        }
    }
}

impl Policy {
    /// The complete rule table, in the order it is applied.
    ///
    /// Pure, so the policy can be asserted on without restricting the test process —
    /// which matters more than usual here, because Landlock cannot be undone: a test that
    /// applied a ruleset would confine the whole test runner for every test after it.
    pub fn rules(&self) -> Vec<Rule> {
        let mut rules = Vec::new();

        // The engine, and whatever sits beside it. A backend is often a wrapper script
        // next to the binary it starts.
        rules.push(Rule {
            path: engine_root(&self.engine),
            access: BitFlags::ReadExecute,
            optional: false,
        });
        for path in SYSTEM_READ_EXEC {
            rules.push(Rule {
                path: PathBuf::from(path),
                access: BitFlags::ReadExecute,
                optional: true,
            });
        }
        for path in SYSTEM_READ_ONLY {
            rules.push(Rule {
                path: PathBuf::from(path),
                access: BitFlags::ReadOnly,
                optional: true,
            });
        }
        for path in DEVICES {
            rules.push(Rule {
                path: PathBuf::from(path),
                access: BitFlags::ReadOnly,
                optional: true,
            });
        }
        rules.push(Rule {
            path: self.model_dir.clone(),
            access: BitFlags::ReadOnly,
            optional: false,
        });
        rules.push(Rule {
            path: self.runtime_dir.clone(),
            access: BitFlags::ReadWrite,
            optional: false,
        });
        rules
    }

    /// Whether `path` is somewhere this policy permits a model to be read from.
    ///
    /// Checked before the engine is started so a model outside the sandbox is refused with
    /// a sentence that names the problem, rather than surfacing from inside the engine as
    /// a permission error on a file it will not say much about.
    pub fn permits_model(&self, path: &Path) -> bool {
        // Compared after canonicalization so `..` cannot walk out of the model directory.
        let (Ok(model_dir), Ok(path)) = (self.model_dir.canonicalize(), path.canonicalize()) else {
            return false;
        };
        path.starts_with(model_dir)
    }
}

/// The directory whose contents the engine may read and execute.
///
/// The binary's parent, not the binary itself, because a Landlock rule on a single file
/// would not cover a wrapper script's sibling.
fn engine_root(engine: &Path) -> PathBuf {
    engine
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Ask the kernel whether it will actually enforce anything — **by restricting this
/// process**.
///
/// Only for a process that is about to exit. Landlock cannot be undone, and this applies a
/// ruleset with no rules, which denies everything: whatever calls this keeps its already
/// open file descriptors and can print, and should then leave.
///
/// It has to be done this way. The cheap probe — build a ruleset and see whether it errors
/// — does not work: under best-effort compatibility, creating a ruleset succeeds on a
/// kernel with no Landlock at all, and only `restrict_self` reveals that nothing was
/// enforced. That version of this function reported "available" on a kernel whose
/// `landlock_create_ruleset` returns `ENOSYS`, which is the wrong answer for something a
/// fail-closed decision rests on. The crate keeps its own runtime ABI query private, on
/// the reasoning that policies should not vary with the kernel underneath them — so the
/// authoritative answer is the one the kernel gives when you actually ask it to enforce.
pub fn probe_by_restricting_this_process() -> Enforcement {
    match landlock::Ruleset::default()
        .handle_access(AccessFs::from_all(POLICY_ABI))
        .and_then(|r| r.create())
        .and_then(|r| r.restrict_self())
    {
        Ok(status) => match status.ruleset {
            RulesetStatus::FullyEnforced => Enforcement::Full,
            RulesetStatus::PartiallyEnforced => Enforcement::Partial,
            RulesetStatus::NotEnforced => Enforcement::None,
        },
        Err(_) => Enforcement::None,
    }
}

/// Restrict this process, and therefore every process it starts.
///
/// Irreversible by design. Returns what the kernel actually enforced, which the caller
/// must act on rather than assume.
pub fn restrict(policy: &Policy) -> Result<Enforcement, SandboxError> {
    let access = AccessFs::from_all(POLICY_ABI);
    let mut ruleset = landlock::Ruleset::default()
        .handle_access(access)
        .map_err(SandboxError::from)?
        .create()
        .map_err(SandboxError::from)?;

    for rule in policy.rules() {
        let fd = match PathFd::new(&rule.path) {
            Ok(fd) => fd,
            Err(e) => {
                if rule.optional {
                    continue;
                }
                return Err(SandboxError::MissingPath {
                    path: rule.path.clone(),
                    reason: e.to_string(),
                });
            }
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, rule.access.to_access()))
            .map_err(SandboxError::from)?;
    }

    let status = ruleset.restrict_self().map_err(SandboxError::from)?;
    Ok(match status.ruleset {
        RulesetStatus::FullyEnforced => Enforcement::Full,
        RulesetStatus::PartiallyEnforced => Enforcement::Partial,
        RulesetStatus::NotEnforced => Enforcement::None,
    })
}

/// How much of the policy the running kernel actually applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    Full,
    /// The kernel understood some of the requested access rights but not all.
    Partial,
    /// The kernel has no Landlock support. Nothing is confined.
    None,
}

impl Enforcement {
    pub fn as_str(&self) -> &'static str {
        match self {
            Enforcement::Full => "full",
            Enforcement::Partial => "partial",
            Enforcement::None => "none",
        }
    }

    /// Whether the engine is meaningfully confined.
    pub fn is_confined(&self) -> bool {
        matches!(self, Enforcement::Full | Enforcement::Partial)
    }
}

#[derive(Debug)]
pub enum SandboxError {
    /// A path the policy requires does not exist or cannot be opened.
    MissingPath {
        path: PathBuf,
        reason: String,
    },
    Ruleset(String),
}

impl From<RulesetError> for SandboxError {
    fn from(e: RulesetError) -> Self {
        SandboxError::Ruleset(e.to_string())
    }
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::MissingPath { path, reason } => write!(
                f,
                "the sandbox policy needs {} and it cannot be opened: {reason}",
                path.display()
            ),
            SandboxError::Ruleset(e) => write!(f, "cannot build the Landlock ruleset: {e}"),
        }
    }
}

impl std::error::Error for SandboxError {}

#[cfg(test)]
mod tests {
    use super::*;

    // Note what is *not* tested here: applying a ruleset. Landlock cannot be undone, so a
    // test that called `restrict` would confine the test runner for every test after it,
    // in an order-dependent way. Enforcement is proven end to end instead, against a real
    // engine, in tests/end_to_end.rs.

    fn policy() -> Policy {
        Policy {
            engine: PathBuf::from("/usr/lib/otwono/ai/llama.cpp/cpu/bin/llama-server"),
            model_dir: PathBuf::from("/var/lib/otwono/models/blobs"),
            runtime_dir: PathBuf::from("/run/otwono/ai"),
        }
    }

    fn allowed(p: &Policy, path: &str) -> Option<BitFlags> {
        p.rules()
            .into_iter()
            .find(|r| r.path == Path::new(path))
            .map(|r| r.access)
    }

    #[test]
    fn the_secrets_this_exists_to_protect_are_not_in_the_policy() {
        // The headline property. If any of these ever appears, the boundary is gone and
        // the rest of this file is decoration.
        let p = policy();
        for secret in [
            "/var/lib/otwono/identity",
            "/var/lib/otwono",
            "/etc/otwono",
            "/etc/otwono/publishers.d",
            "/var/log/otwono",
            "/etc",
            "/root",
            "/home",
            "/",
        ] {
            assert!(
                allowed(&p, secret).is_none(),
                "{secret} must not be reachable by the inference engine"
            );
        }
    }

    #[test]
    fn the_model_store_is_readable_and_not_writable() {
        // An engine that could write the blob store could replace a verified model with
        // its own, and the next load would trust it.
        assert_eq!(
            allowed(&policy(), "/var/lib/otwono/models/blobs"),
            Some(BitFlags::ReadOnly)
        );
    }

    #[test]
    fn the_runtime_directory_is_the_only_writable_path() {
        let p = policy();
        let writable: Vec<_> = p
            .rules()
            .into_iter()
            .filter(|r| r.access == BitFlags::ReadWrite)
            .map(|r| r.path)
            .collect();
        assert_eq!(writable, vec![PathBuf::from("/run/otwono/ai")]);
    }

    #[test]
    fn the_engines_own_directory_is_executable() {
        // Its parent, not the file: a backend is often a wrapper script beside the binary.
        assert_eq!(
            allowed(&policy(), "/usr/lib/otwono/ai/llama.cpp/cpu/bin"),
            Some(BitFlags::ReadExecute)
        );
    }

    #[test]
    fn system_library_paths_are_optional_but_the_configured_ones_are_not() {
        // /lib64 is absent on plenty of systems; a missing model directory is a mistake.
        let p = policy();
        let by_path = |path: &str| p.rules().into_iter().find(|r| r.path == Path::new(path)).unwrap();
        assert!(by_path("/lib64").optional);
        assert!(by_path("/usr").optional);
        assert!(!by_path("/var/lib/otwono/models/blobs").optional);
        assert!(!by_path("/run/otwono/ai").optional);
    }

    #[test]
    fn a_model_outside_the_store_is_not_permitted() {
        let dir = std::env::temp_dir().join(format!("otwono-sbx-{}", std::process::id()));
        let blobs = dir.join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join("inside"), b"x").unwrap();
        std::fs::write(dir.join("outside"), b"x").unwrap();

        let p = Policy {
            engine: PathBuf::from("/bin/sh"),
            model_dir: blobs.clone(),
            runtime_dir: dir.clone(),
        };
        assert!(p.permits_model(&blobs.join("inside")));
        assert!(!p.permits_model(&dir.join("outside")));
        // And the obvious escape, which is why the comparison canonicalizes.
        assert!(!p.permits_model(&blobs.join("../outside")));
        // A path that does not exist is not permitted either: it cannot be canonicalized,
        // so it cannot be shown to be inside.
        assert!(!p.permits_model(&blobs.join("absent")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enforcement_levels_report_whether_anything_is_confined() {
        assert!(Enforcement::Full.is_confined());
        assert!(Enforcement::Partial.is_confined());
        assert!(!Enforcement::None.is_confined());
        assert_eq!(Enforcement::None.as_str(), "none");
    }
}

//! Where user data lives, and the rule that application code never writes
//! anywhere else.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Environment variable used by tests and by portable installations.
pub const DATA_DIR_ENV: &str = "OTWONO_DATA_DIR";

/// Root directory for everything the user owns: database, backups, attachments,
/// project artefacts, exports and the encrypted vault fallback.
///
/// * Windows: `%APPDATA%\OTWONO AI`
/// * macOS: `~/Library/Application Support/OTWONO AI`
/// * Linux: `~/.local/share/otwono-ai`
pub fn data_dir() -> Result<PathBuf> {
    if let Some(raw) = std::env::var_os(DATA_DIR_ENV) {
        let path = PathBuf::from(raw);
        std::fs::create_dir_all(&path)
            .with_context(|| format!("creating data directory {}", path.display()))?;
        return Ok(path);
    }
    let dirs = directories::ProjectDirs::from("com", "OTWONO", "OTWONO AI")
        .ok_or_else(|| anyhow!("could not determine a home directory for this user"))?;
    let path = dirs.data_dir().to_path_buf();
    std::fs::create_dir_all(&path)
        .with_context(|| format!("creating data directory {}", path.display()))?;
    Ok(path)
}

pub fn database_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("otwono.sqlite3"))
}

fn subdir(name: &str) -> Result<PathBuf> {
    let path = data_dir()?.join(name);
    std::fs::create_dir_all(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    Ok(path)
}

/// Timestamped database copies taken before schema changes.
pub fn backups_dir() -> Result<PathBuf> {
    subdir("backups")
}

/// Files the user attached to a conversation, copied in so history is stable.
pub fn attachments_dir() -> Result<PathBuf> {
    subdir("attachments")
}

/// Per-project output directory; the only place `file_write` may target.
pub fn project_artifacts_dir(project_id: &str) -> Result<PathBuf> {
    if project_id.is_empty() || project_id.contains(['/', '\\', '.']) {
        return Err(anyhow!("refusing suspicious project id {project_id:?}"));
    }
    let path = subdir("projects")?.join(project_id);
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

/// The runtime handshake file the desktop shell reads to reach the service.
pub fn runtime_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("runtime.json"))
}

/// Encrypted vault used only when no OS credential store is available.
pub fn vault_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("vault.bin"))
}

pub fn vault_key_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("vault.key"))
}

/// Restrict a file to the current user where the platform supports it.
pub fn restrict_to_owner(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    {
        // On Windows the data directory already sits under the user's roaming
        // profile, which is not readable by other standard users.
        let _ = path;
    }
    Ok(())
}

/// True when `candidate` is inside `root` after both are canonicalised.
/// Used by every filesystem capability so that `..` cannot escape a grant.
pub fn is_within(root: &Path, candidate: &Path) -> Result<bool> {
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    let candidate = candidate
        .canonicalize()
        .with_context(|| format!("resolving {}", candidate.display()))?;
    Ok(candidate.starts_with(&root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_is_honoured() {
        let tmp = tempfile::tempdir().unwrap();
        temp_env(tmp.path(), || {
            assert_eq!(data_dir().unwrap(), tmp.path());
            assert_eq!(database_path().unwrap(), tmp.path().join("otwono.sqlite3"));
        });
    }

    #[test]
    fn project_ids_cannot_traverse_directories() {
        let tmp = tempfile::tempdir().unwrap();
        temp_env(tmp.path(), || {
            assert!(project_artifacts_dir("../escape").is_err());
            assert!(project_artifacts_dir("a/b").is_err());
            assert!(project_artifacts_dir("").is_err());
            assert!(project_artifacts_dir("prj_abc123").is_ok());
        });
    }

    #[test]
    fn containment_check_rejects_parent_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let inside = tmp.path().join("inside");
        std::fs::create_dir_all(&inside).unwrap();
        let file = inside.join("f.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(is_within(tmp.path(), &file).unwrap());

        let outside = tempfile::tempdir().unwrap();
        let other = outside.path().join("g.txt");
        std::fs::write(&other, "x").unwrap();
        assert!(!is_within(&inside, &other).unwrap());
    }

    /// `set_var` is process-global; serialise the tests that use it.
    pub(crate) fn temp_env(dir: &std::path::Path, f: impl FnOnce()) {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os(DATA_DIR_ENV);
        std::env::set_var(DATA_DIR_ENV, dir);
        f();
        match previous {
            Some(value) => std::env::set_var(DATA_DIR_ENV, value),
            None => std::env::remove_var(DATA_DIR_ENV),
        }
    }
}

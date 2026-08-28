//! Connection pool and the entry point every repository is built on.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;

use crate::migrations::{self, MigrationOutcome};

pub type Conn = PooledConnection<SqliteConnectionManager>;

/// Pragmas applied to every pooled connection. Foreign keys are enforced (they
/// are off by default in SQLite), WAL keeps readers from blocking the writer,
/// and a busy timeout turns lock contention into a wait rather than an error.
fn configure(conn: &Connection, in_memory: bool) -> rusqlite::Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    if !in_memory {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct Db {
    pool: Pool<SqliteConnectionManager>,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").field("path", &self.path).finish()
    }
}

impl Db {
    /// Open (creating if needed) the database at `path`, run migrations, and
    /// take a pre-migration backup into `backups_dir` when a schema change is
    /// about to happen.
    pub fn open(path: &Path, backups_dir: &Path) -> Result<(Self, MigrationOutcome)> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let mut bootstrap = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        configure(&bootstrap, false)?;
        let outcome = migrations::migrate(&mut bootstrap, Some(path), Some(backups_dir))?;
        bootstrap.close().map_err(|(_, e)| e)?;
        crate::paths::restrict_to_owner(path).ok();

        let manager = SqliteConnectionManager::file(path)
            .with_init(|conn| configure(conn, false));
        let pool = Pool::builder()
            .max_size(8)
            .build(manager)
            .context("building the database connection pool")?;

        Ok((Self { pool, path: Some(path.to_path_buf()) }, outcome))
    }

    /// Open the database at the standard data-directory location.
    pub fn open_default() -> Result<(Self, MigrationOutcome)> {
        Self::open(&crate::paths::database_path()?, &crate::paths::backups_dir()?)
    }

    /// A migrated, isolated in-memory database. Used by tests.
    pub fn open_in_memory() -> Result<Self> {
        let manager = SqliteConnectionManager::memory().with_init(|conn| configure(conn, true));
        // One connection: a `:memory:` database is per-connection, so the pool
        // must not hand out a second, empty one.
        let pool = Pool::builder().max_size(1).build(manager)?;
        {
            let mut conn = pool.get()?;
            migrations::migrate(&mut conn, None, None)?;
        }
        Ok(Self { pool, path: None })
    }

    pub fn conn(&self) -> Result<Conn> {
        self.pool.get().context("checking out a database connection")
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn schema_version(&self) -> Result<i64> {
        let conn = self.conn()?;
        migrations::current_version(&conn)
    }

    /// Run `f` inside a transaction, committing on `Ok` and rolling back on
    /// `Err`. Every multi-row write in this crate goes through here.
    pub fn transaction<T>(&self, f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// Take an on-demand backup, e.g. before an import or on the user's request.
    pub fn backup_now(&self, backups_dir: &Path, label: &str) -> Result<PathBuf> {
        std::fs::create_dir_all(backups_dir)?;
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let safe_label: String = label
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let target = backups_dir.join(format!("otwono-{safe_label}-{stamp}.sqlite3"));
        let source = self.conn()?;
        let mut destination = Connection::open(&target)?;
        let backup = rusqlite::backup::Backup::new(&source, &mut destination)?;
        backup.run_to_completion(64, std::time::Duration::from_millis(10), None)?;
        drop(backup);
        destination.close().map_err(|(_, e)| e)?;
        crate::paths::restrict_to_owner(&target).ok();
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_in_memory_database_is_migrated_and_usable() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), migrations::target_version());
        db.conn()
            .unwrap()
            .execute(
                "INSERT INTO settings (key, value, updated_at) VALUES ('a', 'b', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let db = Db::open_in_memory().unwrap();
        let err = db
            .conn()
            .unwrap()
            .execute(
                "INSERT INTO messages (id, conversation_id, role, content, ordinal, created_at)
                 VALUES ('m1', 'does_not_exist', 'user', 'hi', 0, '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("foreign key"), "{err}");
    }

    #[test]
    fn a_failed_transaction_writes_nothing() {
        let db = Db::open_in_memory().unwrap();
        let result: Result<()> = db.transaction(|tx| {
            tx.execute(
                "INSERT INTO settings (key, value, updated_at) VALUES ('x', '1', '2026-01-01T00:00:00Z')",
                [],
            )?;
            anyhow::bail!("deliberate failure")
        });
        assert!(result.is_err());
        let count: i64 = db
            .conn()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM settings WHERE key='x'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn a_file_database_survives_reopening_and_can_be_backed_up() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("otwono.sqlite3");
        let backups = tmp.path().join("backups");

        {
            let (db, outcome) = Db::open(&db_path, &backups).unwrap();
            assert_eq!(outcome.from, 0);
            db.conn()
                .unwrap()
                .execute(
                    "INSERT INTO settings (key, value, updated_at) VALUES ('accent', 'amber', '2026-01-01T00:00:00Z')",
                    [],
                )
                .unwrap();
            let backup = db.backup_now(&backups, "manual").unwrap();
            assert!(backup.exists());
        }

        let (db, outcome) = Db::open(&db_path, &backups).unwrap();
        assert!(outcome.applied.is_empty(), "reopening must not re-run migrations");
        let accent: String = db
            .conn()
            .unwrap()
            .query_row("SELECT value FROM settings WHERE key='accent'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(accent, "amber");
    }
}

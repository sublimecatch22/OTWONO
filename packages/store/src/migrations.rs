//! Ordered, embedded schema migrations.
//!
//! Rules this module enforces:
//! * migrations run in a single transaction each — a failure leaves the
//!   database exactly as it was;
//! * a timestamped copy of the database is taken **before** the first
//!   migration of a run, so a bad upgrade is always recoverable;
//! * a database newer than the binary is refused rather than downgraded.

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "core",
        sql: include_str!("../migrations/0001_core.sql"),
    },
    Migration {
        version: 2,
        name: "marketplace_and_account",
        sql: include_str!("../migrations/0002_marketplace_and_account.sql"),
    },
];

/// Schema version this binary expects once migrations have run.
pub fn target_version() -> i64 {
    MIGRATIONS.iter().map(|m| m.version).max().unwrap_or(0)
}

fn ensure_migrations_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            applied_at TEXT NOT NULL
         ) STRICT;",
    )?;
    Ok(())
}

pub fn current_version(conn: &Connection) -> Result<i64> {
    ensure_migrations_table(conn)?;
    let version: Option<i64> = conn.query_row(
        "SELECT MAX(version) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    Ok(version.unwrap_or(0))
}

/// Copy the database file next to itself under `backups/`, named for the
/// schema version it is leaving. Returns `None` when there is nothing to back
/// up (a database that has never been migrated).
pub fn backup_before_migration(db_path: &Path, backups_dir: &Path, from: i64) -> Result<Option<PathBuf>> {
    if from == 0 || !db_path.exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(backups_dir)?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let target = backups_dir.join(format!("otwono-v{from}-{stamp}.sqlite3"));

    // Use SQLite's own backup API rather than a file copy so that a
    // concurrently-open write-ahead log is included.
    let source = Connection::open(db_path)?;
    let mut destination = Connection::open(&target)?;
    let backup = rusqlite::backup::Backup::new(&source, &mut destination)?;
    backup
        .run_to_completion(64, std::time::Duration::from_millis(10), None)
        .context("copying database for pre-migration backup")?;
    drop(backup);
    destination.close().map_err(|(_, e)| e)?;
    crate::paths::restrict_to_owner(&target).ok();
    Ok(Some(target))
}

#[derive(Debug, Clone)]
pub struct MigrationOutcome {
    pub from: i64,
    pub to: i64,
    pub applied: Vec<&'static str>,
    pub backup: Option<PathBuf>,
}

/// Bring `conn` up to `target_version()`. Idempotent.
pub fn migrate(conn: &mut Connection, db_path: Option<&Path>, backups_dir: Option<&Path>) -> Result<MigrationOutcome> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let from = current_version(conn)?;
    let to = target_version();

    if from > to {
        bail!(
            "this database is at schema version {from} but this build of OTWONO AI understands \
             only version {to}. Install the newer version of the application, or restore a \
             backup from the backups folder."
        );
    }

    let pending: Vec<&Migration> = MIGRATIONS.iter().filter(|m| m.version > from).collect();
    if pending.is_empty() {
        return Ok(MigrationOutcome { from, to, applied: Vec::new(), backup: None });
    }

    let backup = match (db_path, backups_dir) {
        (Some(db), Some(dir)) => backup_before_migration(db, dir, from)?,
        _ => None,
    };

    let mut applied = Vec::new();
    for migration in pending {
        let tx = conn.transaction()?;
        tx.execute_batch(migration.sql)
            .with_context(|| format!("applying migration {:04}_{}", migration.version, migration.name))?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                migration.version,
                migration.name,
                otwono_types::ids::format_ts(&otwono_types::now())
            ],
        )?;
        tx.commit()?;
        applied.push(migration.name);
        tracing::info!(version = migration.version, name = migration.name, "applied migration");
    }

    let reached = current_version(conn)?;
    if reached != to {
        return Err(anyhow!("migration finished at version {reached}, expected {to}"));
    }

    Ok(MigrationOutcome { from, to, applied, backup })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn migrations_are_ordered_and_unique() {
        let mut previous = 0;
        for migration in MIGRATIONS {
            assert!(
                migration.version > previous,
                "migration versions must strictly increase; {} follows {previous}",
                migration.version
            );
            previous = migration.version;
        }
    }

    #[test]
    fn a_fresh_database_reaches_the_target_version() {
        let mut conn = memory();
        let outcome = migrate(&mut conn, None, None).unwrap();
        assert_eq!(outcome.from, 0);
        assert_eq!(outcome.to, target_version());
        assert_eq!(outcome.applied.len(), MIGRATIONS.len());
        assert_eq!(current_version(&conn).unwrap(), target_version());
    }

    #[test]
    fn migrating_twice_changes_nothing() {
        let mut conn = memory();
        migrate(&mut conn, None, None).unwrap();
        let second = migrate(&mut conn, None, None).unwrap();
        assert!(second.applied.is_empty());
        assert_eq!(current_version(&conn).unwrap(), target_version());
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused_not_downgraded() {
        let mut conn = memory();
        migrate(&mut conn, None, None).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, 'future', '2030-01-01T00:00:00Z')",
            [target_version() + 5],
        )
        .unwrap();
        let err = migrate(&mut conn, None, None).unwrap_err().to_string();
        assert!(err.contains("newer version"), "unhelpful error: {err}");
    }

    #[test]
    fn a_failing_migration_leaves_the_schema_untouched() {
        let mut conn = memory();
        migrate(&mut conn, None, None).unwrap();
        let before = current_version(&conn).unwrap();

        // Simulate a broken migration body applied through the same path.
        let tx = conn.transaction().unwrap();
        let result = tx.execute_batch("CREATE TABLE ok_table (id TEXT); SELECT this_is_not_valid();");
        assert!(result.is_err());
        drop(tx); // rolled back

        assert_eq!(current_version(&conn).unwrap(), before);
        let table_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='ok_table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_exists, 0, "partial migration must not survive");
    }

    #[test]
    fn a_backup_is_written_before_an_upgrade_and_is_readable() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("otwono.sqlite3");
        let backups = tmp.path().join("backups");

        // Start at version 1 only.
        {
            let mut conn = Connection::open(&db_path).unwrap();
            ensure_migrations_table(&conn).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(MIGRATIONS[0].sql).unwrap();
            tx.execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (1, 'core', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
            tx.commit().unwrap();
            conn.execute(
                "INSERT INTO settings (key, value, updated_at) VALUES ('theme', 'dark', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        }

        let mut conn = Connection::open(&db_path).unwrap();
        let outcome = migrate(&mut conn, Some(&db_path), Some(&backups)).unwrap();
        assert_eq!(outcome.from, 1);
        assert_eq!(outcome.to, target_version());
        let backup = outcome.backup.expect("a backup should have been taken");
        assert!(backup.exists());

        // The backup still holds the user's data at the pre-upgrade version.
        let restored = Connection::open(&backup).unwrap();
        let theme: String = restored
            .query_row("SELECT value FROM settings WHERE key='theme'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(theme, "dark");
        assert_eq!(current_version(&restored).unwrap(), 1);

        // And the live database kept the data through the upgrade.
        let theme: String = conn
            .query_row("SELECT value FROM settings WHERE key='theme'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(theme, "dark");
    }

    #[test]
    fn no_backup_is_taken_for_a_brand_new_database() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("otwono.sqlite3");
        let backups = tmp.path().join("backups");
        let mut conn = Connection::open(&db_path).unwrap();
        let outcome = migrate(&mut conn, Some(&db_path), Some(&backups)).unwrap();
        assert!(outcome.backup.is_none());
    }

    #[test]
    fn the_simulated_ledger_cannot_record_a_real_payment() {
        let mut conn = memory();
        migrate(&mut conn, None, None).unwrap();
        conn.execute_batch(
            "INSERT INTO listings (id, creator_account_id, title, created_at, updated_at)
             VALUES ('lst_1', 'acc_1', 'Test', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');",
        )
        .unwrap();
        let err = conn
            .execute(
                "INSERT INTO marketplace_ledger (id, listing_id, entry_type, amount_minor, account_id, simulated, created_at)
                 VALUES ('led_1', 'lst_1', 'payout', 100, 'acc_1', 0, '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("constraint"), "{err}");
    }
}

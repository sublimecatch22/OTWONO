//! The relay's own database.
//!
//! It holds accounts, profiles, revocable tokens and the metadata a user has
//! explicitly chosen to synchronise. It never holds a prompt, a file, a
//! knowledge index, or a model.

use anyhow::{Context, Result};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
  id             TEXT PRIMARY KEY,
  email          TEXT NOT NULL UNIQUE,
  password_hash  TEXT NOT NULL,
  display_name   TEXT NOT NULL DEFAULT '',
  email_verified INTEGER NOT NULL DEFAULT 0,
  verification_token TEXT,
  reset_token    TEXT,
  reset_expires_at TEXT,
  created_at     TEXT NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS profiles (
  account_id     TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  display_name   TEXT NOT NULL DEFAULT '',
  biography      TEXT NOT NULL DEFAULT '',
  interests      TEXT NOT NULL DEFAULT '[]',
  capabilities   TEXT NOT NULL DEFAULT '[]',
  portfolio_links TEXT NOT NULL DEFAULT '[]',
  avatar_url     TEXT,
  -- Which fields other people may see. Everything defaults to private.
  visibility     TEXT NOT NULL DEFAULT '{}',
  -- Set when this profile represents an AI identity rather than a person.
  is_ai_identity INTEGER NOT NULL DEFAULT 0,
  owner_account_id TEXT REFERENCES accounts(id) ON DELETE SET NULL,
  updated_at     TEXT NOT NULL
) STRICT;

-- Device and site sessions. The token itself is stored only as a hash.
CREATE TABLE IF NOT EXISTS tokens (
  id          TEXT PRIMARY KEY,
  account_id  TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  token_hash  TEXT NOT NULL UNIQUE,
  kind        TEXT NOT NULL,
  label       TEXT NOT NULL DEFAULT '',
  scopes      TEXT NOT NULL DEFAULT '[]',
  created_at  TEXT NOT NULL,
  last_used_at TEXT,
  expires_at  TEXT,
  revoked_at  TEXT
) STRICT;
CREATE INDEX IF NOT EXISTS idx_tokens_account ON tokens(account_id, revoked_at);

-- Pairing codes minted by a desktop app for a site to redeem.
CREATE TABLE IF NOT EXISTS pairings (
  code_hash   TEXT PRIMARY KEY,
  account_id  TEXT REFERENCES accounts(id) ON DELETE CASCADE,
  scopes      TEXT NOT NULL DEFAULT '[]',
  site        TEXT,
  created_at  TEXT NOT NULL,
  expires_at  TEXT NOT NULL,
  consumed_at TEXT
) STRICT;

-- Project metadata a user chose to synchronise. Titles and states only.
CREATE TABLE IF NOT EXISTS synced_projects (
  id          TEXT PRIMARY KEY,
  account_id  TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  title       TEXT NOT NULL,
  state       TEXT NOT NULL,
  task_count  INTEGER NOT NULL DEFAULT 0,
  completed_tasks INTEGER NOT NULL DEFAULT 0,
  updated_at  TEXT NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS audit (
  id         TEXT PRIMARY KEY,
  account_id TEXT,
  action     TEXT NOT NULL,
  detail     TEXT NOT NULL DEFAULT '{}',
  ip_prefix  TEXT,
  created_at TEXT NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS rate_limits (
  bucket       TEXT NOT NULL,
  window_start TEXT NOT NULL,
  count        INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (bucket, window_start)
) STRICT;
"#;

#[derive(Clone)]
pub struct RelayDb {
    pool: Pool<SqliteConnectionManager>,
}

fn configure(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    Ok(())
}

impl RelayDb {
    pub fn open(path: &str) -> Result<Self> {
        let manager = SqliteConnectionManager::file(path).with_init(configure);
        let pool = Pool::builder().max_size(8).build(manager)?;
        pool.get()?
            .execute_batch(SCHEMA)
            .context("creating the relay schema")?;
        Ok(Self { pool })
    }

    pub fn open_in_memory() -> Result<Self> {
        let manager = SqliteConnectionManager::memory().with_init(configure);
        let pool = Pool::builder().max_size(1).build(manager)?;
        pool.get()?.execute_batch(SCHEMA)?;
        Ok(Self { pool })
    }

    pub fn conn(&self) -> Result<r2d2::PooledConnection<SqliteConnectionManager>> {
        Ok(self.pool.get()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_relay_schema_has_nowhere_to_put_a_prompt_or_a_file() {
        let db = RelayDb::open_in_memory().unwrap();
        let conn = db.conn().unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        for forbidden in [
            "messages",
            "conversations",
            "documents",
            "chunks",
            "knowledge_sources",
        ] {
            assert!(
                !tables.iter().any(|table| table == forbidden),
                "the relay must not have a {forbidden} table"
            );
        }
        assert!(tables.iter().any(|table| table == "accounts"));
        assert!(tables.iter().any(|table| table == "synced_projects"));
    }

    #[test]
    fn synchronised_projects_carry_no_content_columns() {
        let db = RelayDb::open_in_memory().unwrap();
        let conn = db.conn().unwrap();
        let mut stmt = conn.prepare("PRAGMA table_info(synced_projects)").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        for forbidden in ["objective", "output", "instructions", "content", "body"] {
            assert!(
                !columns.iter().any(|column| column == forbidden),
                "synced_projects must not have a {forbidden} column"
            );
        }
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let db = RelayDb::open_in_memory().unwrap();
        let error = db
            .conn()
            .unwrap()
            .execute(
                "INSERT INTO profiles (account_id, updated_at) VALUES ('nope', '2026-01-01')",
                [],
            )
            .unwrap_err();
        assert!(error.to_string().to_lowercase().contains("foreign key"));
    }
}

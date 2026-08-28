//! Provider connections.
//!
//! The credential for a connection is *not* stored here. `has_credential` is a
//! flag maintained alongside the secret store; the value only ever lives in the
//! vault.

use anyhow::Result;
use rusqlite::{params, OptionalExtension, Row};

use otwono_types::provider::{ProviderConnection, ProviderKind};

use crate::Db;

fn map(row: &Row<'_>) -> rusqlite::Result<ProviderConnection> {
    Ok(ProviderConnection {
        id: row.get(0)?,
        kind: ProviderKind::parse(&row.get::<_, String>(1)?)
            .unwrap_or(ProviderKind::OpenAiCompatible),
        label: row.get(2)?,
        endpoint: row.get(3)?,
        enabled: row.get::<_, i64>(4)? != 0,
        has_credential: row.get::<_, i64>(5)? != 0,
        default_model: row.get(6)?,
        default_embedding_model: row.get(7)?,
    })
}

const COLUMNS: &str = "id, kind, label, endpoint, enabled, has_credential, default_model, \
                       default_embedding_model";

pub struct ProviderRepo<'a> {
    db: &'a Db,
}

#[derive(Debug, Clone)]
pub struct NewProvider {
    pub kind: ProviderKind,
    pub label: String,
    pub endpoint: String,
    pub default_model: Option<String>,
    pub default_embedding_model: Option<String>,
    /// Ignored for connections that require a credential they do not have.
    pub enabled: bool,
}

impl<'a> ProviderRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    pub fn create(&self, new: NewProvider) -> Result<ProviderConnection> {
        let id = otwono_types::new_id("prv");
        let now = crate::now_str();
        // A connection that points off-device stays disabled until a credential
        // is supplied, whatever the caller asked for.
        let enabled = new.enabled && new.kind.is_local_by_default();
        self.db.conn()?.execute(
            "INSERT INTO provider_connections
               (id, kind, label, endpoint, enabled, has_credential, default_model,
                default_embedding_model, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8, ?8)",
            params![
                id,
                new.kind.as_str(),
                new.label,
                new.endpoint.trim_end_matches('/'),
                enabled as i64,
                new.default_model,
                new.default_embedding_model,
                now
            ],
        )?;
        self.get(&id)?
            .ok_or_else(|| anyhow::anyhow!("connection vanished immediately after creation"))
    }

    pub fn get(&self, id: &str) -> Result<Option<ProviderConnection>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM provider_connections WHERE id = ?1"),
                [id],
                map,
            )
            .optional()?)
    }

    pub fn find_by_endpoint(&self, endpoint: &str) -> Result<Option<ProviderConnection>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM provider_connections WHERE endpoint = ?1"),
                [endpoint.trim_end_matches('/')],
                map,
            )
            .optional()?)
    }

    pub fn list(&self) -> Result<Vec<ProviderConnection>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM provider_connections ORDER BY created_at"
        ))?;
        let rows = stmt.query_map([], map)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn update(&self, connection: &ProviderConnection) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE provider_connections
                SET label = ?2, endpoint = ?3, enabled = ?4, default_model = ?5,
                    default_embedding_model = ?6, updated_at = ?7
              WHERE id = ?1",
            params![
                connection.id,
                connection.label,
                connection.endpoint.trim_end_matches('/'),
                (connection.enabled
                    && (connection.kind.is_local_by_default() || connection.has_credential))
                    as i64,
                connection.default_model,
                connection.default_embedding_model,
                crate::now_str(),
            ],
        )?;
        Ok(())
    }

    /// Record that a credential now exists (or no longer does) for this
    /// connection. Called by the service immediately after the secret store
    /// write succeeds, never before.
    pub fn set_has_credential(&self, id: &str, has_credential: bool) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE provider_connections SET has_credential = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, has_credential as i64, crate::now_str()],
        )?;
        if !has_credential {
            // Losing the credential disables an off-device connection.
            self.db.conn()?.execute(
                "UPDATE provider_connections SET enabled = 0
                  WHERE id = ?1 AND kind = 'openai_compatible'",
                [id],
            )?;
        }
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.db
            .conn()?
            .execute("DELETE FROM provider_connections WHERE id = ?1", [id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_local() -> NewProvider {
        NewProvider {
            kind: ProviderKind::Ollama,
            label: "Ollama".into(),
            endpoint: "http://127.0.0.1:11434/".into(),
            default_model: None,
            default_embedding_model: None,
            enabled: true,
        }
    }

    #[test]
    fn a_local_connection_can_be_enabled_immediately() {
        let db = Db::open_in_memory().unwrap();
        let repo = ProviderRepo::new(&db);
        let created = repo.create(new_local()).unwrap();
        assert!(created.enabled);
        assert!(!created.has_credential);
        assert_eq!(
            created.endpoint, "http://127.0.0.1:11434",
            "trailing slash trimmed"
        );
    }

    #[test]
    fn an_online_connection_stays_disabled_until_a_credential_exists() {
        let db = Db::open_in_memory().unwrap();
        let repo = ProviderRepo::new(&db);
        let created = repo
            .create(NewProvider {
                kind: ProviderKind::OpenAiCompatible,
                label: "Hosted".into(),
                endpoint: "https://api.example.com/v1".into(),
                default_model: None,
                default_embedding_model: None,
                enabled: true,
            })
            .unwrap();
        assert!(
            !created.enabled,
            "an online provider must not enable itself"
        );

        // Enabling it without a credential is still refused.
        let mut attempt = created.clone();
        attempt.enabled = true;
        repo.update(&attempt).unwrap();
        assert!(!repo.get(&created.id).unwrap().unwrap().enabled);

        // Once the credential is recorded, enabling works.
        repo.set_has_credential(&created.id, true).unwrap();
        let mut attempt = repo.get(&created.id).unwrap().unwrap();
        attempt.enabled = true;
        repo.update(&attempt).unwrap();
        assert!(repo.get(&created.id).unwrap().unwrap().enabled);
    }

    #[test]
    fn removing_a_credential_disables_an_online_connection() {
        let db = Db::open_in_memory().unwrap();
        let repo = ProviderRepo::new(&db);
        let created = repo
            .create(NewProvider {
                kind: ProviderKind::OpenAiCompatible,
                label: "Hosted".into(),
                endpoint: "https://api.example.com/v1".into(),
                default_model: None,
                default_embedding_model: None,
                enabled: false,
            })
            .unwrap();
        repo.set_has_credential(&created.id, true).unwrap();
        let mut enabled = repo.get(&created.id).unwrap().unwrap();
        enabled.enabled = true;
        repo.update(&enabled).unwrap();

        repo.set_has_credential(&created.id, false).unwrap();
        let after = repo.get(&created.id).unwrap().unwrap();
        assert!(!after.has_credential);
        assert!(!after.enabled);
    }

    #[test]
    fn connections_list_and_delete() {
        let db = Db::open_in_memory().unwrap();
        let repo = ProviderRepo::new(&db);
        let a = repo.create(new_local()).unwrap();
        repo.create(NewProvider {
            kind: ProviderKind::LmStudio,
            label: "LM Studio".into(),
            endpoint: "http://127.0.0.1:1234".into(),
            ..new_local()
        })
        .unwrap();
        assert_eq!(repo.list().unwrap().len(), 2);
        assert!(repo
            .find_by_endpoint("http://127.0.0.1:1234/")
            .unwrap()
            .is_some());
        repo.delete(&a.id).unwrap();
        assert_eq!(repo.list().unwrap().len(), 1);
        assert!(repo.get(&a.id).unwrap().is_none());
    }

    #[test]
    fn no_credential_column_exists_on_the_table() {
        let db = Db::open_in_memory().unwrap();
        let conn = db.conn().unwrap();
        let mut stmt = conn
            .prepare("PRAGMA table_info(provider_connections)")
            .unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for column in &columns {
            let lowered = column.to_ascii_lowercase();
            assert!(
                !(lowered.contains("api_key") || lowered == "token" || lowered.contains("secret")),
                "provider_connections must not have a column that could hold a secret: {column}"
            );
        }
        assert!(columns.contains(&"has_credential".to_string()));
    }
}

//! Storage for permission grants and the requests that produce them.
//!
//! The evaluation rules live in `otwono-permissions`; this module only reads
//! and writes rows.

use anyhow::Result;
use rusqlite::{params, OptionalExtension, Row};

use otwono_types::permission::{Capability, Decision, Grant, PermissionRequest, Scope};

use crate::Db;

const GRANT_COLUMNS: &str = "id, capability, scopes, decision, spend_limit_minor, \
    spend_category, expires_at, revoked_at, created_at, created_by, note";

const REQUEST_COLUMNS: &str = "id, capability, scopes, summary, requested_by_agent_id, \
    project_id, task_id, created_at, resolved_at, resolution";

fn decision_str(decision: Decision) -> &'static str {
    match decision {
        Decision::AllowOnce => "allow_once",
        Decision::Allow => "allow",
        Decision::Deny => "deny",
    }
}

fn parse_decision(value: &str) -> Decision {
    match value {
        "allow_once" => Decision::AllowOnce,
        "allow" => Decision::Allow,
        _ => Decision::Deny,
    }
}

fn map_grant(row: &Row<'_>) -> rusqlite::Result<Grant> {
    Ok(Grant {
        id: row.get(0)?,
        capability: Capability::parse(&row.get::<_, String>(1)?)
            .unwrap_or(Capability::KnowledgeSearch),
        scopes: crate::json_column(row.get(2)?),
        decision: parse_decision(&row.get::<_, String>(3)?),
        spend_limit_minor: row.get(4)?,
        spend_category: row.get(5)?,
        expires_at: crate::parse_ts_opt(row.get(6)?),
        revoked_at: crate::parse_ts_opt(row.get(7)?),
        created_at: crate::parse_ts(&row.get::<_, String>(8)?),
        created_by: row.get(9)?,
        note: row.get(10)?,
    })
}

fn map_request(row: &Row<'_>) -> rusqlite::Result<PermissionRequest> {
    Ok(PermissionRequest {
        id: row.get(0)?,
        capability: Capability::parse(&row.get::<_, String>(1)?)
            .unwrap_or(Capability::KnowledgeSearch),
        scopes: crate::json_column(row.get(2)?),
        summary: row.get(3)?,
        requested_by_agent_id: row.get(4)?,
        project_id: row.get(5)?,
        task_id: row.get(6)?,
        created_at: crate::parse_ts(&row.get::<_, String>(7)?),
        resolved_at: crate::parse_ts_opt(row.get(8)?),
        resolution: row.get::<_, Option<String>>(9)?.map(|v| parse_decision(&v)),
    })
}

#[derive(Debug, Clone)]
pub struct NewGrant {
    pub capability: Capability,
    pub scopes: Vec<Scope>,
    pub decision: Decision,
    pub spend_limit_minor: Option<i64>,
    pub spend_category: Option<String>,
    pub expires_at: Option<otwono_types::Timestamp>,
    pub created_by: String,
    pub note: Option<String>,
}

pub struct PermissionRepo<'a> {
    db: &'a Db,
}

impl<'a> PermissionRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    pub fn grant(&self, new: NewGrant) -> Result<Grant> {
        let id = otwono_types::new_id("grn");
        self.db.conn()?.execute(
            "INSERT INTO permission_grants
               (id, capability, scopes, decision, spend_limit_minor, spend_category,
                expires_at, created_at, created_by, note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id,
                new.capability.as_str(),
                crate::to_json(&new.scopes),
                decision_str(new.decision),
                new.spend_limit_minor,
                new.spend_category,
                new.expires_at.as_ref().map(otwono_types::ids::format_ts),
                crate::now_str(),
                new.created_by,
                new.note
            ],
        )?;
        self.get_grant(&id)?
            .ok_or_else(|| anyhow::anyhow!("grant not found after creation"))
    }

    pub fn get_grant(&self, id: &str) -> Result<Option<Grant>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                &format!("SELECT {GRANT_COLUMNS} FROM permission_grants WHERE id = ?1"),
                [id],
                map_grant,
            )
            .optional()?)
    }

    /// Grants that are still in force: not revoked and not expired.
    pub fn active_grants(&self) -> Result<Vec<Grant>> {
        let conn = self.db.conn()?;
        let now = crate::now_str();
        let mut stmt = conn.prepare(&format!(
            "SELECT {GRANT_COLUMNS} FROM permission_grants
              WHERE revoked_at IS NULL AND (expires_at IS NULL OR expires_at > ?1)
              ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map([now], map_grant)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn all_grants(&self) -> Result<Vec<Grant>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {GRANT_COLUMNS} FROM permission_grants ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map([], map_grant)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn revoke(&self, grant_id: &str) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE permission_grants SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
            params![grant_id, crate::now_str()],
        )?;
        Ok(())
    }

    /// Revoke everything at once. Backs the "revoke all permissions" control
    /// that sits beside the emergency stop.
    pub fn revoke_all(&self) -> Result<usize> {
        Ok(self.db.conn()?.execute(
            "UPDATE permission_grants SET revoked_at = ?1 WHERE revoked_at IS NULL",
            [crate::now_str()],
        )?)
    }

    /// Consume a one-shot grant so it cannot be used twice.
    pub fn consume_once(&self, grant_id: &str) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE permission_grants SET revoked_at = ?2
              WHERE id = ?1 AND decision = 'allow_once' AND revoked_at IS NULL",
            params![grant_id, crate::now_str()],
        )?;
        Ok(())
    }

    // ---- requests

    pub fn open_request(
        &self,
        capability: Capability,
        scopes: &[Scope],
        summary: &str,
        agent_id: Option<&str>,
        project_id: Option<&str>,
        task_id: Option<&str>,
    ) -> Result<PermissionRequest> {
        let id = otwono_types::new_id("req");
        self.db.conn()?.execute(
            "INSERT INTO permission_requests
               (id, capability, scopes, summary, requested_by_agent_id, project_id, task_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id, capability.as_str(), crate::to_json(&scopes), summary,
                agent_id, project_id, task_id, crate::now_str()
            ],
        )?;
        self.get_request(&id)?
            .ok_or_else(|| anyhow::anyhow!("request not found after creation"))
    }

    pub fn get_request(&self, id: &str) -> Result<Option<PermissionRequest>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                &format!("SELECT {REQUEST_COLUMNS} FROM permission_requests WHERE id = ?1"),
                [id],
                map_request,
            )
            .optional()?)
    }

    pub fn open_requests(&self) -> Result<Vec<PermissionRequest>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {REQUEST_COLUMNS} FROM permission_requests
              WHERE resolved_at IS NULL ORDER BY created_at"
        ))?;
        let rows = stmt.query_map([], map_request)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Answer a request. An allow also writes the grant it implies, in one
    /// transaction, so a crash cannot leave a request answered but ungranted.
    pub fn resolve_request(
        &self,
        request_id: &str,
        decision: Decision,
        expires_at: Option<otwono_types::Timestamp>,
    ) -> Result<Option<Grant>> {
        let request = self
            .get_request(request_id)?
            .ok_or_else(|| anyhow::anyhow!("request {request_id} does not exist"))?;
        if request.resolved_at.is_some() {
            anyhow::bail!("this request has already been answered");
        }

        let grant_id = otwono_types::new_id("grn");
        let now = crate::now_str();
        let creates_grant = !matches!(decision, Decision::Deny);

        self.db.transaction(|tx| {
            tx.execute(
                "UPDATE permission_requests SET resolved_at = ?2, resolution = ?3 WHERE id = ?1",
                params![request_id, now, decision_str(decision)],
            )?;
            if creates_grant {
                tx.execute(
                    "INSERT INTO permission_grants
                       (id, capability, scopes, decision, expires_at, created_at, created_by, note)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'user', ?7)",
                    params![
                        grant_id,
                        request.capability.as_str(),
                        crate::to_json(&request.scopes),
                        decision_str(decision),
                        expires_at.as_ref().map(otwono_types::ids::format_ts),
                        now,
                        format!("answered request {request_id}")
                    ],
                )?;
            } else {
                // A denial is also recorded as a grant row, so that a later
                // check finds an explicit deny rather than falling through to
                // "ask again".
                tx.execute(
                    "INSERT INTO permission_grants
                       (id, capability, scopes, decision, created_at, created_by, note)
                     VALUES (?1, ?2, ?3, 'deny', ?4, 'user', ?5)",
                    params![
                        grant_id,
                        request.capability.as_str(),
                        crate::to_json(&request.scopes),
                        now,
                        format!("denied request {request_id}")
                    ],
                )?;
            }
            Ok(())
        })?;

        self.get_grant(&grant_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_grant(capability: Capability, decision: Decision) -> NewGrant {
        NewGrant {
            capability,
            scopes: vec![Scope::Project {
                project_id: "prj_1".into(),
            }],
            decision,
            spend_limit_minor: None,
            spend_category: None,
            expires_at: None,
            created_by: "user".into(),
            note: None,
        }
    }

    #[test]
    fn a_grant_is_active_until_it_is_revoked() {
        let db = Db::open_in_memory().unwrap();
        let repo = PermissionRepo::new(&db);
        let grant = repo
            .grant(new_grant(Capability::FileRead, Decision::Allow))
            .unwrap();
        assert_eq!(repo.active_grants().unwrap().len(), 1);

        repo.revoke(&grant.id).unwrap();
        assert!(repo.active_grants().unwrap().is_empty());
        assert_eq!(repo.all_grants().unwrap().len(), 1, "history is kept");
        assert!(repo
            .get_grant(&grant.id)
            .unwrap()
            .unwrap()
            .revoked_at
            .is_some());
    }

    #[test]
    fn an_expired_grant_stops_being_active_without_being_deleted() {
        let db = Db::open_in_memory().unwrap();
        let repo = PermissionRepo::new(&db);
        repo.grant(NewGrant {
            expires_at: Some(otwono_types::now() - chrono::Duration::minutes(1)),
            ..new_grant(Capability::HttpFetch, Decision::Allow)
        })
        .unwrap();
        repo.grant(NewGrant {
            expires_at: Some(otwono_types::now() + chrono::Duration::hours(1)),
            ..new_grant(Capability::FileRead, Decision::Allow)
        })
        .unwrap();

        let active = repo.active_grants().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].capability, Capability::FileRead);
        assert_eq!(repo.all_grants().unwrap().len(), 2);
    }

    #[test]
    fn a_one_shot_grant_can_only_be_used_once() {
        let db = Db::open_in_memory().unwrap();
        let repo = PermissionRepo::new(&db);
        let grant = repo
            .grant(new_grant(Capability::FileWrite, Decision::AllowOnce))
            .unwrap();
        assert_eq!(repo.active_grants().unwrap().len(), 1);

        repo.consume_once(&grant.id).unwrap();
        assert!(repo.active_grants().unwrap().is_empty());
    }

    #[test]
    fn consuming_does_not_touch_a_standing_grant() {
        let db = Db::open_in_memory().unwrap();
        let repo = PermissionRepo::new(&db);
        let grant = repo
            .grant(new_grant(Capability::FileRead, Decision::Allow))
            .unwrap();
        repo.consume_once(&grant.id).unwrap();
        assert_eq!(repo.active_grants().unwrap().len(), 1);
    }

    #[test]
    fn revoke_all_clears_every_standing_permission() {
        let db = Db::open_in_memory().unwrap();
        let repo = PermissionRepo::new(&db);
        repo.grant(new_grant(Capability::FileRead, Decision::Allow))
            .unwrap();
        repo.grant(new_grant(Capability::HttpFetch, Decision::Allow))
            .unwrap();
        repo.grant(new_grant(Capability::RelaySync, Decision::AllowOnce))
            .unwrap();

        assert_eq!(repo.revoke_all().unwrap(), 3);
        assert!(repo.active_grants().unwrap().is_empty());
    }

    #[test]
    fn answering_a_request_with_allow_creates_the_matching_grant() {
        let db = Db::open_in_memory().unwrap();
        let repo = PermissionRepo::new(&db);
        let request = repo
            .open_request(
                Capability::HttpFetch,
                &[Scope::Host {
                    host: "example.com".into(),
                }],
                "This agent wants to fetch a page from an approved website.",
                Some("agt_1"),
                Some("prj_1"),
                None,
            )
            .unwrap();
        assert_eq!(repo.open_requests().unwrap().len(), 1);

        let grant = repo
            .resolve_request(&request.id, Decision::Allow, None)
            .unwrap()
            .unwrap();
        assert_eq!(grant.capability, Capability::HttpFetch);
        assert_eq!(
            grant.scopes,
            vec![Scope::Host {
                host: "example.com".into()
            }]
        );
        assert!(repo.open_requests().unwrap().is_empty());
        assert_eq!(
            repo.get_request(&request.id).unwrap().unwrap().resolution,
            Some(Decision::Allow)
        );
    }

    #[test]
    fn a_denial_is_recorded_explicitly_so_it_is_not_asked_again() {
        let db = Db::open_in_memory().unwrap();
        let repo = PermissionRepo::new(&db);
        let request = repo
            .open_request(
                Capability::HttpFetch,
                &[Scope::Host {
                    host: "tracker.example".into(),
                }],
                "…",
                None,
                None,
                None,
            )
            .unwrap();
        let grant = repo
            .resolve_request(&request.id, Decision::Deny, None)
            .unwrap()
            .unwrap();
        assert_eq!(grant.decision, Decision::Deny);
        assert_eq!(repo.active_grants().unwrap().len(), 1);
    }

    #[test]
    fn a_request_cannot_be_answered_twice() {
        let db = Db::open_in_memory().unwrap();
        let repo = PermissionRepo::new(&db);
        let request = repo
            .open_request(Capability::FileRead, &[], "…", None, None, None)
            .unwrap();
        repo.resolve_request(&request.id, Decision::Allow, None)
            .unwrap();
        let err = repo
            .resolve_request(&request.id, Decision::Deny, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already been answered"), "{err}");
        assert_eq!(
            repo.all_grants().unwrap().len(),
            1,
            "no second grant was written"
        );
    }

    #[test]
    fn spend_limits_are_stored_with_the_grant() {
        let db = Db::open_in_memory().unwrap();
        let repo = PermissionRepo::new(&db);
        let grant = repo
            .grant(NewGrant {
                spend_limit_minor: Some(50_00),
                spend_category: Some("software".into()),
                ..new_grant(Capability::BudgetRecord, Decision::Allow)
            })
            .unwrap();
        let reloaded = repo.get_grant(&grant.id).unwrap().unwrap();
        assert_eq!(reloaded.spend_limit_minor, Some(50_00));
        assert_eq!(reloaded.spend_category.as_deref(), Some("software"));
    }
}

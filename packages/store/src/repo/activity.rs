//! The audit log.
//!
//! Every tool call, permission decision, provider request and state change is
//! written here with a timestamp and an actor. Values that could carry a secret
//! are redacted *before* the row is written, not when it is displayed.

use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::Db;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorType {
    User,
    Agent,
    System,
    /// A request that arrived from a paired WordPress site via the relay.
    Relay,
}

impl ActorType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::System => "system",
            Self::Relay => "relay",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Denied,
    Failed,
    /// Started but not yet finished; updated by a later entry.
    Pending,
}

impl Outcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Denied => "denied",
            Self::Failed => "failed",
            Self::Pending => "pending",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub id: String,
    pub created_at: String,
    pub actor_type: ActorType,
    pub actor_id: Option<String>,
    pub actor_name: Option<String>,
    pub action: String,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub project_id: Option<String>,
    pub task_id: Option<String>,
    pub outcome: Outcome,
    pub detail: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct NewActivity {
    pub actor_type: ActorType,
    pub actor_id: Option<String>,
    pub actor_name: Option<String>,
    pub action: String,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub project_id: Option<String>,
    pub task_id: Option<String>,
    pub outcome: Outcome,
    pub detail: serde_json::Value,
}

impl NewActivity {
    pub fn system(action: impl Into<String>) -> Self {
        Self {
            actor_type: ActorType::System,
            actor_id: None,
            actor_name: None,
            action: action.into(),
            target_type: None,
            target_id: None,
            project_id: None,
            task_id: None,
            outcome: Outcome::Ok,
            detail: serde_json::json!({}),
        }
    }

    pub fn user(action: impl Into<String>) -> Self {
        Self {
            actor_type: ActorType::User,
            ..Self::system(action)
        }
    }

    pub fn agent(
        agent_id: impl Into<String>,
        agent_name: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            actor_type: ActorType::Agent,
            actor_id: Some(agent_id.into()),
            actor_name: Some(agent_name.into()),
            ..Self::system(action)
        }
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = detail;
        self
    }

    pub fn with_target(
        mut self,
        target_type: impl Into<String>,
        target_id: impl Into<String>,
    ) -> Self {
        self.target_type = Some(target_type.into());
        self.target_id = Some(target_id.into());
        self
    }

    pub fn with_project(mut self, project_id: impl Into<String>) -> Self {
        self.project_id = Some(project_id.into());
        self
    }

    pub fn with_task(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    pub fn with_outcome(mut self, outcome: Outcome) -> Self {
        self.outcome = outcome;
        self
    }
}

/// Keys whose values are replaced before storage. Matching mirrors the agent
/// package rule: normalised exact names plus high-signal fragments.
const REDACT_EXACT: &[&str] = &[
    "token",
    "secret",
    "password",
    "passphrase",
    "credential",
    "credentials",
    "auth",
    "authorization",
    "bearer",
    "cookie",
    "key",
    "jwt",
];
const REDACT_FRAGMENTS: &[&str] = &[
    "apikey",
    "accesstoken",
    "refreshtoken",
    "idtoken",
    "authtoken",
    "bearertoken",
    "sessiontoken",
    "privatekey",
    "secretkey",
    "clientsecret",
    "apisecret",
];

pub const REDACTED: &str = "[redacted]";

fn normalise(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Replace sensitive values anywhere in a detail document.
pub fn redact(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, child) in map {
                let normalised = normalise(key);
                let sensitive = REDACT_EXACT.contains(&normalised.as_str())
                    || REDACT_FRAGMENTS.iter().any(|f| normalised.contains(f));
                out.insert(
                    key.clone(),
                    if sensitive {
                        serde_json::Value::String(REDACTED.into())
                    } else {
                        redact(child)
                    },
                );
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact).collect())
        }
        other => other.clone(),
    }
}

#[derive(Debug, Clone, Default)]
pub struct ActivityQuery {
    pub project_id: Option<String>,
    pub task_id: Option<String>,
    pub actor_type: Option<ActorType>,
    pub action_prefix: Option<String>,
    pub limit: u32,
    pub offset: u32,
}

pub struct ActivityRepo<'a> {
    db: &'a Db,
}

impl<'a> ActivityRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    pub fn record(&self, entry: NewActivity) -> Result<String> {
        let id = otwono_types::new_id("act");
        let detail = crate::to_json(&redact(&entry.detail));
        self.db.conn()?.execute(
            "INSERT INTO activity_log
               (id, created_at, actor_type, actor_id, actor_name, action, target_type,
                target_id, project_id, task_id, outcome, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                id,
                otwono_types::ids::format_ts(&otwono_types::now()),
                entry.actor_type.as_str(),
                entry.actor_id,
                entry.actor_name,
                entry.action,
                entry.target_type,
                entry.target_id,
                entry.project_id,
                entry.task_id,
                entry.outcome.as_str(),
                detail,
            ],
        )?;
        Ok(id)
    }

    pub fn list(&self, query: &ActivityQuery) -> Result<Vec<ActivityEntry>> {
        let limit = query.limit.clamp(1, 1000);
        let conn = self.db.conn()?;
        let mut sql = String::from(
            "SELECT id, created_at, actor_type, actor_id, actor_name, action, target_type,
                    target_id, project_id, task_id, outcome, detail
             FROM activity_log WHERE 1 = 1",
        );
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(project) = &query.project_id {
            sql.push_str(" AND project_id = ?");
            binds.push(Box::new(project.clone()));
        }
        if let Some(task) = &query.task_id {
            sql.push_str(" AND task_id = ?");
            binds.push(Box::new(task.clone()));
        }
        if let Some(actor) = query.actor_type {
            sql.push_str(" AND actor_type = ?");
            binds.push(Box::new(actor.as_str().to_string()));
        }
        if let Some(prefix) = &query.action_prefix {
            sql.push_str(" AND action LIKE ?");
            binds.push(Box::new(format!("{prefix}%")));
        }
        sql.push_str(" ORDER BY created_at DESC, rowid DESC LIMIT ? OFFSET ?");
        binds.push(Box::new(limit as i64));
        binds.push(Box::new(query.offset as i64));

        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |row| {
            Ok(ActivityEntry {
                id: row.get(0)?,
                created_at: row.get(1)?,
                actor_type: match row.get::<_, String>(2)?.as_str() {
                    "user" => ActorType::User,
                    "agent" => ActorType::Agent,
                    "relay" => ActorType::Relay,
                    _ => ActorType::System,
                },
                actor_id: row.get(3)?,
                actor_name: row.get(4)?,
                action: row.get(5)?,
                target_type: row.get(6)?,
                target_id: row.get(7)?,
                project_id: row.get(8)?,
                task_id: row.get(9)?,
                outcome: match row.get::<_, String>(10)?.as_str() {
                    "denied" => Outcome::Denied,
                    "failed" => Outcome::Failed,
                    "pending" => Outcome::Pending,
                    _ => Outcome::Ok,
                },
                detail: crate::json_column(row.get(11)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self
            .db
            .conn()?
            .query_row("SELECT COUNT(*) FROM activity_log", [], |r| r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secrets_are_redacted_before_they_reach_the_database() {
        let db = Db::open_in_memory().unwrap();
        let repo = ActivityRepo::new(&db);
        repo.record(NewActivity::system("provider.request").with_detail(json!({
            "endpoint": "http://127.0.0.1:11434",
            "api_key": "sk-live-should-not-appear",
            "headers": { "Authorization": "Bearer secret-value" },
            "nested": [{ "refresh_token": "rt-should-not-appear" }],
            "max_output_tokens": 512
        })))
        .unwrap();

        let raw: String = db
            .conn()
            .unwrap()
            .query_row("SELECT detail FROM activity_log", [], |r| r.get(0))
            .unwrap();
        assert!(!raw.contains("sk-live-should-not-appear"), "{raw}");
        assert!(!raw.contains("secret-value"), "{raw}");
        assert!(!raw.contains("rt-should-not-appear"), "{raw}");
        assert!(
            raw.contains("127.0.0.1:11434"),
            "non-sensitive detail must survive"
        );
        assert!(raw.contains("512"), "model parameters must not be redacted");
    }

    #[test]
    fn entries_come_back_newest_first_and_filter_by_project() {
        let db = Db::open_in_memory().unwrap();
        let repo = ActivityRepo::new(&db);
        repo.record(NewActivity::user("project.create").with_project("prj_1"))
            .unwrap();
        repo.record(NewActivity::user("task.run").with_project("prj_1"))
            .unwrap();
        repo.record(NewActivity::user("chat.send").with_project("prj_2"))
            .unwrap();

        let all = repo
            .list(&ActivityQuery {
                limit: 50,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].action, "chat.send", "newest first");

        let scoped = repo
            .list(&ActivityQuery {
                project_id: Some("prj_1".into()),
                limit: 50,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(scoped.len(), 2);
        assert!(scoped
            .iter()
            .all(|e| e.project_id.as_deref() == Some("prj_1")));
    }

    #[test]
    fn queries_filter_by_actor_and_action_prefix() {
        let db = Db::open_in_memory().unwrap();
        let repo = ActivityRepo::new(&db);
        repo.record(NewActivity::agent(
            "agt_1",
            "Researcher",
            "tool.knowledge_search",
        ))
        .unwrap();
        repo.record(NewActivity::agent("agt_1", "Researcher", "tool.http_fetch"))
            .unwrap();
        repo.record(NewActivity::user("settings.update")).unwrap();

        let tools = repo
            .list(&ActivityQuery {
                action_prefix: Some("tool.".into()),
                limit: 50,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(tools.len(), 2);

        let by_user = repo
            .list(&ActivityQuery {
                actor_type: Some(ActorType::User),
                limit: 50,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_user.len(), 1);
        assert_eq!(by_user[0].action, "settings.update");
    }

    #[test]
    fn denied_outcomes_are_recorded_as_such() {
        let db = Db::open_in_memory().unwrap();
        let repo = ActivityRepo::new(&db);
        repo.record(
            NewActivity::agent("agt_1", "Researcher", "permission.check")
                .with_outcome(Outcome::Denied),
        )
        .unwrap();
        let entries = repo
            .list(&ActivityQuery {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(entries[0].outcome, Outcome::Denied);
    }

    #[test]
    fn the_limit_is_clamped_to_something_sane() {
        let db = Db::open_in_memory().unwrap();
        let repo = ActivityRepo::new(&db);
        for _ in 0..5 {
            repo.record(NewActivity::system("tick")).unwrap();
        }
        assert_eq!(
            repo.list(&ActivityQuery {
                limit: 0,
                ..Default::default()
            })
            .unwrap()
            .len(),
            1
        );
        assert_eq!(
            repo.list(&ActivityQuery {
                limit: u32::MAX,
                ..Default::default()
            })
            .unwrap()
            .len(),
            5
        );
    }

    #[test]
    fn redaction_leaves_ordinary_documents_alone() {
        let input = json!({ "path": "/home/u/notes.md", "chunks": 12, "monkey": "banana" });
        assert_eq!(redact(&input), input);
    }
}

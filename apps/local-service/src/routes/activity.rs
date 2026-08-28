//! The audit log, and its exportable report.

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use otwono_store::repo::activity::{ActivityEntry, ActivityQuery, ActivityRepo, ActorType};

use crate::error::ApiResult;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct LogQuery {
    pub project_id: Option<String>,
    pub task_id: Option<String>,
    pub actor_type: Option<String>,
    pub action_prefix: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub offset: Option<u32>,
}

impl LogQuery {
    fn into_repo_query(self) -> ActivityQuery {
        ActivityQuery {
            project_id: self.project_id,
            task_id: self.task_id,
            actor_type: match self.actor_type.as_deref() {
                Some("user") => Some(ActorType::User),
                Some("agent") => Some(ActorType::Agent),
                Some("system") => Some(ActorType::System),
                Some("relay") => Some(ActorType::Relay),
                _ => None,
            },
            action_prefix: self.action_prefix,
            limit: self.limit.unwrap_or(100),
            offset: self.offset.unwrap_or(0),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct LogResponse {
    pub entries: Vec<ActivityEntry>,
    pub total: i64,
}

pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<LogQuery>,
) -> ApiResult<Json<LogResponse>> {
    let repo = ActivityRepo::new(&state.db);
    Ok(Json(LogResponse {
        total: repo.count()?,
        entries: repo.list(&query.into_repo_query())?,
    }))
}

/// A plain-text activity report the user can keep or send on. Values that could
/// carry a secret were already redacted when each row was written.
pub async fn export(
    State(state): State<AppState>,
    Query(query): Query<LogQuery>,
) -> ApiResult<crate::error::TextResponse> {
    let mut repo_query = query.into_repo_query();
    repo_query.limit = repo_query.limit.max(1000);
    let entries = ActivityRepo::new(&state.db).list(&repo_query)?;

    let mut out = format!(
        "OTWONO AI activity report\nGenerated {}\n{} entries\n\n",
        otwono_types::ids::format_ts(&otwono_types::now()),
        entries.len()
    );
    for entry in &entries {
        out.push_str(&format!(
            "{}  {:<7}  {:<28}  {}\n",
            entry.created_at,
            entry.actor_type.as_str(),
            entry.action,
            entry.actor_name.clone().unwrap_or_default()
        ));
        if entry.detail != serde_json::json!({}) {
            out.push_str(&format!("    {}\n", entry.detail));
        }
    }

    Ok(crate::error::plain_text(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otwono_store::repo::activity::NewActivity;

    #[tokio::test]
    async fn entries_are_listed_newest_first_with_a_total() {
        let state = AppState::for_tests();
        let repo = ActivityRepo::new(&state.db);
        repo.record(NewActivity::user("project.create")).unwrap();
        repo.record(NewActivity::user("chat.send")).unwrap();

        let Json(response) = list(
            State(state),
            Query(LogQuery {
                project_id: None,
                task_id: None,
                actor_type: None,
                action_prefix: None,
                limit: None,
                offset: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.total, 2);
        assert_eq!(response.entries[0].action, "chat.send");
    }

    #[tokio::test]
    async fn the_exported_report_contains_no_secrets() {
        let state = AppState::for_tests();
        ActivityRepo::new(&state.db)
            .record(
                NewActivity::user("connection.create").with_detail(serde_json::json!({
                    "endpoint": "https://api.example.com",
                    "api_key": "sk-live-should-not-appear",
                })),
            )
            .unwrap();

        let (_, body) = export(
            State(state),
            Query(LogQuery {
                project_id: None,
                task_id: None,
                actor_type: None,
                action_prefix: None,
                limit: None,
                offset: None,
            }),
        )
        .await
        .unwrap();

        assert!(body.contains("connection.create"));
        assert!(body.contains("api.example.com"));
        assert!(!body.contains("sk-live-should-not-appear"), "{body}");
        assert!(body.contains("[redacted]"));
    }
}

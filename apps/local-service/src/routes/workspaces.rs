//! Offices, Labs, Boardrooms and Think Tanks, plus their sessions and
//! experiments.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use otwono_agent_core::executor::ProviderExecutor;
use otwono_agent_core::lab::LabRunner;
use otwono_agent_core::session::SessionRunner;
use otwono_store::repo::agents::AgentRepo;
use otwono_store::repo::providers::ProviderRepo;
use otwono_store::repo::workspaces::{
    LabExperiment, LabResult, LabVariant, NewWorkspace, WorkspaceRepo,
};
use otwono_types::agent::Agent;
use otwono_types::workspace::{Session, SessionContribution, Workspace, WorkspaceKind};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub kind: Option<String>,
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceSummary {
    #[serde(flatten)]
    pub workspace: Workspace,
    pub member_count: usize,
    pub purpose: &'static str,
    pub runs_sessions: bool,
}

fn summarise(repo: &WorkspaceRepo<'_>, workspace: Workspace) -> ApiResult<WorkspaceSummary> {
    Ok(WorkspaceSummary {
        member_count: repo.members(&workspace.id)?.len(),
        purpose: workspace.kind.purpose(),
        runs_sessions: workspace.kind.is_session_based(),
        workspace,
    })
}

pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Vec<WorkspaceSummary>>> {
    let kind = match &query.kind {
        Some(kind) => {
            Some(WorkspaceKind::parse(kind).map_err(|e| ApiError::BadRequest(e.to_string()))?)
        }
        None => None,
    };
    let repo = WorkspaceRepo::new(&state.db);
    repo.list(kind, query.include_archived)?
        .into_iter()
        .map(|workspace| summarise(&repo, workspace))
        .collect::<ApiResult<Vec<_>>>()
        .map(Json)
}

/// What each workspace kind is for, so the client does not duplicate the copy.
#[derive(Debug, Serialize)]
pub struct KindDescription {
    pub kind: &'static str,
    pub display_name: &'static str,
    pub purpose: &'static str,
    pub runs_sessions: bool,
}

pub async fn kinds() -> Json<Vec<KindDescription>> {
    Json(
        WorkspaceKind::ALL
            .iter()
            .map(|kind| KindDescription {
                kind: kind.as_str(),
                display_name: kind.display_name(),
                purpose: kind.purpose(),
                runs_sessions: kind.is_session_based(),
            })
            .collect(),
    )
}

#[derive(Debug, Deserialize)]
pub struct CreateWorkspace {
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub shared_instructions: String,
    #[serde(default)]
    pub knowledge_source_ids: Vec<String>,
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateWorkspace>,
) -> ApiResult<Json<Workspace>> {
    let kind = WorkspaceKind::parse(&body.kind).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    WorkspaceRepo::new(&state.db)
        .create(NewWorkspace {
            kind,
            name: body.name,
            description: body.description,
            icon: body.icon.unwrap_or_else(|| kind.as_str().to_string()),
            shared_instructions: body.shared_instructions,
            knowledge_source_ids: body.knowledge_source_ids,
        })
        .map(Json)
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

#[derive(Debug, Serialize)]
pub struct WorkspaceDetail {
    #[serde(flatten)]
    pub summary: WorkspaceSummary,
    pub members: Vec<MemberDetail>,
    pub sessions: Vec<Session>,
    pub experiments: Vec<LabExperiment>,
}

#[derive(Debug, Serialize)]
pub struct MemberDetail {
    pub agent: Agent,
    pub job_role: String,
    pub is_coordinator: bool,
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<WorkspaceDetail>> {
    let repo = WorkspaceRepo::new(&state.db);
    let agents = AgentRepo::new(&state.db);
    let workspace = repo
        .get(&id)?
        .ok_or_else(|| ApiError::not_found("That workspace"))?;
    let is_session_based = workspace.kind.is_session_based();
    let is_lab = workspace.kind == WorkspaceKind::Lab;

    let members = repo
        .members(&id)?
        .into_iter()
        .filter_map(|member| {
            agents
                .get(&member.agent_id)
                .ok()
                .flatten()
                .map(|agent| MemberDetail {
                    agent,
                    job_role: member.job_role,
                    is_coordinator: member.is_coordinator,
                })
        })
        .collect();

    Ok(Json(WorkspaceDetail {
        members,
        sessions: if is_session_based {
            repo.list_sessions(&id)?
        } else {
            Vec::new()
        },
        experiments: if is_lab {
            repo.list_experiments(&id)?
        } else {
            Vec::new()
        },
        summary: summarise(&repo, workspace)?,
    }))
}

#[derive(Debug, Deserialize)]
pub struct UpdateWorkspace {
    pub name: Option<String>,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub shared_instructions: Option<String>,
    pub knowledge_source_ids: Option<Vec<String>>,
    pub favorite: Option<bool>,
    pub archived: Option<bool>,
}

pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateWorkspace>,
) -> ApiResult<Json<Workspace>> {
    let repo = WorkspaceRepo::new(&state.db);
    let mut workspace = repo
        .get(&id)?
        .ok_or_else(|| ApiError::not_found("That workspace"))?;

    if let Some(value) = body.name {
        workspace.name = value;
    }
    if let Some(value) = body.description {
        workspace.description = value;
    }
    if let Some(value) = body.icon {
        workspace.icon = value;
    }
    if let Some(value) = body.shared_instructions {
        workspace.shared_instructions = value;
    }
    if let Some(value) = body.knowledge_source_ids {
        workspace.knowledge_source_ids = value;
    }
    if let Some(value) = body.favorite {
        workspace.favorite = value;
    }
    if let Some(value) = body.archived {
        workspace.archived = value;
    }

    repo.update(&workspace)?;
    Ok(Json(workspace))
}

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let repo = WorkspaceRepo::new(&state.db);
    if repo.get(&id)?.is_none() {
        return Err(ApiError::not_found("That workspace"));
    }
    repo.delete(&id)?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Debug, Deserialize)]
pub struct DuplicateRequest {
    pub name: String,
}

pub async fn duplicate(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DuplicateRequest>,
) -> ApiResult<Json<Workspace>> {
    WorkspaceRepo::new(&state.db)
        .duplicate(&id, &body.name)
        .map(Json)
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

#[derive(Debug, Deserialize)]
pub struct AddMember {
    pub agent_id: String,
    #[serde(default)]
    pub job_role: String,
    #[serde(default)]
    pub is_coordinator: bool,
}

pub async fn add_member(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AddMember>,
) -> ApiResult<Json<serde_json::Value>> {
    let repo = WorkspaceRepo::new(&state.db);
    if repo.get(&id)?.is_none() {
        return Err(ApiError::not_found("That workspace"));
    }
    let agents = AgentRepo::new(&state.db);
    let agent = agents
        .get(&body.agent_id)?
        .ok_or_else(|| ApiError::not_found("That agent"))?;
    let job_role = if body.job_role.trim().is_empty() {
        agent.role.clone()
    } else {
        body.job_role
    };
    repo.add_member(&id, &body.agent_id, &job_role, body.is_coordinator)?;
    Ok(Json(serde_json::json!({ "added": true })))
}

pub async fn remove_member(
    State(state): State<AppState>,
    Path((id, agent_id)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    WorkspaceRepo::new(&state.db).remove_member(&id, &agent_id)?;
    Ok(Json(serde_json::json!({ "removed": true })))
}

// ---- sessions

#[derive(Debug, Deserialize)]
pub struct CreateSession {
    pub question: String,
    #[serde(default)]
    pub chair_agent_id: Option<String>,
}

pub async fn create_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CreateSession>,
) -> ApiResult<Json<Session>> {
    if body.question.trim().is_empty() {
        return Err(ApiError::BadRequest("A session needs a question.".into()));
    }
    WorkspaceRepo::new(&state.db)
        .create_session(&id, body.question.trim(), body.chair_agent_id.as_deref())
        .map(Json)
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

#[derive(Debug, Serialize)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub session: Session,
    pub contributions: Vec<SessionContribution>,
}

pub async fn get_session(
    State(state): State<AppState>,
    Path((_workspace_id, session_id)): Path<(String, String)>,
) -> ApiResult<Json<SessionDetail>> {
    let repo = WorkspaceRepo::new(&state.db);
    let session = repo
        .get_session(&session_id)?
        .ok_or_else(|| ApiError::not_found("That session"))?;
    Ok(Json(SessionDetail {
        contributions: repo.contributions(&session_id)?,
        session,
    }))
}

fn executor(state: &AppState) -> ApiResult<ProviderExecutor> {
    let connection = ProviderRepo::new(&state.db)
        .list()?
        .into_iter()
        .find(|c| c.enabled)
        .ok_or_else(|| {
            ApiError::BadRequest(
                "No AI connection is available. Connect Ollama or LM Studio in Connections first."
                    .to_string(),
            )
        })?;
    Ok(ProviderExecutor::new(state.provider_for(&connection)))
}

pub async fn run_session(
    State(state): State<AppState>,
    Path((_workspace_id, session_id)): Path<(String, String)>,
) -> ApiResult<Json<SessionDetail>> {
    let executor = executor(&state)?;
    let session = SessionRunner::new(&state.db, &executor)
        .run(&session_id)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(Json(SessionDetail {
        contributions: WorkspaceRepo::new(&state.db).contributions(&session_id)?,
        session,
    }))
}

// ---- lab experiments

#[derive(Debug, Deserialize)]
pub struct CreateExperiment {
    pub name: String,
    pub prompt: String,
    pub variants: Vec<LabVariant>,
}

pub async fn create_experiment(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CreateExperiment>,
) -> ApiResult<Json<LabExperiment>> {
    let repo = WorkspaceRepo::new(&state.db);
    let workspace = repo
        .get(&id)?
        .ok_or_else(|| ApiError::not_found("That workspace"))?;
    if workspace.kind != WorkspaceKind::Lab {
        return Err(ApiError::BadRequest(
            "Experiments belong to a Lab. Create one, or use an existing Lab.".into(),
        ));
    }
    if body.variants.is_empty() {
        return Err(ApiError::BadRequest(
            "An experiment needs at least one configuration to test.".into(),
        ));
    }
    repo.create_experiment(&id, &body.name, &body.prompt, &body.variants)
        .map(Json)
        .map_err(ApiError::Internal)
}

pub async fn run_experiment(
    State(state): State<AppState>,
    Path((_workspace_id, experiment_id)): Path<(String, String)>,
) -> ApiResult<Json<Vec<LabResult>>> {
    let executor = executor(&state)?;
    LabRunner::new(&state.db, &executor)
        .run(&experiment_id)
        .await
        .map(Json)
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

#[derive(Debug, Deserialize)]
pub struct PromoteRequest {
    pub variant_id: String,
    pub target_agent_id: String,
}

pub async fn promote_variant(
    State(state): State<AppState>,
    Path((_workspace_id, experiment_id)): Path<(String, String)>,
    Json(body): Json<PromoteRequest>,
) -> ApiResult<Json<Agent>> {
    struct NoExecutor;
    #[async_trait::async_trait]
    impl otwono_agent_core::AgentExecutor for NoExecutor {
        async fn run(
            &self,
            _turn: otwono_agent_core::executor::AgentTurn,
        ) -> anyhow::Result<otwono_agent_core::executor::AgentOutcome> {
            anyhow::bail!("promotion does not run a model")
        }
    }
    let executor = NoExecutor;
    LabRunner::new(&state.db, &executor)
        .promote(&experiment_id, &body.variant_id, &body.target_agent_id)
        .map(Json)
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_workspace_kind_is_described_for_the_client() {
        let Json(kinds) = kinds().await;
        assert_eq!(kinds.len(), 5);
        assert!(kinds
            .iter()
            .any(|k| k.kind == "think_tank" && k.runs_sessions));
        assert!(kinds.iter().any(|k| k.kind == "office" && !k.runs_sessions));
        assert!(kinds.iter().all(|k| k.purpose.ends_with('.')));
    }

    #[tokio::test]
    async fn a_session_cannot_be_created_in_an_office() {
        let state = AppState::for_tests();
        let Json(office) = create(
            State(state.clone()),
            Json(CreateWorkspace {
                kind: "office".into(),
                name: "Ops".into(),
                description: String::new(),
                icon: None,
                shared_instructions: String::new(),
                knowledge_source_ids: vec![],
            }),
        )
        .await
        .unwrap();

        let error = create_session(
            State(state),
            Path(office.id),
            Json(CreateSession {
                question: "Ship?".into(),
                chair_agent_id: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::BadRequest(ref m)
            if m.contains("do not run structured sessions")));
    }

    #[tokio::test]
    async fn an_experiment_cannot_be_created_outside_a_lab() {
        let state = AppState::for_tests();
        let Json(office) = create(
            State(state.clone()),
            Json(CreateWorkspace {
                kind: "office".into(),
                name: "Ops".into(),
                description: String::new(),
                icon: None,
                shared_instructions: String::new(),
                knowledge_source_ids: vec![],
            }),
        )
        .await
        .unwrap();

        let error = create_experiment(
            State(state),
            Path(office.id),
            Json(CreateExperiment {
                name: "Tone".into(),
                prompt: "Summarise.".into(),
                variants: vec![],
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::BadRequest(ref m) if m.contains("belong to a Lab")));
    }

    #[tokio::test]
    async fn a_workspace_detail_lists_its_team_with_the_coordinator_marked() {
        use otwono_agent_core::seed::seed_templates;
        let state = AppState::for_tests();
        seed_templates(&state.db).unwrap();
        let agents = AgentRepo::new(&state.db);
        let exec = agents
            .get_by_template_key("executive-orchestrator")
            .unwrap()
            .unwrap();
        let writer = agents.get_by_template_key("writer").unwrap().unwrap();

        let Json(office) = create(
            State(state.clone()),
            Json(CreateWorkspace {
                kind: "office".into(),
                name: "Ops".into(),
                description: String::new(),
                icon: None,
                shared_instructions: String::new(),
                knowledge_source_ids: vec![],
            }),
        )
        .await
        .unwrap();

        let _ = add_member(
            State(state.clone()),
            Path(office.id.clone()),
            Json(AddMember {
                agent_id: exec.id,
                job_role: String::new(),
                is_coordinator: true,
            }),
        )
        .await
        .unwrap();
        let _ = add_member(
            State(state.clone()),
            Path(office.id.clone()),
            Json(AddMember {
                agent_id: writer.id,
                job_role: "Drafting".into(),
                is_coordinator: false,
            }),
        )
        .await
        .unwrap();

        let Json(detail) = get(State(state), Path(office.id)).await.unwrap();
        assert_eq!(detail.members.len(), 2);
        assert_eq!(
            detail.members.iter().filter(|m| m.is_coordinator).count(),
            1
        );
        assert!(detail.members.iter().any(|m| m.job_role == "Drafting"));
        assert!(!detail.summary.runs_sessions);
    }
}

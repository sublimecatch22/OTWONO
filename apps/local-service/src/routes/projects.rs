//! Projects, tasks, planning, execution and reports.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use otwono_agent_core::executor::ProviderExecutor;
use otwono_agent_core::{Orchestrator, RunReport};
use otwono_store::repo::activity::{ActivityRepo, NewActivity};
use otwono_store::repo::agents::AgentRepo;
use otwono_store::repo::projects::{Artifact, NewProject, NewTask, ProjectRepo};
use otwono_store::repo::providers::ProviderRepo;
use otwono_types::project::{Project, ProjectState, Task, TaskState};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub workspace_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProjectSummary {
    #[serde(flatten)]
    pub project: Project,
    pub task_count: usize,
    pub completed_tasks: usize,
    pub awaiting_approval: usize,
}

pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Vec<ProjectSummary>>> {
    let repo = ProjectRepo::new(&state.db);
    let mut summaries = Vec::new();
    for project in repo.list(query.workspace_id.as_deref())? {
        let tasks = repo.tasks(&project.id)?;
        summaries.push(ProjectSummary {
            task_count: tasks.len(),
            completed_tasks: tasks
                .iter()
                .filter(|t| t.state == TaskState::Completed)
                .count(),
            awaiting_approval: tasks
                .iter()
                .filter(|t| t.state == TaskState::AwaitingApproval)
                .count(),
            project,
        });
    }
    Ok(Json(summaries))
}

#[derive(Debug, Deserialize)]
pub struct CreateProject {
    pub title: String,
    #[serde(default)]
    pub objective: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub orchestrator_agent_id: Option<String>,
    #[serde(default)]
    pub verifier_agent_id: Option<String>,
    #[serde(default)]
    pub max_steps: Option<u32>,
    #[serde(default)]
    pub max_task_retries: Option<u32>,
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateProject>,
) -> ApiResult<Json<Project>> {
    let agents = AgentRepo::new(&state.db);
    // Default to the shipped orchestrator and verifier when the user has not
    // chosen, so a project is runnable without extra setup.
    let orchestrator = body.orchestrator_agent_id.or_else(|| {
        agents
            .get_by_template_key("executive-orchestrator")
            .ok()
            .flatten()
            .map(|a| a.id)
    });
    let verifier = body.verifier_agent_id.or_else(|| {
        agents
            .get_by_template_key("verification-agent")
            .ok()
            .flatten()
            .map(|a| a.id)
    });

    let project = ProjectRepo::new(&state.db)
        .create(NewProject {
            title: body.title,
            objective: body.objective,
            acceptance_criteria: body.acceptance_criteria,
            workspace_id: body.workspace_id,
            orchestrator_agent_id: orchestrator,
            verifier_agent_id: verifier,
            max_steps: body.max_steps,
            max_task_retries: body.max_task_retries,
        })
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    ActivityRepo::new(&state.db)
        .record(NewActivity::user("project.create").with_project(&project.id))?;
    Ok(Json(project))
}

#[derive(Debug, Serialize)]
pub struct ProjectDetail {
    #[serde(flatten)]
    pub project: Project,
    pub tasks: Vec<Task>,
    pub artifacts: Vec<Artifact>,
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ProjectDetail>> {
    let repo = ProjectRepo::new(&state.db);
    let project = repo
        .get(&id)?
        .ok_or_else(|| ApiError::not_found("That project"))?;
    Ok(Json(ProjectDetail {
        tasks: repo.tasks(&id)?,
        artifacts: repo.artifacts(&id)?,
        project,
    }))
}

#[derive(Debug, Deserialize)]
pub struct UpdateProject {
    pub title: Option<String>,
    pub objective: Option<String>,
    pub acceptance_criteria: Option<Vec<String>>,
    pub orchestrator_agent_id: Option<Option<String>>,
    pub verifier_agent_id: Option<Option<String>>,
    pub max_steps: Option<u32>,
    pub max_task_retries: Option<u32>,
    pub workspace_id: Option<Option<String>>,
    /// Opt in to sending this project's metadata to a linked account.
    pub sync_enabled: Option<bool>,
}

pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateProject>,
) -> ApiResult<Json<Project>> {
    let repo = ProjectRepo::new(&state.db);
    let mut project = repo
        .get(&id)?
        .ok_or_else(|| ApiError::not_found("That project"))?;

    if let Some(value) = body.title {
        project.title = value;
    }
    if let Some(value) = body.objective {
        project.objective = value;
    }
    if let Some(value) = body.acceptance_criteria {
        project.acceptance_criteria = value;
    }
    if let Some(value) = body.orchestrator_agent_id {
        project.orchestrator_agent_id = value;
    }
    if let Some(value) = body.verifier_agent_id {
        project.verifier_agent_id = value;
    }
    if let Some(value) = body.max_steps {
        project.max_steps = value.clamp(1, 500);
    }
    if let Some(value) = body.max_task_retries {
        project.max_task_retries = value.min(10);
    }
    if let Some(value) = body.workspace_id {
        project.workspace_id = value;
    }
    if let Some(value) = body.sync_enabled {
        project.sync_enabled = value;
    }

    repo.update(&project)?;
    Ok(Json(project))
}

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let repo = ProjectRepo::new(&state.db);
    if repo.get(&id)?.is_none() {
        return Err(ApiError::not_found("That project"));
    }
    repo.delete(&id)?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Debug, Deserialize)]
pub struct TransitionRequest {
    pub state: String,
}

pub async fn transition(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<TransitionRequest>,
) -> ApiResult<Json<Project>> {
    let target =
        ProjectState::parse(&body.state).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    ProjectRepo::new(&state.db)
        .transition(&id, target)
        .map(Json)
        .map_err(|e| ApiError::Conflict(e.to_string()))
}

#[derive(Debug, Deserialize)]
pub struct AddTask {
    pub title: String,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub assigned_agent_id: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub requires_approval: bool,
}

pub async fn add_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AddTask>,
) -> ApiResult<Json<Task>> {
    let repo = ProjectRepo::new(&state.db);
    let task = repo
        .add_task(
            &id,
            NewTask {
                title: body.title,
                instructions: body.instructions,
                acceptance_criteria: body.acceptance_criteria,
                assigned_agent_id: body.assigned_agent_id,
                depends_on: body.depends_on,
                requires_approval: body.requires_approval,
                max_attempts: None,
            },
        )
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    repo.refresh_readiness(&id)?;
    Ok(Json(task))
}

/// Build the executor for a project's orchestrator, or explain why it cannot.
fn executor_for(state: &AppState, project: &Project) -> ApiResult<ProviderExecutor> {
    let agents = AgentRepo::new(&state.db);
    let orchestrator = project
        .orchestrator_agent_id
        .as_deref()
        .and_then(|id| agents.get(id).ok().flatten())
        .ok_or_else(|| {
            ApiError::BadRequest(
                "This project has no orchestrator agent. Choose one in the project's settings."
                    .to_string(),
            )
        })?;

    let providers = ProviderRepo::new(&state.db);
    let connection = orchestrator
        .provider_connection_id
        .as_deref()
        .and_then(|id| providers.get(id).ok().flatten())
        .or_else(|| providers.list().ok()?.into_iter().find(|c| c.enabled))
        .ok_or_else(|| {
            ApiError::BadRequest(
                "No AI connection is available. Connect Ollama or LM Studio in Connections \
                 before running a project."
                    .to_string(),
            )
        })?;

    Ok(ProviderExecutor::new(state.provider_for(&connection)))
}

pub async fn plan(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<Task>>> {
    let project = ProjectRepo::new(&state.db)
        .get(&id)?
        .ok_or_else(|| ApiError::not_found("That project"))?;
    super::ensure_agent_models(&state)?;
    let executor = executor_for(&state, &project)?;
    Orchestrator::new(&state.db, &executor)
        .plan(&id)
        .await
        .map(Json)
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

pub async fn run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<RunReport>> {
    let project = ProjectRepo::new(&state.db)
        .get(&id)?
        .ok_or_else(|| ApiError::not_found("That project"))?;
    super::ensure_agent_models(&state)?;
    let executor = executor_for(&state, &project)?;
    Orchestrator::new(&state.db, &executor)
        .run(&id)
        .await
        .map(Json)
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

#[derive(Debug, Deserialize)]
pub struct ApprovalDecision {
    pub approve: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

pub async fn decide_task(
    State(state): State<AppState>,
    Path((project_id, task_id)): Path<(String, String)>,
    Json(body): Json<ApprovalDecision>,
) -> ApiResult<Json<Task>> {
    let project = ProjectRepo::new(&state.db)
        .get(&project_id)?
        .ok_or_else(|| ApiError::not_found("That project"))?;
    let _ = project;
    // Deciding does not need a model, so no executor is built here.
    struct NoExecutor;
    #[async_trait::async_trait]
    impl otwono_agent_core::AgentExecutor for NoExecutor {
        async fn run(
            &self,
            _turn: otwono_agent_core::executor::AgentTurn,
        ) -> anyhow::Result<otwono_agent_core::executor::AgentOutcome> {
            anyhow::bail!("approving a task does not run a model")
        }
    }
    let executor = NoExecutor;
    let orchestrator = Orchestrator::new(&state.db, &executor);

    if body.approve {
        orchestrator
            .approve_task(&task_id)
            .map(Json)
            .map_err(|e| ApiError::Conflict(e.to_string()))
    } else {
        orchestrator
            .decline_task(
                &task_id,
                body.reason.as_deref().unwrap_or("no reason given"),
            )
            .map(Json)
            .map_err(|e| ApiError::Conflict(e.to_string()))
    }
}

pub async fn report(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<crate::error::TextResponse> {
    struct NoExecutor;
    #[async_trait::async_trait]
    impl otwono_agent_core::AgentExecutor for NoExecutor {
        async fn run(
            &self,
            _turn: otwono_agent_core::executor::AgentTurn,
        ) -> anyhow::Result<otwono_agent_core::executor::AgentOutcome> {
            anyhow::bail!("building a report does not run a model")
        }
    }
    let executor = NoExecutor;
    let markdown = Orchestrator::new(&state.db, &executor)
        .completion_report(&id)
        .map_err(|_| ApiError::not_found("That project"))?;
    Ok(crate::error::markdown(markdown))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otwono_agent_core::seed::seed_templates;

    #[tokio::test]
    async fn a_new_project_gets_the_shipped_orchestrator_and_verifier_by_default() {
        let state = AppState::for_tests();
        seed_templates(&state.db).unwrap();

        let Json(project) = create(
            State(state.clone()),
            Json(CreateProject {
                title: "Quarterly report".into(),
                objective: "Produce it".into(),
                acceptance_criteria: vec![],
                workspace_id: None,
                orchestrator_agent_id: None,
                verifier_agent_id: None,
                max_steps: None,
                max_task_retries: None,
            }),
        )
        .await
        .unwrap();

        assert!(project.orchestrator_agent_id.is_some());
        assert!(project.verifier_agent_id.is_some());
        assert_eq!(project.state, ProjectState::Draft);
        assert!(!project.sync_enabled, "synchronisation is opt-in");
    }

    #[tokio::test]
    async fn planning_without_a_connection_says_what_to_connect() {
        let state = AppState::for_tests();
        seed_templates(&state.db).unwrap();
        let Json(project) = create(
            State(state.clone()),
            Json(CreateProject {
                title: "P".into(),
                objective: "O".into(),
                acceptance_criteria: vec![],
                workspace_id: None,
                orchestrator_agent_id: None,
                verifier_agent_id: None,
                max_steps: None,
                max_task_retries: None,
            }),
        )
        .await
        .unwrap();

        let error = plan(State(state), Path(project.id)).await.unwrap_err();
        assert!(matches!(error, ApiError::BadRequest(ref m)
            if m.contains("Connect Ollama or LM Studio")));
    }

    #[tokio::test]
    async fn an_illegal_state_change_is_a_conflict_not_a_silent_no_op() {
        let state = AppState::for_tests();
        let Json(project) = create(
            State(state.clone()),
            Json(CreateProject {
                title: "P".into(),
                objective: String::new(),
                acceptance_criteria: vec![],
                workspace_id: None,
                orchestrator_agent_id: None,
                verifier_agent_id: None,
                max_steps: None,
                max_task_retries: None,
            }),
        )
        .await
        .unwrap();

        let error = transition(
            State(state.clone()),
            Path(project.id.clone()),
            Json(TransitionRequest {
                state: "running".into(),
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::Conflict(_)));

        let Json(detail) = get(State(state), Path(project.id)).await.unwrap();
        assert_eq!(detail.project.state, ProjectState::Draft);
    }

    #[tokio::test]
    async fn tasks_added_by_hand_respect_dependencies() {
        let state = AppState::for_tests();
        let Json(project) = create(
            State(state.clone()),
            Json(CreateProject {
                title: "P".into(),
                objective: String::new(),
                acceptance_criteria: vec![],
                workspace_id: None,
                orchestrator_agent_id: None,
                verifier_agent_id: None,
                max_steps: None,
                max_task_retries: None,
            }),
        )
        .await
        .unwrap();

        let Json(first) = add_task(
            State(state.clone()),
            Path(project.id.clone()),
            Json(AddTask {
                title: "Gather".into(),
                instructions: String::new(),
                acceptance_criteria: vec![],
                assigned_agent_id: None,
                depends_on: vec![],
                requires_approval: false,
            }),
        )
        .await
        .unwrap();

        let Json(second) = add_task(
            State(state.clone()),
            Path(project.id.clone()),
            Json(AddTask {
                title: "Write".into(),
                instructions: String::new(),
                acceptance_criteria: vec![],
                assigned_agent_id: None,
                depends_on: vec![first.id.clone()],
                requires_approval: false,
            }),
        )
        .await
        .unwrap();

        let Json(detail) = get(State(state), Path(project.id)).await.unwrap();
        let reloaded_first = detail.tasks.iter().find(|t| t.id == first.id).unwrap();
        let reloaded_second = detail.tasks.iter().find(|t| t.id == second.id).unwrap();
        assert_eq!(reloaded_first.state, TaskState::Ready);
        assert_eq!(reloaded_second.state, TaskState::Queued);
    }
}

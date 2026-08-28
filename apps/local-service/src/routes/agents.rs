//! Agent CRUD, templates, version history, the test console, and packages.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use otwono_agent_core::executor::{AgentExecutor, AgentTurn, ProviderExecutor};
use otwono_agent_core::{prompt, seed, templates};
use otwono_store::repo::activity::{ActivityRepo, NewActivity};
use otwono_store::repo::agents::{AgentRepo, NewAgent};
use otwono_store::repo::providers::ProviderRepo;
use otwono_types::agent::{Agent, AgentPackage, ApprovalPolicy, MemoryScope, ModelParameters};
use otwono_types::permission::Capability;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub include_archived: bool,
}

pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Vec<Agent>>> {
    Ok(Json(AgentRepo::new(&state.db).list(
        query.workspace_id.as_deref(),
        query.include_archived,
    )?))
}

#[derive(Debug, Serialize)]
pub struct TemplateSummary {
    pub key: &'static str,
    pub name: &'static str,
    pub role: &'static str,
    pub description: &'static str,
    pub icon: &'static str,
    pub capabilities: Vec<&'static str>,
    /// Set when this template has already been created.
    pub agent_id: Option<String>,
}

pub async fn list_templates(
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<TemplateSummary>>> {
    let repo = AgentRepo::new(&state.db);
    let mut summaries = Vec::with_capacity(templates::TEMPLATES.len());
    for template in templates::TEMPLATES {
        summaries.push(TemplateSummary {
            key: template.key,
            name: template.name,
            role: template.role,
            description: template.description,
            icon: template.icon,
            capabilities: template.capabilities.iter().map(|c| c.as_str()).collect(),
            agent_id: repo.get_by_template_key(template.key)?.map(|a| a.id),
        });
    }
    Ok(Json(summaries))
}

pub async fn seed_templates(State(state): State<AppState>) -> ApiResult<Json<Vec<Agent>>> {
    Ok(Json(
        seed::seed_templates(&state.db).map_err(ApiError::Internal)?,
    ))
}

#[derive(Debug, Deserialize)]
pub struct CreateAgent {
    pub name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_icon")]
    pub icon: String,
    #[serde(default)]
    pub system_instructions: String,
    #[serde(default)]
    pub provider_connection_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub parameters: ModelParameters,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub knowledge_source_ids: Vec<String>,
    #[serde(default = "default_memory")]
    pub memory_scope: MemoryScope,
    #[serde(default = "default_policy")]
    pub approval_policy: ApprovalPolicy,
    #[serde(default = "default_steps")]
    pub max_steps: u32,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u32,
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Start from a shipped template rather than a blank agent.
    #[serde(default)]
    pub from_template: Option<String>,
}

fn default_icon() -> String {
    "agent".into()
}
fn default_memory() -> MemoryScope {
    MemoryScope::Conversation
}
fn default_policy() -> ApprovalPolicy {
    ApprovalPolicy::OffDeviceOnly
}
fn default_steps() -> u32 {
    12
}
fn default_timeout() -> u32 {
    120
}

fn parse_capabilities(names: &[String]) -> ApiResult<Vec<Capability>> {
    names
        .iter()
        .map(|name| {
            Capability::parse(name).map_err(|_| {
                ApiError::BadRequest(format!(
                    "{name:?} is not a capability OTWONO knows about. Allowed: {}.",
                    Capability::ALL
                        .iter()
                        .map(|c| c.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
        })
        .collect()
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateAgent>,
) -> ApiResult<Json<Agent>> {
    let repo = AgentRepo::new(&state.db);

    let mut new = NewAgent {
        name: body.name,
        role: body.role,
        description: body.description,
        icon: body.icon,
        system_instructions: body.system_instructions,
        provider_connection_id: body.provider_connection_id,
        model: body.model,
        parameters: body.parameters,
        capabilities: parse_capabilities(&body.capabilities)?,
        knowledge_source_ids: body.knowledge_source_ids,
        memory_scope: body.memory_scope,
        approval_policy: body.approval_policy,
        max_steps: body.max_steps,
        timeout_seconds: body.timeout_seconds,
        workspace_id: body.workspace_id,
        template_key: None,
        is_template: false,
    };

    if let Some(key) = &body.from_template {
        let template = templates::find(key).ok_or_else(|| {
            ApiError::BadRequest(format!("{key:?} is not one of the shipped templates."))
        })?;
        if new.system_instructions.trim().is_empty() {
            new.system_instructions = template.system_instructions.into();
        }
        if new.role.trim().is_empty() {
            new.role = template.role.into();
        }
        if new.capabilities.is_empty() {
            new.capabilities = template.capabilities.to_vec();
        }
        new.memory_scope = template.memory_scope;
        new.approval_policy = template.approval_policy;
        new.max_steps = template.max_steps;
        new.timeout_seconds = template.timeout_seconds;
    }

    let agent = repo
        .create(new)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    ActivityRepo::new(&state.db)
        .record(NewActivity::user("agent.create").with_target("agent", &agent.id))?;
    Ok(Json(agent))
}

pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Agent>> {
    AgentRepo::new(&state.db)
        .get(&id)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("That agent"))
}

#[derive(Debug, Deserialize)]
pub struct UpdateAgent {
    pub name: Option<String>,
    pub role: Option<String>,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub system_instructions: Option<String>,
    pub provider_connection_id: Option<Option<String>>,
    pub model: Option<Option<String>>,
    pub parameters: Option<ModelParameters>,
    pub capabilities: Option<Vec<String>>,
    pub knowledge_source_ids: Option<Vec<String>>,
    pub memory_scope: Option<MemoryScope>,
    pub approval_policy: Option<ApprovalPolicy>,
    pub max_steps: Option<u32>,
    pub timeout_seconds: Option<u32>,
    pub workspace_id: Option<Option<String>>,
    pub note: Option<String>,
}

pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateAgent>,
) -> ApiResult<Json<Agent>> {
    let repo = AgentRepo::new(&state.db);
    let mut agent = repo
        .get(&id)?
        .ok_or_else(|| ApiError::not_found("That agent"))?;

    if let Some(value) = body.name {
        agent.name = value;
    }
    if let Some(value) = body.role {
        agent.role = value;
    }
    if let Some(value) = body.description {
        agent.description = value;
    }
    if let Some(value) = body.icon {
        agent.icon = value;
    }
    if let Some(value) = body.system_instructions {
        agent.system_instructions = value;
    }
    if let Some(value) = body.provider_connection_id {
        agent.provider_connection_id = value;
    }
    if let Some(value) = body.model {
        agent.model = value;
    }
    if let Some(value) = body.parameters {
        agent.parameters = value;
    }
    if let Some(value) = body.capabilities {
        agent.capabilities = parse_capabilities(&value)?;
    }
    if let Some(value) = body.knowledge_source_ids {
        agent.knowledge_source_ids = value;
    }
    if let Some(value) = body.memory_scope {
        agent.memory_scope = value;
    }
    if let Some(value) = body.approval_policy {
        agent.approval_policy = value;
    }
    if let Some(value) = body.max_steps {
        agent.max_steps = value;
    }
    if let Some(value) = body.timeout_seconds {
        agent.timeout_seconds = value;
    }
    if let Some(value) = body.workspace_id {
        agent.workspace_id = value;
    }

    let updated = repo
        .update(&agent, body.note.as_deref())
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(Json(updated))
}

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let repo = AgentRepo::new(&state.db);
    if repo.get(&id)?.is_none() {
        return Err(ApiError::not_found("That agent"));
    }
    repo.delete(&id)?;
    ActivityRepo::new(&state.db)
        .record(NewActivity::user("agent.delete").with_target("agent", &id))?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Debug, Serialize)]
pub struct VersionSummary {
    pub version: u32,
    pub note: Option<String>,
    pub created_at: String,
}

pub async fn versions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<VersionSummary>>> {
    Ok(Json(
        AgentRepo::new(&state.db)
            .versions(&id)?
            .into_iter()
            .map(|version| VersionSummary {
                version: version.version,
                note: version.note,
                created_at: version.created_at,
            })
            .collect(),
    ))
}

pub async fn restore_version(
    State(state): State<AppState>,
    Path((id, version)): Path<(String, u32)>,
) -> ApiResult<Json<Agent>> {
    AgentRepo::new(&state.db)
        .restore_version(&id, version)
        .map(Json)
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

pub async fn export(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<AgentPackage>> {
    let repo = AgentRepo::new(&state.db);
    let agent = repo
        .get(&id)?
        .ok_or_else(|| ApiError::not_found("That agent"))?;
    // Only the provider *kind* travels, never the connection or its endpoint.
    let hint = agent
        .provider_connection_id
        .as_deref()
        .and_then(|connection_id| {
            ProviderRepo::new(&state.db)
                .get(connection_id)
                .ok()
                .flatten()
        })
        .map(|connection| connection.kind.as_str().to_string());

    repo.export(&id, hint)
        .map(Json)
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

#[derive(Debug, Deserialize)]
pub struct ImportRequest {
    pub package: AgentPackage,
    #[serde(default)]
    pub workspace_id: Option<String>,
}

pub async fn import(
    State(state): State<AppState>,
    Json(body): Json<ImportRequest>,
) -> ApiResult<Json<Agent>> {
    let agent = AgentRepo::new(&state.db)
        .import(&body.package, body.workspace_id)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    ActivityRepo::new(&state.db).record(
        NewActivity::user("agent.import")
            .with_target("agent", &agent.id)
            .with_detail(serde_json::json!({ "name": agent.name })),
    )?;
    Ok(Json(agent))
}

#[derive(Debug, Deserialize)]
pub struct TestRequest {
    pub message: String,
    /// Override the agent's model for this one run.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TestResponse {
    pub output: String,
    pub finish_reason: String,
    pub token_estimate: Option<u32>,
    pub model: String,
    pub elapsed_ms: u64,
    /// The exact system message the agent was given, so the console shows what
    /// was actually sent rather than what the user assumes.
    pub system_message: String,
}

/// The agent test console: one turn, no tools, nothing persisted.
pub async fn test_console(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<TestRequest>,
) -> ApiResult<Json<TestResponse>> {
    let agent = AgentRepo::new(&state.db)
        .get(&id)?
        .ok_or_else(|| ApiError::not_found("That agent"))?;

    let connection = agent
        .provider_connection_id
        .as_deref()
        .and_then(|connection_id| {
            ProviderRepo::new(&state.db)
                .get(connection_id)
                .ok()
                .flatten()
        })
        .ok_or_else(|| {
            ApiError::BadRequest(format!(
                "{} has no model connection. Choose one on the agent's settings before testing it.",
                agent.name
            ))
        })?;
    let model = body
        .model
        .or_else(|| agent.model.clone())
        .or_else(|| connection.default_model.clone())
        .ok_or_else(|| ApiError::BadRequest(format!("{} has no model selected.", agent.name)))?;

    let mut parts = prompt::for_agent(&agent, None);
    // The console runs the agent without tools: it is for checking how the
    // agent writes, not for letting it act.
    parts.tools.clear();
    parts.user_message = body.message;
    let system_message = prompt::system_message(&parts);
    let messages = prompt::build(&parts);

    let provider = state.provider_for(&connection);
    let executor = ProviderExecutor::new(Arc::clone(&provider));
    let started = std::time::Instant::now();
    let outcome = executor
        .run(AgentTurn {
            agent_id: agent.id.clone(),
            agent_name: agent.name.clone(),
            model: model.clone(),
            messages,
            temperature: agent.parameters.temperature,
            max_output_tokens: agent.parameters.max_output_tokens,
            timeout_seconds: agent.timeout_seconds,
        })
        .await
        .map_err(|e| ApiError::Upstream(e.to_string()))?;

    Ok(Json(TestResponse {
        output: outcome.text,
        finish_reason: outcome.finish_reason,
        token_estimate: outcome.token_estimate,
        model,
        elapsed_ms: started.elapsed().as_millis() as u64,
        system_message,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seeded() -> AppState {
        let state = AppState::for_tests();
        let _ = seed_templates(State(state.clone())).await.unwrap();
        state
    }

    #[tokio::test]
    async fn templates_are_listed_with_whether_they_exist_yet() {
        let state = AppState::for_tests();
        let Json(before) = list_templates(State(state.clone())).await.unwrap();
        assert_eq!(before.len(), 10);
        assert!(before.iter().all(|t| t.agent_id.is_none()));

        let _ = seed_templates(State(state.clone())).await.unwrap();
        let Json(after) = list_templates(State(state)).await.unwrap();
        assert!(after.iter().all(|t| t.agent_id.is_some()));
    }

    #[tokio::test]
    async fn an_agent_created_from_a_template_inherits_its_settings() {
        let state = AppState::for_tests();
        let Json(agent) = create(
            State(state),
            Json(CreateAgent {
                name: "My Researcher".into(),
                from_template: Some("researcher".into()),
                role: String::new(),
                description: String::new(),
                icon: default_icon(),
                system_instructions: String::new(),
                provider_connection_id: None,
                model: None,
                parameters: ModelParameters::default(),
                capabilities: vec![],
                knowledge_source_ids: vec![],
                memory_scope: default_memory(),
                approval_policy: default_policy(),
                max_steps: default_steps(),
                timeout_seconds: default_timeout(),
                workspace_id: None,
            }),
        )
        .await
        .unwrap();

        assert_eq!(agent.name, "My Researcher");
        assert_eq!(agent.role, "Research");
        assert_eq!(agent.capabilities, vec![Capability::KnowledgeSearch]);
        assert!(agent.system_instructions.contains("gather evidence"));
    }

    #[tokio::test]
    async fn an_unknown_capability_is_refused_with_the_allowed_list() {
        let state = AppState::for_tests();
        let error = create(
            State(state),
            Json(CreateAgent {
                name: "Dangerous".into(),
                capabilities: vec!["run_shell".into()],
                from_template: None,
                role: String::new(),
                description: String::new(),
                icon: default_icon(),
                system_instructions: String::new(),
                provider_connection_id: None,
                model: None,
                parameters: ModelParameters::default(),
                knowledge_source_ids: vec![],
                memory_scope: default_memory(),
                approval_policy: default_policy(),
                max_steps: default_steps(),
                timeout_seconds: default_timeout(),
                workspace_id: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::BadRequest(ref m)
            if m.contains("run_shell") && m.contains("knowledge_search")));
    }

    #[tokio::test]
    async fn editing_an_agent_records_a_version_that_can_be_restored() {
        let state = seeded().await;
        let repo = AgentRepo::new(&state.db);
        let agent = repo.get_by_template_key("writer").unwrap().unwrap();
        let original = agent.system_instructions.clone();

        let _ = update(
            State(state.clone()),
            Path(agent.id.clone()),
            Json(UpdateAgent {
                system_instructions: Some("Write only in limericks.".into()),
                note: Some("experiment".into()),
                name: None,
                role: None,
                description: None,
                icon: None,
                provider_connection_id: None,
                model: None,
                parameters: None,
                capabilities: None,
                knowledge_source_ids: None,
                memory_scope: None,
                approval_policy: None,
                max_steps: None,
                timeout_seconds: None,
                workspace_id: None,
            }),
        )
        .await
        .unwrap();

        let Json(history) = versions(State(state.clone()), Path(agent.id.clone()))
            .await
            .unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].note.as_deref(), Some("experiment"));

        let Json(restored) = restore_version(State(state), Path((agent.id.clone(), 1)))
            .await
            .unwrap();
        assert_eq!(restored.system_instructions, original);
    }

    #[tokio::test]
    async fn a_package_round_trips_and_carries_no_local_identifiers() {
        let state = seeded().await;
        let agent = AgentRepo::new(&state.db)
            .get_by_template_key("researcher")
            .unwrap()
            .unwrap();

        let Json(package) = export(State(state.clone()), Path(agent.id.clone()))
            .await
            .unwrap();
        let json = serde_json::to_string(&package).unwrap();
        assert!(!json.contains(&agent.id));

        let Json(imported) = import(
            State(state),
            Json(ImportRequest {
                package,
                workspace_id: None,
            }),
        )
        .await
        .unwrap();
        assert_ne!(imported.id, agent.id);
        assert_eq!(imported.name, agent.name);
        assert!(imported.provider_connection_id.is_none());
    }

    #[tokio::test]
    async fn a_package_containing_a_credential_is_refused() {
        let state = seeded().await;
        let agent = AgentRepo::new(&state.db)
            .get_by_template_key("researcher")
            .unwrap()
            .unwrap();
        let Json(mut package) = export(State(state.clone()), Path(agent.id)).await.unwrap();
        package.parameters.extra.insert(
            "api_key".into(),
            serde_json::Value::String("sk-live".into()),
        );

        let error = import(
            State(state),
            Json(ImportRequest {
                package,
                workspace_id: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, ApiError::BadRequest(ref m) if m.contains("must not contain secrets"))
        );
    }

    #[tokio::test]
    async fn the_test_console_says_what_is_missing_rather_than_failing_obscurely() {
        let state = seeded().await;
        let agent = AgentRepo::new(&state.db)
            .get_by_template_key("writer")
            .unwrap()
            .unwrap();
        let error = test_console(
            State(state),
            Path(agent.id),
            Json(TestRequest {
                message: "Hello".into(),
                model: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::BadRequest(ref m)
            if m.contains("no model connection") && m.contains("agent's settings")));
    }
}

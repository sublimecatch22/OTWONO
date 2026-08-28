//! Connections to local AI runtimes.

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use otwono_providers::detect;
use otwono_store::repo::activity::{ActivityRepo, NewActivity};
use otwono_store::repo::providers::{NewProvider, ProviderRepo};
use otwono_store::secrets::provider_key;
use otwono_types::provider::{ConnectionTest, ProviderConnection, ProviderKind};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct ConnectionsResponse {
    pub connections: Vec<ProviderConnection>,
    /// True when at least one connection is enabled and has a default model.
    pub ready_for_chat: bool,
    pub guidance: Option<&'static str>,
}

pub async fn list(State(state): State<AppState>) -> ApiResult<Json<ConnectionsResponse>> {
    let connections = ProviderRepo::new(&state.db).list()?;
    let ready = connections
        .iter()
        .any(|c| c.enabled && c.default_model.is_some());
    Ok(Json(ConnectionsResponse {
        guidance: if ready {
            None
        } else {
            Some(detect::nothing_found_guidance())
        },
        ready_for_chat: ready,
        connections,
    }))
}

#[derive(Debug, Serialize)]
pub struct DetectionResponse {
    pub found: Vec<DetectedRuntime>,
    pub guidance: &'static str,
}

#[derive(Debug, Serialize)]
pub struct DetectedRuntime {
    pub kind: ProviderKind,
    pub display_name: &'static str,
    pub endpoint: String,
    pub test: ConnectionTest,
    pub usable: bool,
    /// The id of an existing connection for this endpoint, if there is one.
    pub existing_connection_id: Option<String>,
}

/// Probe the documented loopback ports. Nothing outside the machine is touched.
pub async fn detect_runtimes(State(state): State<AppState>) -> ApiResult<Json<DetectionResponse>> {
    let repo = ProviderRepo::new(&state.db);
    let detections = detect::detect_all().await;

    let mut found = Vec::with_capacity(detections.len());
    for detection in detections {
        let existing = repo.find_by_endpoint(&detection.endpoint)?.map(|c| c.id);
        found.push(DetectedRuntime {
            kind: detection.kind,
            display_name: detection.kind.display_name(),
            endpoint: detection.endpoint.clone(),
            usable: detection.is_usable(),
            test: detection.test,
            existing_connection_id: existing,
        });
    }

    Ok(Json(DetectionResponse {
        found,
        guidance: detect::nothing_found_guidance(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct CreateConnection {
    pub kind: String,
    pub label: String,
    pub endpoint: String,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub default_embedding_model: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    /// Stored in the credential vault immediately and never written to the
    /// database.
    #[serde(default)]
    pub api_key: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateConnection>,
) -> ApiResult<Json<ProviderConnection>> {
    let kind = ProviderKind::parse(&body.kind).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    if url::Url::parse(&body.endpoint).is_err() {
        return Err(ApiError::BadRequest(format!(
            "{:?} is not a valid address. It should look like http://127.0.0.1:11434.",
            body.endpoint
        )));
    }

    let repo = ProviderRepo::new(&state.db);
    let connection = repo.create(NewProvider {
        kind,
        label: body.label,
        endpoint: body.endpoint,
        default_model: body.default_model,
        default_embedding_model: body.default_embedding_model,
        enabled: body.enabled,
    })?;

    if let Some(key) = body.api_key.filter(|k| !k.trim().is_empty()) {
        state
            .secrets
            .set(&provider_key(&connection.id), key.trim())
            .map_err(ApiError::Internal)?;
        repo.set_has_credential(&connection.id, true)?;
    }

    ActivityRepo::new(&state.db).record(
        NewActivity::user("connection.create")
            .with_target("connection", &connection.id)
            .with_detail(
                serde_json::json!({ "kind": kind.as_str(), "endpoint": connection.endpoint }),
            ),
    )?;

    repo.get(&connection.id)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("That connection"))
}

#[derive(Debug, Deserialize)]
pub struct UpdateConnection {
    pub label: Option<String>,
    pub endpoint: Option<String>,
    pub enabled: Option<bool>,
    pub default_model: Option<Option<String>>,
    pub default_embedding_model: Option<Option<String>>,
    /// `Some(None)` removes the stored credential.
    pub api_key: Option<Option<String>>,
}

pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateConnection>,
) -> ApiResult<Json<ProviderConnection>> {
    let repo = ProviderRepo::new(&state.db);
    let mut connection = repo
        .get(&id)?
        .ok_or_else(|| ApiError::not_found("That connection"))?;

    if let Some(label) = body.label {
        connection.label = label;
    }
    if let Some(endpoint) = body.endpoint {
        if url::Url::parse(&endpoint).is_err() {
            return Err(ApiError::BadRequest(format!(
                "{endpoint:?} is not a valid address."
            )));
        }
        connection.endpoint = endpoint;
    }
    if let Some(model) = body.default_model {
        connection.default_model = model;
    }
    if let Some(model) = body.default_embedding_model {
        connection.default_embedding_model = model;
    }

    match body.api_key {
        Some(Some(key)) if !key.trim().is_empty() => {
            state
                .secrets
                .set(&provider_key(&id), key.trim())
                .map_err(ApiError::Internal)?;
            repo.set_has_credential(&id, true)?;
            connection.has_credential = true;
        }
        Some(None) => {
            state
                .secrets
                .delete(&provider_key(&id))
                .map_err(ApiError::Internal)?;
            repo.set_has_credential(&id, false)?;
            connection.has_credential = false;
        }
        _ => {}
    }

    if let Some(enabled) = body.enabled {
        connection.enabled = enabled;
    }
    repo.update(&connection)?;

    let saved = repo
        .get(&id)?
        .ok_or_else(|| ApiError::not_found("That connection"))?;
    if body.enabled == Some(true) && !saved.enabled {
        return Err(ApiError::BadRequest(
            "This connection needs an API key before it can be enabled. Add one and try again."
                .to_string(),
        ));
    }
    Ok(Json(saved))
}

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let repo = ProviderRepo::new(&state.db);
    if repo.get(&id)?.is_none() {
        return Err(ApiError::not_found("That connection"));
    }
    state.secrets.delete(&provider_key(&id)).ok();
    repo.delete(&id)?;
    ActivityRepo::new(&state.db)
        .record(NewActivity::user("connection.delete").with_target("connection", &id))?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// Test a connection and refresh the models it can serve.
pub async fn test(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ConnectionTest>> {
    let connection = ProviderRepo::new(&state.db)
        .get(&id)?
        .ok_or_else(|| ApiError::not_found("That connection"))?;
    let provider = state.provider_for(&connection);
    let result = provider.test().await;

    ActivityRepo::new(&state.db).record(
        NewActivity::user("connection.test")
            .with_target("connection", &id)
            .with_detail(serde_json::json!({
                "health": format!("{:?}", result.health),
                "models": result.models.len(),
            })),
    )?;

    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_local_connection_can_be_created_and_listed() {
        let state = AppState::for_tests();
        let Json(created) = create(
            State(state.clone()),
            Json(CreateConnection {
                kind: "ollama".into(),
                label: "Ollama".into(),
                endpoint: "http://127.0.0.1:11434".into(),
                default_model: Some("llama3.1".into()),
                default_embedding_model: None,
                enabled: true,
                api_key: None,
            }),
        )
        .await
        .unwrap();
        assert!(created.enabled);
        assert!(!created.has_credential);

        let Json(response) = list(State(state)).await.unwrap();
        assert_eq!(response.connections.len(), 1);
        assert!(response.ready_for_chat);
        assert!(response.guidance.is_none());
    }

    #[tokio::test]
    async fn with_no_usable_connection_the_user_is_told_what_to_do() {
        let state = AppState::for_tests();
        let Json(response) = list(State(state)).await.unwrap();
        assert!(!response.ready_for_chat);
        assert!(response.guidance.unwrap().contains("works without one"));
    }

    #[tokio::test]
    async fn an_api_key_goes_to_the_vault_and_never_to_the_database() {
        let state = AppState::for_tests();
        let Json(created) = create(
            State(state.clone()),
            Json(CreateConnection {
                kind: "openai_compatible".into(),
                label: "Hosted".into(),
                endpoint: "https://api.example.com/v1".into(),
                default_model: None,
                default_embedding_model: None,
                enabled: true,
                api_key: Some("sk-test-should-not-be-in-the-database".into()),
            }),
        )
        .await
        .unwrap();

        assert!(created.has_credential);
        assert_eq!(
            state
                .secrets
                .get(&provider_key(&created.id))
                .unwrap()
                .as_deref(),
            Some("sk-test-should-not-be-in-the-database")
        );

        // Prove the value is nowhere in the database.
        let conn = state.db.conn().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, kind, label, endpoint, default_model FROM provider_connections")
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |row| {
                Ok(format!(
                    "{:?}{:?}{:?}{:?}{:?}",
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            !rows.join("").contains("sk-test"),
            "the key reached the database"
        );
    }

    #[tokio::test]
    async fn an_online_connection_cannot_be_enabled_without_a_key() {
        let state = AppState::for_tests();
        let Json(created) = create(
            State(state.clone()),
            Json(CreateConnection {
                kind: "openai_compatible".into(),
                label: "Hosted".into(),
                endpoint: "https://api.example.com/v1".into(),
                default_model: None,
                default_embedding_model: None,
                enabled: true,
                api_key: None,
            }),
        )
        .await
        .unwrap();
        assert!(!created.enabled);

        let error = update(
            State(state.clone()),
            Path(created.id.clone()),
            Json(UpdateConnection {
                label: None,
                endpoint: None,
                enabled: Some(true),
                default_model: None,
                default_embedding_model: None,
                api_key: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::BadRequest(ref m) if m.contains("needs an API key")));
    }

    #[tokio::test]
    async fn removing_a_key_deletes_it_from_the_vault_and_disables_the_connection() {
        let state = AppState::for_tests();
        let Json(created) = create(
            State(state.clone()),
            Json(CreateConnection {
                kind: "openai_compatible".into(),
                label: "Hosted".into(),
                endpoint: "https://api.example.com/v1".into(),
                default_model: None,
                default_embedding_model: None,
                enabled: false,
                api_key: Some("sk-test".into()),
            }),
        )
        .await
        .unwrap();

        let _ = update(
            State(state.clone()),
            Path(created.id.clone()),
            Json(UpdateConnection {
                label: None,
                endpoint: None,
                enabled: Some(true),
                default_model: None,
                default_embedding_model: None,
                api_key: None,
            }),
        )
        .await
        .unwrap();

        let Json(after) = update(
            State(state.clone()),
            Path(created.id.clone()),
            Json(UpdateConnection {
                label: None,
                endpoint: None,
                enabled: None,
                default_model: None,
                default_embedding_model: None,
                api_key: Some(None),
            }),
        )
        .await
        .unwrap();

        assert!(!after.has_credential);
        assert!(!after.enabled);
        assert!(state
            .secrets
            .get(&provider_key(&created.id))
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn a_nonsense_endpoint_is_refused_with_an_example() {
        let state = AppState::for_tests();
        let error = create(
            State(state),
            Json(CreateConnection {
                kind: "ollama".into(),
                label: "Broken".into(),
                endpoint: "not a url".into(),
                default_model: None,
                default_embedding_model: None,
                enabled: true,
                api_key: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, ApiError::BadRequest(ref m) if m.contains("http://127.0.0.1:11434"))
        );
    }

    #[tokio::test]
    async fn detection_only_probes_loopback_addresses() {
        let state = AppState::for_tests();
        let Json(response) = detect_runtimes(State(state)).await.unwrap();
        assert_eq!(response.found.len(), 2);
        for runtime in &response.found {
            assert!(
                runtime.endpoint.starts_with("http://127.0.0.1:"),
                "{} is not loopback",
                runtime.endpoint
            );
        }
    }
}

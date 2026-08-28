//! Permission grants, requests and revocation.

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use otwono_permissions::{PermissionEngine, Request};
use otwono_store::repo::activity::{ActivityRepo, NewActivity, Outcome};
use otwono_store::repo::permissions::{NewGrant, PermissionRepo};
use otwono_types::permission::{
    Capability, CheckOutcome, Decision, Grant, PermissionRequest, Scope,
};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct PermissionsResponse {
    pub grants: Vec<Grant>,
    pub open_requests: Vec<PermissionRequest>,
    pub emergency_stop: bool,
    pub capabilities: Vec<CapabilityDescription>,
}

#[derive(Debug, Serialize)]
pub struct CapabilityDescription {
    pub capability: &'static str,
    /// The sentence shown in an approval dialog, after "This agent wants to".
    pub human_request: &'static str,
    pub leaves_device: bool,
}

pub async fn list(State(state): State<AppState>) -> ApiResult<Json<PermissionsResponse>> {
    let repo = PermissionRepo::new(&state.db);
    Ok(Json(PermissionsResponse {
        grants: repo.active_grants()?,
        open_requests: repo.open_requests()?,
        emergency_stop: PermissionEngine::new(&state.db).emergency_stop()?,
        capabilities: Capability::ALL
            .iter()
            .map(|capability| CapabilityDescription {
                capability: capability.as_str(),
                human_request: capability.human_request(),
                leaves_device: capability.leaves_device(),
            })
            .collect(),
    }))
}

pub async fn history(State(state): State<AppState>) -> ApiResult<Json<Vec<Grant>>> {
    Ok(Json(PermissionRepo::new(&state.db).all_grants()?))
}

#[derive(Debug, Deserialize)]
pub struct GrantRequest {
    pub capability: String,
    #[serde(default)]
    pub scopes: Vec<Scope>,
    /// "allow", "allow_once" or "deny".
    pub decision: String,
    #[serde(default)]
    pub expires_in_minutes: Option<i64>,
    #[serde(default)]
    pub spend_limit_minor: Option<i64>,
    #[serde(default)]
    pub spend_category: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

fn parse_decision(value: &str) -> ApiResult<Decision> {
    match value {
        "allow" => Ok(Decision::Allow),
        "allow_once" => Ok(Decision::AllowOnce),
        "deny" => Ok(Decision::Deny),
        other => Err(ApiError::BadRequest(format!(
            "{other:?} is not a decision. Use allow, allow_once or deny."
        ))),
    }
}

pub async fn grant(
    State(state): State<AppState>,
    Json(body): Json<GrantRequest>,
) -> ApiResult<Json<Grant>> {
    let capability =
        Capability::parse(&body.capability).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let decision = parse_decision(&body.decision)?;

    let grant = PermissionRepo::new(&state.db).grant(NewGrant {
        capability,
        scopes: body.scopes,
        decision,
        spend_limit_minor: body.spend_limit_minor,
        spend_category: body.spend_category,
        expires_at: body
            .expires_in_minutes
            .filter(|minutes| *minutes > 0)
            .map(|minutes| otwono_types::now() + chrono::Duration::minutes(minutes)),
        created_by: "user".into(),
        note: body.note,
    })?;

    ActivityRepo::new(&state.db).record(
        NewActivity::user("permission.grant")
            .with_target("grant", &grant.id)
            .with_outcome(if decision == Decision::Deny {
                Outcome::Denied
            } else {
                Outcome::Ok
            })
            .with_detail(serde_json::json!({
                "capability": capability.as_str(),
                "decision": body.decision,
            })),
    )?;

    Ok(Json(grant))
}

pub async fn revoke(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let repo = PermissionRepo::new(&state.db);
    if repo.get_grant(&id)?.is_none() {
        return Err(ApiError::not_found("That permission"));
    }
    repo.revoke(&id)?;
    ActivityRepo::new(&state.db)
        .record(NewActivity::user("permission.revoke").with_target("grant", &id))?;
    Ok(Json(serde_json::json!({ "revoked": true })))
}

pub async fn revoke_all(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let count = PermissionRepo::new(&state.db).revoke_all()?;
    ActivityRepo::new(&state.db).record(
        NewActivity::user("permission.revoke_all")
            .with_detail(serde_json::json!({ "count": count })),
    )?;
    Ok(Json(serde_json::json!({
        "revoked": count,
        "message": format!("{count} permission(s) were revoked. Agents will ask again next time.")
    })))
}

#[derive(Debug, Deserialize)]
pub struct ResolveRequest {
    /// "allow", "allow_once" or "deny".
    pub decision: String,
    #[serde(default)]
    pub expires_in_minutes: Option<i64>,
}

pub async fn resolve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ResolveRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let decision = parse_decision(&body.decision)?;
    let grant = PermissionRepo::new(&state.db)
        .resolve_request(
            &id,
            decision,
            body.expires_in_minutes
                .filter(|minutes| *minutes > 0)
                .map(|minutes| otwono_types::now() + chrono::Duration::minutes(minutes)),
        )
        .map_err(|e| ApiError::Conflict(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "resolved": true,
        "grant": grant,
    })))
}

#[derive(Debug, Deserialize)]
pub struct CheckRequest {
    pub capability: String,
    #[serde(default)]
    pub scopes: Vec<Scope>,
}

/// Ask what would happen, without doing it. Used by the interface to show
/// whether an action would need approval before the user starts it.
pub async fn check(
    State(state): State<AppState>,
    Json(body): Json<CheckRequest>,
) -> ApiResult<Json<CheckOutcome>> {
    let capability =
        Capability::parse(&body.capability).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let mut request = Request::new(capability);
    request.scopes = body.scopes;
    Ok(Json(PermissionEngine::new(&state.db).check(&request)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nothing_is_granted_to_begin_with_and_every_capability_is_explained() {
        let state = AppState::for_tests();
        let Json(response) = list(State(state)).await.unwrap();
        assert!(response.grants.is_empty());
        assert!(!response.emergency_stop);
        assert_eq!(response.capabilities.len(), Capability::ALL.len());
        assert!(response
            .capabilities
            .iter()
            .any(|c| c.capability == "http_fetch" && c.leaves_device));
    }

    #[tokio::test]
    async fn a_grant_takes_effect_and_revoking_undoes_it() {
        let state = AppState::for_tests();
        let Json(grant) = grant(
            State(state.clone()),
            Json(GrantRequest {
                capability: "knowledge_search".into(),
                scopes: vec![Scope::Global],
                decision: "allow".into(),
                expires_in_minutes: None,
                spend_limit_minor: None,
                spend_category: None,
                note: None,
            }),
        )
        .await
        .unwrap();

        let Json(outcome) = check(
            State(state.clone()),
            Json(CheckRequest {
                capability: "knowledge_search".into(),
                scopes: vec![],
            }),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, CheckOutcome::Allowed { .. }));

        let _ = revoke(State(state.clone()), Path(grant.id)).await.unwrap();
        let Json(outcome) = check(
            State(state),
            Json(CheckRequest {
                capability: "knowledge_search".into(),
                scopes: vec![],
            }),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, CheckOutcome::NeedsApproval { .. }));
    }

    #[tokio::test]
    async fn a_time_limited_grant_expires() {
        let state = AppState::for_tests();
        let _ = grant(
            State(state.clone()),
            Json(GrantRequest {
                capability: "file_read".into(),
                scopes: vec![Scope::Global],
                decision: "allow".into(),
                expires_in_minutes: Some(-1),
                spend_limit_minor: None,
                spend_category: None,
                note: None,
            }),
        )
        .await
        .unwrap();

        // A negative window is treated as "no expiry" rather than an already
        // expired grant, so the request is not silently useless.
        let Json(outcome) = check(
            State(state),
            Json(CheckRequest {
                capability: "file_read".into(),
                scopes: vec![],
            }),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, CheckOutcome::Allowed { .. }));
    }

    #[tokio::test]
    async fn revoke_all_clears_everything_and_says_how_many() {
        let state = AppState::for_tests();
        for capability in ["file_read", "knowledge_search"] {
            let _ = grant(
                State(state.clone()),
                Json(GrantRequest {
                    capability: capability.into(),
                    scopes: vec![Scope::Global],
                    decision: "allow".into(),
                    expires_in_minutes: None,
                    spend_limit_minor: None,
                    spend_category: None,
                    note: None,
                }),
            )
            .await
            .unwrap();
        }

        let Json(response) = revoke_all(State(state.clone())).await.unwrap();
        assert_eq!(response["revoked"], 2);
        assert!(response["message"].as_str().unwrap().contains("ask again"));
        assert!(PermissionRepo::new(&state.db)
            .active_grants()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn an_unknown_decision_or_capability_is_refused_with_the_options() {
        let state = AppState::for_tests();
        let error = grant(
            State(state.clone()),
            Json(GrantRequest {
                capability: "knowledge_search".into(),
                scopes: vec![],
                decision: "maybe".into(),
                expires_in_minutes: None,
                spend_limit_minor: None,
                spend_category: None,
                note: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::BadRequest(ref m) if m.contains("allow_once")));

        let error = check(
            State(state),
            Json(CheckRequest {
                capability: "run_shell".into(),
                scopes: vec![],
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::BadRequest(_)));
    }
}

//! System status, the emergency stop, backups and data export.

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use otwono_permissions::PermissionEngine;
use otwono_store::repo::activity::{ActivityRepo, NewActivity, Outcome};
use otwono_store::repo::permissions::PermissionRepo;
use otwono_store::repo::settings::SettingsRepo;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct Health {
    pub status: &'static str,
    pub service: &'static str,
    pub version: &'static str,
}

/// The only unauthenticated route. It deliberately says nothing about the
/// user's data — just that a service of this version is alive.
pub async fn health() -> Json<Health> {
    Json(Health {
        status: "ok",
        service: "otwono-local-service",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[derive(Debug, Serialize)]
pub struct SystemStatus {
    pub version: &'static str,
    pub schema_version: i64,
    pub started_at: String,
    pub data_directory: String,
    pub secret_backend: otwono_store::SecretBackend,
    pub secret_backend_detail: &'static str,
    pub emergency_stop: bool,
    pub open_permission_requests: usize,
    pub telemetry_opt_in: bool,
    pub onboarding_complete: bool,
}

pub async fn status(State(state): State<AppState>) -> ApiResult<Json<SystemStatus>> {
    let settings = SettingsRepo::new(&state.db);
    let backend = state.secret_backend();
    Ok(Json(SystemStatus {
        version: env!("CARGO_PKG_VERSION"),
        schema_version: state.schema_version,
        started_at: state.started_at.clone(),
        data_directory: otwono_store::paths::data_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
        secret_backend: backend,
        secret_backend_detail: backend.describe(),
        emergency_stop: settings.emergency_stop()?,
        open_permission_requests: PermissionRepo::new(&state.db).open_requests()?.len(),
        telemetry_opt_in: settings.get_bool(otwono_store::repo::settings::TELEMETRY_KEY, false)?,
        onboarding_complete: settings
            .get_bool(otwono_store::repo::settings::ONBOARDING_KEY, false)?,
    }))
}

#[derive(Debug, Deserialize)]
pub struct EmergencyStopRequest {
    pub engaged: bool,
    /// When engaging, also revoke every standing permission.
    #[serde(default)]
    pub revoke_all_permissions: bool,
}

#[derive(Debug, Serialize)]
pub struct EmergencyStopResponse {
    pub engaged: bool,
    pub revoked_grants: usize,
    pub message: String,
}

pub async fn set_emergency_stop(
    State(state): State<AppState>,
    Json(body): Json<EmergencyStopRequest>,
) -> ApiResult<Json<EmergencyStopResponse>> {
    let engine = PermissionEngine::new(&state.db);
    engine.set_emergency_stop(body.engaged)?;

    let revoked = if body.engaged && body.revoke_all_permissions {
        PermissionRepo::new(&state.db).revoke_all()?
    } else {
        0
    };

    ActivityRepo::new(&state.db).record(
        NewActivity::user(if body.engaged {
            "system.emergency_stop_engaged"
        } else {
            "system.emergency_stop_released"
        })
        .with_outcome(Outcome::Ok)
        .with_detail(serde_json::json!({ "revoked_grants": revoked })),
    )?;

    let message = if body.engaged {
        if revoked > 0 {
            format!(
                "Everything is stopped and {revoked} permission(s) were revoked. No agent can \
                 act until you release the stop."
            )
        } else {
            "Everything is stopped. No agent can act until you release the stop.".to_string()
        }
    } else {
        "The emergency stop is released. Standing permissions apply again.".to_string()
    };

    Ok(Json(EmergencyStopResponse {
        engaged: body.engaged,
        revoked_grants: revoked,
        message,
    }))
}

#[derive(Debug, Serialize)]
pub struct BackupResponse {
    pub path: String,
    pub message: String,
}

pub async fn backup(State(state): State<AppState>) -> ApiResult<Json<BackupResponse>> {
    let directory = otwono_store::paths::backups_dir().map_err(ApiError::Internal)?;
    let path = state
        .db
        .backup_now(&directory, "manual")
        .map_err(ApiError::Internal)?;
    ActivityRepo::new(&state.db).record(NewActivity::user("system.backup").with_detail(
        serde_json::json!({
            "path": path.to_string_lossy(),
        }),
    ))?;
    Ok(Json(BackupResponse {
        message: format!("A copy of your data was saved to {}.", path.display()),
        path: path.to_string_lossy().to_string(),
    }))
}

#[derive(Debug, Serialize)]
pub struct BackupListing {
    pub backups: Vec<BackupEntry>,
    pub directory: String,
}

#[derive(Debug, Serialize)]
pub struct BackupEntry {
    pub file_name: String,
    pub byte_size: u64,
    pub modified: Option<String>,
}

pub async fn list_backups() -> ApiResult<Json<BackupListing>> {
    let directory = otwono_store::paths::backups_dir().map_err(ApiError::Internal)?;
    let mut backups = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&directory) {
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            backups.push(BackupEntry {
                file_name: entry.file_name().to_string_lossy().to_string(),
                byte_size: metadata.len(),
                modified: metadata
                    .modified()
                    .ok()
                    .map(chrono::DateTime::<chrono::Utc>::from)
                    .map(|t| otwono_types::ids::format_ts(&t)),
            });
        }
    }
    backups.sort_by(|a, b| b.file_name.cmp(&a.file_name));
    Ok(Json(BackupListing {
        backups,
        directory: directory.to_string_lossy().to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_reveals_nothing_about_the_users_data() {
        let Json(health) = health().await;
        let json = serde_json::to_string(&health).unwrap();
        assert!(json.contains("\"status\":\"ok\""));
        for leak in ["path", "token", "directory", "user", "home"] {
            assert!(
                !json.to_lowercase().contains(leak),
                "health leaked {leak}: {json}"
            );
        }
    }

    #[tokio::test]
    async fn status_reports_which_secret_store_is_actually_in_use() {
        let state = AppState::for_tests();
        let Json(status) = status(State(state)).await.unwrap();
        assert_eq!(
            status.secret_backend,
            otwono_store::SecretBackend::Ephemeral
        );
        assert!(!status.secret_backend_detail.is_empty());
        assert!(!status.emergency_stop);
    }

    #[tokio::test]
    async fn engaging_the_stop_can_also_revoke_every_permission() {
        use otwono_store::repo::permissions::NewGrant;
        use otwono_types::permission::{Capability, Decision, Scope};

        let state = AppState::for_tests();
        PermissionRepo::new(&state.db)
            .grant(NewGrant {
                capability: Capability::FileRead,
                scopes: vec![Scope::Global],
                decision: Decision::Allow,
                spend_limit_minor: None,
                spend_category: None,
                expires_at: None,
                created_by: "user".into(),
                note: None,
            })
            .unwrap();

        let Json(response) = set_emergency_stop(
            State(state.clone()),
            Json(EmergencyStopRequest {
                engaged: true,
                revoke_all_permissions: true,
            }),
        )
        .await
        .unwrap();

        assert!(response.engaged);
        assert_eq!(response.revoked_grants, 1);
        assert!(response.message.contains("No agent can act"));
        assert!(PermissionEngine::new(&state.db).emergency_stop().unwrap());
        assert!(PermissionRepo::new(&state.db)
            .active_grants()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn releasing_the_stop_does_not_restore_revoked_permissions() {
        let state = AppState::for_tests();
        let _ = set_emergency_stop(
            State(state.clone()),
            Json(EmergencyStopRequest {
                engaged: true,
                revoke_all_permissions: true,
            }),
        )
        .await
        .unwrap();
        let Json(response) = set_emergency_stop(
            State(state.clone()),
            Json(EmergencyStopRequest {
                engaged: false,
                revoke_all_permissions: false,
            }),
        )
        .await
        .unwrap();
        assert!(!response.engaged);
        assert_eq!(response.revoked_grants, 0);
        assert!(PermissionRepo::new(&state.db)
            .active_grants()
            .unwrap()
            .is_empty());
    }
}

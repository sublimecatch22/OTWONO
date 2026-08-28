//! The optional link to an OTWONO relay account, and the pairing flow.

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use otwono_store::repo::account::{
    AccountRepo, RelayLink, ALLOWED_SCOPES, PAIRING_CODE_TTL_SECONDS,
};
use otwono_store::repo::activity::{ActivityRepo, NewActivity};
use otwono_store::secrets::relay_token_key;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct AccountStatus {
    pub linked: bool,
    pub link: Option<RelayLink>,
    pub available_scopes: Vec<&'static str>,
    /// What synchronisation does and does not send.
    pub privacy_notice: &'static str,
}

pub const PRIVACY_NOTICE: &str =
    "Linking an account synchronises only your profile, and the metadata of projects you have \
     explicitly marked for synchronisation. Your conversations, files, knowledge index and model \
     data stay on this device.";

pub async fn status(State(state): State<AppState>) -> ApiResult<Json<AccountStatus>> {
    let link = AccountRepo::new(&state.db).link()?;
    Ok(Json(AccountStatus {
        linked: link
            .as_ref()
            .map(|link| link.revoked_at.is_none() && link.account_id.is_some())
            .unwrap_or(false),
        link,
        available_scopes: ALLOWED_SCOPES.to_vec(),
        privacy_notice: PRIVACY_NOTICE,
    }))
}

#[derive(Debug, Deserialize)]
pub struct PairingRequest {
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PairingResponse {
    /// Shown once, in the desktop app. Only its hash is stored.
    pub code: String,
    pub scopes: Vec<String>,
    pub expires_at: String,
    pub expires_in_seconds: i64,
    pub instructions: String,
}

/// Mint a pairing code for a WordPress site to redeem.
pub async fn create_pairing_code(
    State(state): State<AppState>,
    Json(body): Json<PairingRequest>,
) -> ApiResult<Json<PairingResponse>> {
    let scopes = if body.scopes.is_empty() {
        vec!["profile.read".to_string(), "profile.write".to_string()]
    } else {
        body.scopes
    };

    let issued = AccountRepo::new(&state.db)
        .create_pairing_code(&scopes)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    ActivityRepo::new(&state.db).record(
        NewActivity::user("account.pairing_code_created")
            .with_detail(serde_json::json!({ "scopes": issued.scopes })),
    )?;

    Ok(Json(PairingResponse {
        instructions: format!(
            "Enter this code on your WordPress site under OTWONO AI → Connection within {} \
             minutes. It works once.",
            PAIRING_CODE_TTL_SECONDS / 60
        ),
        code: issued.code,
        scopes: issued.scopes,
        expires_at: issued.expires_at,
        expires_in_seconds: PAIRING_CODE_TTL_SECONDS,
    }))
}

#[derive(Debug, Deserialize)]
pub struct RedeemRequest {
    pub code: String,
    /// The site redeeming the code, recorded for the audit log.
    pub site: String,
}

#[derive(Debug, Serialize)]
pub struct RedeemResponse {
    pub scopes: Vec<String>,
    pub message: String,
}

/// Redeem a pairing code. In hosted mode the relay calls this; in local
/// development mode the WordPress site calls it directly.
pub async fn redeem_pairing_code(
    State(state): State<AppState>,
    Json(body): Json<RedeemRequest>,
) -> ApiResult<Json<RedeemResponse>> {
    let scopes = AccountRepo::new(&state.db)
        .consume_pairing_code(&body.code, &body.site)
        .map_err(|e| ApiError::Forbidden(e.to_string()))?;

    ActivityRepo::new(&state.db).record(NewActivity::user("account.paired").with_detail(
        serde_json::json!({
            "site": body.site,
            "scopes": scopes,
        }),
    ))?;

    Ok(Json(RedeemResponse {
        message: format!("{} is now paired with these scopes.", body.site),
        scopes,
    }))
}

#[derive(Debug, Deserialize)]
pub struct LinkRequest {
    pub relay_base_url: String,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub account_email: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Stored in the credential vault, never in the database.
    #[serde(default)]
    pub token: Option<String>,
}

pub async fn link(
    State(state): State<AppState>,
    Json(body): Json<LinkRequest>,
) -> ApiResult<Json<AccountStatus>> {
    if url::Url::parse(&body.relay_base_url).is_err() {
        return Err(ApiError::BadRequest(format!(
            "{:?} is not a valid address.",
            body.relay_base_url
        )));
    }
    for scope in &body.scopes {
        if !ALLOWED_SCOPES.contains(&scope.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "{scope:?} is not a scope OTWONO grants."
            )));
        }
    }

    let repo = AccountRepo::new(&state.db);
    let has_token = body.token.as_ref().is_some_and(|t| !t.trim().is_empty());
    let link = repo.upsert_link(
        &body.relay_base_url,
        body.account_id.as_deref(),
        body.account_email.as_deref(),
        body.display_name.as_deref(),
        &body.scopes,
        has_token,
    )?;

    if let Some(token) = body.token.filter(|t| !t.trim().is_empty()) {
        state
            .secrets
            .set(&relay_token_key(&link.id), token.trim())
            .map_err(ApiError::Internal)?;
    }

    ActivityRepo::new(&state.db).record(
        NewActivity::user("account.linked")
            .with_detail(serde_json::json!({ "relay": link.relay_base_url })),
    )?;

    status(State(state)).await
}

pub async fn unlink(State(state): State<AppState>) -> ApiResult<Json<AccountStatus>> {
    let repo = AccountRepo::new(&state.db);
    if let Some(link) = repo.link()? {
        state.secrets.delete(&relay_token_key(&link.id)).ok();
    }
    repo.revoke_link()?;
    repo.purge_expired_codes().ok();
    ActivityRepo::new(&state.db).record(NewActivity::user("account.unlinked"))?;
    status(State(state)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_account_is_linked_to_begin_with_and_the_privacy_promise_is_stated() {
        let state = AppState::for_tests();
        let Json(response) = status(State(state)).await.unwrap();
        assert!(!response.linked);
        assert!(response.privacy_notice.contains("stay on this device"));
        assert!(response.available_scopes.contains(&"profile.read"));
    }

    #[tokio::test]
    async fn a_pairing_code_works_once_and_says_how_long_it_lasts() {
        let state = AppState::for_tests();
        let Json(issued) = create_pairing_code(
            State(state.clone()),
            Json(PairingRequest { scopes: vec![] }),
        )
        .await
        .unwrap();
        assert!(issued.instructions.contains("works once"));
        assert_eq!(issued.expires_in_seconds, PAIRING_CODE_TTL_SECONDS);

        let Json(redeemed) = redeem_pairing_code(
            State(state.clone()),
            Json(RedeemRequest {
                code: issued.code.clone(),
                site: "https://example.com".into(),
            }),
        )
        .await
        .unwrap();
        assert!(redeemed.scopes.contains(&"profile.read".to_string()));

        let error = redeem_pairing_code(
            State(state),
            Json(RedeemRequest {
                code: issued.code,
                site: "https://example.com".into(),
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::Forbidden(ref m) if m.contains("already been used")));
    }

    #[tokio::test]
    async fn an_unknown_scope_cannot_be_requested_or_linked() {
        let state = AppState::for_tests();
        let error = create_pairing_code(
            State(state.clone()),
            Json(PairingRequest {
                scopes: vec!["knowledge.read".into()],
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::BadRequest(_)));

        let error = link(
            State(state),
            Json(LinkRequest {
                relay_base_url: "https://relay.example.com".into(),
                account_id: Some("acc_1".into()),
                account_email: None,
                display_name: None,
                scopes: vec!["admin".into()],
                token: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::BadRequest(ref m) if m.contains("not a scope")));
    }

    #[tokio::test]
    async fn linking_stores_the_token_in_the_vault_and_unlinking_removes_it() {
        let state = AppState::for_tests();
        let _ = link(
            State(state.clone()),
            Json(LinkRequest {
                relay_base_url: "https://relay.example.com".into(),
                account_id: Some("acc_1".into()),
                account_email: Some("person@example.com".into()),
                display_name: Some("A Person".into()),
                scopes: vec!["profile.read".into()],
                token: Some("relay-token-value".into()),
            }),
        )
        .await
        .unwrap();

        let link_id = AccountRepo::new(&state.db).link().unwrap().unwrap().id;
        assert_eq!(
            state
                .secrets
                .get(&relay_token_key(&link_id))
                .unwrap()
                .as_deref(),
            Some("relay-token-value")
        );

        let Json(after) = unlink(State(state.clone())).await.unwrap();
        assert!(!after.linked);
        assert!(state
            .secrets
            .get(&relay_token_key(&link_id))
            .unwrap()
            .is_none());
    }
}

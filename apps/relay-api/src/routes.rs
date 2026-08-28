//! The relay's HTTP surface.
//!
//! Registration and sign-in, email verification and password reset as
//! development adapters, device sessions, profiles with per-field visibility,
//! pairing, and the approved project metadata a WordPress site may read.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::auth::{
    check_rate_limit, hash_pairing_code, hash_password, hash_token, ip_prefix, mint_pairing_code,
    mint_token, validate_scopes, verify_password, ALLOWED_SCOPES,
};
use crate::RelayState;

// --------------------------------------------------------------- errors

#[derive(Debug)]
pub struct RelayError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl RelayError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            message: message.into(),
        }
    }
    fn unauthorised() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorised",
            message: "Sign in to continue.".into(),
        }
    }
    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "forbidden",
            message: message.into(),
        }
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: message.into(),
        }
    }
    fn too_many() -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "rate_limited",
            message: "Too many attempts. Try again later.".into(),
        }
    }
    fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(%error, "relay internal error");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: "Something went wrong. The details are in the relay's log.".into(),
        }
    }
}

impl From<anyhow::Error> for RelayError {
    fn from(error: anyhow::Error) -> Self {
        Self::internal(error)
    }
}
impl From<rusqlite::Error> for RelayError {
    fn from(error: rusqlite::Error) -> Self {
        Self::internal(error)
    }
}

impl IntoResponse for RelayError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({
                "error": { "code": self.code, "message": self.message }
            })),
        )
            .into_response()
    }
}

type RelayResult<T> = Result<T, RelayError>;

fn now() -> String {
    otwono_types::ids::format_ts(&otwono_types::now())
}

fn audit(
    state: &RelayState,
    account_id: Option<&str>,
    action: &str,
    detail: serde_json::Value,
    address: &SocketAddr,
) {
    let result = state.db.conn().and_then(|conn| {
        conn.execute(
            "INSERT INTO audit (id, account_id, action, detail, ip_prefix, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                otwono_types::new_id("aud"),
                account_id,
                action,
                detail.to_string(),
                ip_prefix(&address.to_string()),
                now()
            ],
        )
        .map_err(Into::into)
    });
    if let Err(error) = result {
        tracing::error!(%error, "could not write the relay audit log");
    }
}

// --------------------------------------------------------------- accounts

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub display_name: String,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub account_id: String,
    pub email_verified: bool,
    /// In this development build the verification link is returned rather than
    /// emailed, because no mail infrastructure is configured. Deploying with
    /// real mail replaces this field with a sent message.
    pub verification_token: Option<String>,
    pub notice: &'static str,
}

pub const NO_MAIL_NOTICE: &str =
    "This relay has no email service configured, so the verification token is returned here \
     instead of being sent. Configure mail before using this in production.";

fn normalise_email(email: &str) -> RelayResult<String> {
    let trimmed = email.trim().to_ascii_lowercase();
    if !trimmed.contains('@') || trimmed.len() < 3 || trimmed.len() > 320 {
        return Err(RelayError::bad_request(
            "That does not look like an email address.",
        ));
    }
    Ok(trimmed)
}

pub async fn register(
    State(state): State<RelayState>,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    Json(body): Json<RegisterRequest>,
) -> RelayResult<Json<RegisterResponse>> {
    if !check_rate_limit(
        &state.db,
        &format!("register:{}", ip_prefix(&address.to_string())),
        10,
        3600,
    )? {
        return Err(RelayError::too_many());
    }

    let email = normalise_email(&body.email)?;
    let password_hash =
        hash_password(&body.password).map_err(|e| RelayError::bad_request(e.to_string()))?;
    let account_id = otwono_types::new_id("acc");
    let verification = mint_token();

    let conn = state.db.conn()?;
    let inserted = conn.execute(
        "INSERT INTO accounts
           (id, email, password_hash, display_name, email_verified, verification_token, created_at)
         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
        params![
            account_id,
            email,
            password_hash,
            body.display_name.trim(),
            verification,
            now()
        ],
    );

    match inserted {
        Ok(_) => {}
        Err(rusqlite::Error::SqliteFailure(error, _)) if error.extended_code == 2067 => {
            // A duplicate address must not reveal that the account exists.
            return Err(RelayError::bad_request(
                "That address could not be registered. If you already have an account, sign in \
                 or reset your password.",
            ));
        }
        Err(error) => return Err(error.into()),
    }

    conn.execute(
        "INSERT INTO profiles (account_id, display_name, visibility, updated_at)
         VALUES (?1, ?2, '{}', ?3)",
        params![account_id, body.display_name.trim(), now()],
    )?;
    // The audit log needs a connection of its own. Holding this one across
    // that call would take two from the pool at once, which deadlocks as soon
    // as the pool is saturated.
    drop(conn);

    audit(
        &state,
        Some(&account_id),
        "account.register",
        serde_json::json!({}),
        &address,
    );

    Ok(Json(RegisterResponse {
        account_id,
        email_verified: false,
        verification_token: Some(verification),
        notice: NO_MAIL_NOTICE,
    }))
}

#[derive(Debug, Deserialize)]
pub struct VerifyRequest {
    pub token: String,
}

pub async fn verify_email(
    State(state): State<RelayState>,
    Json(body): Json<VerifyRequest>,
) -> RelayResult<Json<serde_json::Value>> {
    let changed = state.db.conn()?.execute(
        "UPDATE accounts SET email_verified = 1, verification_token = NULL
          WHERE verification_token = ?1",
        [body.token.trim()],
    )?;
    if changed == 0 {
        return Err(RelayError::bad_request(
            "That verification link is not valid.",
        ));
    }
    Ok(Json(serde_json::json!({ "verified": true })))
}

#[derive(Debug, Deserialize)]
pub struct SignInRequest {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub device_label: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SignInResponse {
    pub account_id: String,
    pub display_name: String,
    pub email_verified: bool,
    /// Returned once. Only its hash is stored.
    pub token: String,
    pub scopes: Vec<String>,
}

pub async fn sign_in(
    State(state): State<RelayState>,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    Json(body): Json<SignInRequest>,
) -> RelayResult<Json<SignInResponse>> {
    let email = normalise_email(&body.email)?;
    if !check_rate_limit(&state.db, &format!("signin:{email}"), 10, 900)? {
        return Err(RelayError::too_many());
    }

    let conn = state.db.conn()?;
    let row: Option<(String, String, String, i64)> = conn
        .query_row(
            "SELECT id, password_hash, display_name, email_verified FROM accounts WHERE email = ?1",
            [&email],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;

    // The same answer whether the account exists or the password is wrong.
    let Some((account_id, password_hash, display_name, verified)) = row else {
        return Err(RelayError::forbidden(
            "That email address and password do not match.",
        ));
    };
    if !verify_password(&body.password, &password_hash) {
        // Release the connection before the audit log takes one.
        drop(conn);
        audit(
            &state,
            Some(&account_id),
            "account.signin_failed",
            serde_json::json!({}),
            &address,
        );
        return Err(RelayError::forbidden(
            "That email address and password do not match.",
        ));
    }

    let scopes = if body.scopes.is_empty() {
        vec!["profile.read".to_string(), "profile.write".to_string()]
    } else {
        body.scopes
    };
    validate_scopes(&scopes).map_err(|e| RelayError::bad_request(e.to_string()))?;

    let token = mint_token();
    conn.execute(
        "INSERT INTO tokens (id, account_id, token_hash, kind, label, scopes, created_at)
         VALUES (?1, ?2, ?3, 'session', ?4, ?5, ?6)",
        params![
            otwono_types::new_id("tok"),
            account_id,
            hash_token(&token),
            body.device_label.trim(),
            serde_json::to_string(&scopes).unwrap_or_else(|_| "[]".into()),
            now()
        ],
    )?;
    drop(conn);

    audit(
        &state,
        Some(&account_id),
        "account.signin",
        serde_json::json!({}),
        &address,
    );

    Ok(Json(SignInResponse {
        account_id,
        display_name,
        email_verified: verified != 0,
        token,
        scopes,
    }))
}

/// The account behind a bearer token, and the scopes it holds.
struct Caller {
    account_id: String,
    scopes: Vec<String>,
}

fn authenticate(state: &RelayState, headers: &HeaderMap) -> RelayResult<Caller> {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .ok_or_else(RelayError::unauthorised)?;

    let conn = state.db.conn().map_err(RelayError::internal)?;
    let row: Option<(String, String, String, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT id, account_id, scopes, expires_at, revoked_at FROM tokens WHERE token_hash = ?1",
            [hash_token(presented)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()?;

    let Some((token_id, account_id, scopes, expires_at, revoked_at)) = row else {
        return Err(RelayError::unauthorised());
    };
    if revoked_at.is_some() {
        return Err(RelayError::forbidden("That session was signed out."));
    }
    if expires_at.is_some_and(|expiry| expiry <= now()) {
        return Err(RelayError::forbidden(
            "That session has expired. Sign in again.",
        ));
    }

    conn.execute(
        "UPDATE tokens SET last_used_at = ?2 WHERE id = ?1",
        params![token_id, now()],
    )?;

    Ok(Caller {
        account_id,
        scopes: serde_json::from_str(&scopes).unwrap_or_default(),
    })
}

fn require_scope(caller: &Caller, scope: &str) -> RelayResult<()> {
    if caller.scopes.iter().any(|held| held == scope) {
        Ok(())
    } else {
        Err(RelayError::forbidden(format!(
            "This session does not hold the {scope} permission."
        )))
    }
}

pub async fn sign_out(
    State(state): State<RelayState>,
    headers: HeaderMap,
) -> RelayResult<Json<serde_json::Value>> {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .ok_or_else(RelayError::unauthorised)?;

    state.db.conn()?.execute(
        "UPDATE tokens SET revoked_at = ?2 WHERE token_hash = ?1 AND revoked_at IS NULL",
        params![hash_token(presented), now()],
    )?;
    Ok(Json(serde_json::json!({ "signed_out": true })))
}

#[derive(Debug, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub scopes: Vec<String>,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked: bool,
}

pub async fn list_sessions(
    State(state): State<RelayState>,
    headers: HeaderMap,
) -> RelayResult<Json<Vec<SessionSummary>>> {
    let caller = authenticate(&state, &headers)?;
    let conn = state.db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT id, label, kind, scopes, created_at, last_used_at, revoked_at
           FROM tokens WHERE account_id = ?1 ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([caller.account_id], |row| {
        Ok(SessionSummary {
            id: row.get(0)?,
            label: row.get(1)?,
            kind: row.get(2)?,
            scopes: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or_default(),
            created_at: row.get(4)?,
            last_used_at: row.get(5)?,
            revoked: row.get::<_, Option<String>>(6)?.is_some(),
        })
    })?;
    Ok(Json(rows.collect::<rusqlite::Result<Vec<_>>>()?))
}

pub async fn revoke_session(
    State(state): State<RelayState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> RelayResult<Json<serde_json::Value>> {
    let caller = authenticate(&state, &headers)?;
    let changed = state.db.conn()?.execute(
        "UPDATE tokens SET revoked_at = ?3 WHERE id = ?1 AND account_id = ?2 AND revoked_at IS NULL",
        params![id, caller.account_id, now()],
    )?;
    if changed == 0 {
        return Err(RelayError::not_found("That session was not found."));
    }
    Ok(Json(serde_json::json!({ "revoked": true })))
}

#[derive(Debug, Deserialize)]
pub struct ResetRequest {
    pub email: String,
}

#[derive(Debug, Serialize)]
pub struct ResetResponse {
    /// Always the same, whether or not the address exists.
    pub message: &'static str,
    /// Development only, for the same reason as registration.
    pub reset_token: Option<String>,
    pub notice: &'static str,
}

pub async fn request_reset(
    State(state): State<RelayState>,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    Json(body): Json<ResetRequest>,
) -> RelayResult<Json<ResetResponse>> {
    if !check_rate_limit(
        &state.db,
        &format!("reset:{}", ip_prefix(&address.to_string())),
        10,
        3600,
    )? {
        return Err(RelayError::too_many());
    }
    let email = normalise_email(&body.email)?;
    let token = mint_token();
    let expires = otwono_types::ids::format_ts(&(otwono_types::now() + chrono::Duration::hours(1)));

    let changed = state.db.conn()?.execute(
        "UPDATE accounts SET reset_token = ?2, reset_expires_at = ?3 WHERE email = ?1",
        params![email, token, expires],
    )?;

    Ok(Json(ResetResponse {
        message: "If that address has an account, a reset link has been prepared.",
        reset_token: (changed > 0).then_some(token),
        notice: NO_MAIL_NOTICE,
    }))
}

#[derive(Debug, Deserialize)]
pub struct CompleteResetRequest {
    pub token: String,
    pub password: String,
}

pub async fn complete_reset(
    State(state): State<RelayState>,
    Json(body): Json<CompleteResetRequest>,
) -> RelayResult<Json<serde_json::Value>> {
    let hash = hash_password(&body.password).map_err(|e| RelayError::bad_request(e.to_string()))?;
    let conn = state.db.conn()?;

    let account: Option<String> = conn
        .query_row(
            "SELECT id FROM accounts WHERE reset_token = ?1 AND reset_expires_at > ?2",
            params![body.token.trim(), now()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(account_id) = account else {
        return Err(RelayError::bad_request(
            "That reset link is not valid or has expired.",
        ));
    };

    conn.execute(
        "UPDATE accounts SET password_hash = ?2, reset_token = NULL, reset_expires_at = NULL
          WHERE id = ?1",
        params![account_id, hash],
    )?;
    // A password change signs every session out.
    conn.execute(
        "UPDATE tokens SET revoked_at = ?2 WHERE account_id = ?1 AND revoked_at IS NULL",
        params![account_id, now()],
    )?;

    Ok(Json(serde_json::json!({
        "reset": true,
        "message": "Your password was changed and every session was signed out."
    })))
}

// --------------------------------------------------------------- profiles

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub biography: String,
    #[serde(default)]
    pub interests: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub portfolio_links: Vec<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    /// Field name -> public. Anything absent is private.
    #[serde(default)]
    pub visibility: std::collections::BTreeMap<String, bool>,
    #[serde(default)]
    pub is_ai_identity: bool,
    #[serde(default)]
    pub owner_account_id: Option<String>,
}

/// Fields whose visibility the user controls.
pub const PROFILE_FIELDS: &[&str] = &[
    "display_name",
    "biography",
    "interests",
    "capabilities",
    "portfolio_links",
    "avatar_url",
];

fn read_profile(state: &RelayState, account_id: &str) -> RelayResult<Option<Profile>> {
    let conn = state.db.conn()?;
    Ok(conn
        .query_row(
            "SELECT display_name, biography, interests, capabilities, portfolio_links,
                    avatar_url, visibility, is_ai_identity, owner_account_id
               FROM profiles WHERE account_id = ?1",
            [account_id],
            |row| {
                Ok(Profile {
                    display_name: row.get(0)?,
                    biography: row.get(1)?,
                    interests: serde_json::from_str(&row.get::<_, String>(2)?).unwrap_or_default(),
                    capabilities: serde_json::from_str(&row.get::<_, String>(3)?)
                        .unwrap_or_default(),
                    portfolio_links: serde_json::from_str(&row.get::<_, String>(4)?)
                        .unwrap_or_default(),
                    avatar_url: row.get(5)?,
                    visibility: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
                    is_ai_identity: row.get::<_, i64>(7)? != 0,
                    owner_account_id: row.get(8)?,
                })
            },
        )
        .optional()?)
}

pub async fn get_profile(
    State(state): State<RelayState>,
    headers: HeaderMap,
) -> RelayResult<Json<Profile>> {
    let caller = authenticate(&state, &headers)?;
    require_scope(&caller, "profile.read")?;
    read_profile(&state, &caller.account_id)?
        .map(Json)
        .ok_or_else(|| RelayError::not_found("That profile was not found."))
}

pub async fn put_profile(
    State(state): State<RelayState>,
    headers: HeaderMap,
    Json(mut body): Json<Profile>,
) -> RelayResult<Json<Profile>> {
    let caller = authenticate(&state, &headers)?;
    require_scope(&caller, "profile.write")?;

    body.visibility
        .retain(|field, _| PROFILE_FIELDS.contains(&field.as_str()));
    body.interests.truncate(40);
    body.capabilities.truncate(40);
    body.portfolio_links
        .retain(|link| link.starts_with("https://") || link.starts_with("http://"));
    body.portfolio_links.truncate(20);
    if body.biography.chars().count() > 4_000 {
        return Err(RelayError::bad_request(
            "A biography must be 4000 characters or fewer.",
        ));
    }

    state.db.conn()?.execute(
        "UPDATE profiles SET display_name = ?2, biography = ?3, interests = ?4,
                capabilities = ?5, portfolio_links = ?6, avatar_url = ?7, visibility = ?8,
                is_ai_identity = ?9, updated_at = ?10
          WHERE account_id = ?1",
        params![
            caller.account_id,
            body.display_name.trim(),
            body.biography,
            serde_json::to_string(&body.interests).unwrap_or_default(),
            serde_json::to_string(&body.capabilities).unwrap_or_default(),
            serde_json::to_string(&body.portfolio_links).unwrap_or_default(),
            body.avatar_url,
            serde_json::to_string(&body.visibility).unwrap_or_default(),
            body.is_ai_identity as i64,
            now()
        ],
    )?;

    read_profile(&state, &caller.account_id)?
        .map(Json)
        .ok_or_else(|| RelayError::not_found("That profile was not found."))
}

#[derive(Debug, Serialize)]
pub struct PublicProfile {
    pub account_id: String,
    pub fields: serde_json::Map<String, serde_json::Value>,
    /// Always present when the profile is an AI identity, so a reader is never
    /// left to assume they are talking to a person.
    pub identity_notice: Option<String>,
}

/// What another person may see. Only fields the owner marked public appear.
pub async fn public_profile(
    State(state): State<RelayState>,
    Path(account_id): Path<String>,
) -> RelayResult<Json<PublicProfile>> {
    let profile = read_profile(&state, &account_id)?
        .ok_or_else(|| RelayError::not_found("That profile was not found."))?;

    let mut fields = serde_json::Map::new();
    let public = |field: &str| profile.visibility.get(field).copied().unwrap_or(false);

    if public("display_name") {
        fields.insert(
            "display_name".into(),
            serde_json::json!(profile.display_name),
        );
    }
    if public("biography") {
        fields.insert("biography".into(), serde_json::json!(profile.biography));
    }
    if public("interests") {
        fields.insert("interests".into(), serde_json::json!(profile.interests));
    }
    if public("capabilities") {
        fields.insert(
            "capabilities".into(),
            serde_json::json!(profile.capabilities),
        );
    }
    if public("portfolio_links") {
        fields.insert(
            "portfolio_links".into(),
            serde_json::json!(profile.portfolio_links),
        );
    }
    if public("avatar_url") {
        fields.insert("avatar_url".into(), serde_json::json!(profile.avatar_url));
    }

    Ok(Json(PublicProfile {
        account_id,
        fields,
        identity_notice: profile.is_ai_identity.then(|| {
            "This is an AI identity, not a person. It acts for the account that owns it."
                .to_string()
        }),
    }))
}

// ---------------------------------------------------------------- pairing

#[derive(Debug, Deserialize)]
pub struct CreatePairingRequest {
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CreatePairingResponse {
    pub code: String,
    pub scopes: Vec<String>,
    pub expires_at: String,
}

/// A signed-in desktop application mints a code for a site to redeem.
pub async fn create_pairing(
    State(state): State<RelayState>,
    headers: HeaderMap,
    Json(body): Json<CreatePairingRequest>,
) -> RelayResult<Json<CreatePairingResponse>> {
    let caller = authenticate(&state, &headers)?;
    let scopes = if body.scopes.is_empty() {
        vec!["profile.read".to_string()]
    } else {
        body.scopes
    };
    validate_scopes(&scopes).map_err(|e| RelayError::bad_request(e.to_string()))?;

    // A pairing can never hold a scope the session itself does not hold.
    for scope in &scopes {
        require_scope(&caller, scope)?;
    }

    let code = mint_pairing_code();
    let expires =
        otwono_types::ids::format_ts(&(otwono_types::now() + chrono::Duration::minutes(5)));
    state.db.conn()?.execute(
        "INSERT INTO pairings (code_hash, account_id, scopes, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            hash_pairing_code(&code),
            caller.account_id,
            serde_json::to_string(&scopes).unwrap_or_default(),
            now(),
            expires
        ],
    )?;

    Ok(Json(CreatePairingResponse {
        code,
        scopes,
        expires_at: expires,
    }))
}

#[derive(Debug, Deserialize)]
pub struct RedeemPairingRequest {
    pub code: String,
    pub site: String,
}

#[derive(Debug, Serialize)]
pub struct RedeemPairingResponse {
    pub account_id: String,
    pub token: String,
    pub scopes: Vec<String>,
}

/// A site redeems a code once, for a scoped, revocable token.
pub async fn redeem_pairing(
    State(state): State<RelayState>,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    Json(body): Json<RedeemPairingRequest>,
) -> RelayResult<Json<RedeemPairingResponse>> {
    if !check_rate_limit(
        &state.db,
        &format!("pair:{}", ip_prefix(&address.to_string())),
        20,
        3600,
    )? {
        return Err(RelayError::too_many());
    }

    let hash = hash_pairing_code(&body.code);
    let conn = state.db.conn()?;
    let row: Option<(Option<String>, String, String, Option<String>)> = conn
        .query_row(
            "SELECT account_id, scopes, expires_at, consumed_at FROM pairings WHERE code_hash = ?1",
            [&hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;

    let Some((account_id, scopes, expires_at, consumed_at)) = row else {
        return Err(RelayError::forbidden("That pairing code is not valid."));
    };
    if consumed_at.is_some() {
        return Err(RelayError::forbidden(
            "That pairing code has already been used.",
        ));
    }
    if expires_at <= now() {
        return Err(RelayError::forbidden(
            "That pairing code has expired. Generate a new one in the desktop app.",
        ));
    }
    let Some(account_id) = account_id else {
        return Err(RelayError::forbidden(
            "That pairing code is not linked to an account.",
        ));
    };

    let consumed = conn.execute(
        "UPDATE pairings SET consumed_at = ?2, site = ?3 WHERE code_hash = ?1 AND consumed_at IS NULL",
        params![hash, now(), body.site],
    )?;
    if consumed == 0 {
        return Err(RelayError::forbidden(
            "That pairing code has already been used.",
        ));
    }

    let scope_list: Vec<String> = serde_json::from_str(&scopes).unwrap_or_default();
    let token = mint_token();
    conn.execute(
        "INSERT INTO tokens (id, account_id, token_hash, kind, label, scopes, created_at)
         VALUES (?1, ?2, ?3, 'site', ?4, ?5, ?6)",
        params![
            otwono_types::new_id("tok"),
            account_id,
            hash_token(&token),
            body.site,
            scopes,
            now()
        ],
    )?;
    drop(conn);

    audit(
        &state,
        Some(&account_id),
        "account.paired",
        serde_json::json!({ "site": body.site }),
        &address,
    );

    Ok(Json(RedeemPairingResponse {
        account_id,
        token,
        scopes: scope_list,
    }))
}

// ------------------------------------------------------ synced metadata

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncedProject {
    pub id: String,
    pub title: String,
    pub state: String,
    #[serde(default)]
    pub task_count: i64,
    #[serde(default)]
    pub completed_tasks: i64,
}

#[derive(Debug, Deserialize)]
pub struct SyncRequest {
    pub projects: Vec<SyncedProject>,
}

/// The desktop application pushes the metadata of projects the user marked for
/// synchronisation. Anything resembling content is refused rather than stored.
pub async fn sync_projects(
    State(state): State<RelayState>,
    headers: HeaderMap,
    Json(body): Json<SyncRequest>,
) -> RelayResult<Json<serde_json::Value>> {
    let caller = authenticate(&state, &headers)?;
    require_scope(&caller, "projects.read")?;

    if body.projects.len() > 500 {
        return Err(RelayError::bad_request("Too many projects in one request."));
    }

    let conn = state.db.conn()?;
    for project in &body.projects {
        if project.title.chars().count() > 300 {
            return Err(RelayError::bad_request(
                "A project title must be 300 characters or fewer. The relay stores titles and \
                 states, not content.",
            ));
        }
        conn.execute(
            "INSERT INTO synced_projects
               (id, account_id, title, state, task_count, completed_tasks, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
               title = excluded.title, state = excluded.state,
               task_count = excluded.task_count, completed_tasks = excluded.completed_tasks,
               updated_at = excluded.updated_at",
            params![
                project.id,
                caller.account_id,
                project.title,
                project.state,
                project.task_count,
                project.completed_tasks,
                now()
            ],
        )?;
    }

    Ok(Json(
        serde_json::json!({ "synchronised": body.projects.len() }),
    ))
}

pub async fn list_projects(
    State(state): State<RelayState>,
    headers: HeaderMap,
) -> RelayResult<Json<Vec<SyncedProject>>> {
    let caller = authenticate(&state, &headers)?;
    require_scope(&caller, "projects.read")?;

    let conn = state.db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT id, title, state, task_count, completed_tasks FROM synced_projects
          WHERE account_id = ?1 ORDER BY updated_at DESC",
    )?;
    let rows = stmt.query_map([caller.account_id], |row| {
        Ok(SyncedProject {
            id: row.get(0)?,
            title: row.get(1)?,
            state: row.get(2)?,
            task_count: row.get(3)?,
            completed_tasks: row.get(4)?,
        })
    })?;
    Ok(Json(rows.collect::<rusqlite::Result<Vec<_>>>()?))
}

// ------------------------------------------------------------------ misc

#[derive(Debug, Serialize)]
pub struct RelayHealth {
    pub status: &'static str,
    pub service: &'static str,
    pub version: &'static str,
    pub scopes: Vec<&'static str>,
    pub stores: &'static str,
}

pub async fn health() -> Json<RelayHealth> {
    Json(RelayHealth {
        status: "ok",
        service: "otwono-relay",
        version: env!("CARGO_PKG_VERSION"),
        scopes: ALLOWED_SCOPES.to_vec(),
        stores: "Accounts, profiles and approved project metadata only. This service cannot store \
             conversations, files, knowledge indexes or models.",
    })
}

pub fn router() -> Router<RelayState> {
    Router::new()
        .route("/health", get(health))
        .route("/v1/accounts", post(register))
        .route("/v1/accounts/verify", post(verify_email))
        .route("/v1/accounts/sign-in", post(sign_in))
        .route("/v1/accounts/sign-out", post(sign_out))
        .route("/v1/accounts/reset", post(request_reset))
        .route("/v1/accounts/reset/complete", post(complete_reset))
        .route("/v1/sessions", get(list_sessions))
        .route("/v1/sessions/{id}", delete(revoke_session))
        .route("/v1/profile", get(get_profile).put(put_profile))
        .route("/v1/profiles/{account_id}", get(public_profile))
        .route("/v1/pairings", post(create_pairing))
        .route("/v1/pairings/redeem", post(redeem_pairing))
        .route("/v1/projects", get(list_projects).post(sync_projects))
}

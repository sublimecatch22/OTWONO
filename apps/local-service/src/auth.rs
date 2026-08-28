//! The gate in front of the API.
//!
//! Two independent checks, both required:
//!
//! * a bearer token that only the desktop shell can read from the handshake
//!   file;
//! * an `Origin` that is on an allow-list.
//!
//! The second exists because a page open in the user's browser can reach
//! `127.0.0.1` and *will* attach no token — but a malicious page that somehow
//! learned the token would still be refused, because its origin is not ours.
//! Together they block cross-site request forgery and DNS rebinding.

use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;
use crate::state::AppState;

/// Origins the web view may present. `tauri://localhost` is the packaged app;
/// the `http://localhost:*` entries are the Vite development server.
pub fn default_allowed_origins() -> Vec<String> {
    vec![
        "tauri://localhost".to_string(),
        "https://tauri.localhost".to_string(),
        "http://localhost:1420".to_string(),
        "http://127.0.0.1:1420".to_string(),
    ]
}

fn origin_allowed(allowed: &[String], origin: &str) -> bool {
    allowed.iter().any(|candidate| candidate == origin)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

/// Reject a request that carries a disallowed `Origin`, whatever else it has.
pub fn check_origin(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        // No `Origin` at all: not a browser-initiated cross-site request. The
        // token check below still applies.
        return Ok(());
    };
    let origin = origin.to_str().unwrap_or_default();
    if origin_allowed(&state.allowed_origins, origin) {
        Ok(())
    } else {
        tracing::warn!(%origin, "refused a request from an origin that is not allowed");
        Err(ApiError::Forbidden(
            "This request came from a page OTWONO does not recognise, so it was refused."
                .to_string(),
        ))
    }
}

pub fn check_token(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    match bearer(headers) {
        Some(presented) if crate::runtime::tokens_match(&state.token, presented) => Ok(()),
        _ => Err(ApiError::Unauthorised),
    }
}

/// Middleware applied to everything except `/health`.
///
/// A pre-flight `OPTIONS` request never carries the token, so it is answered
/// here on the origin check alone rather than through a catch-all route —
/// which would otherwise turn an unknown path into a 405.
pub async fn guard(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let headers = request.headers().clone();
    if let Err(error) = check_origin(&state, &headers) {
        return error.into_response();
    }
    if request.method() == axum::http::Method::OPTIONS {
        return preflight_response(&state);
    }
    if let Err(error) = check_token(&state, &headers) {
        return error.into_response();
    }
    next.run(request).await
}

fn preflight_response(state: &AppState) -> Response {
    let allowed = state.allowed_origins.join(", ");
    (
        StatusCode::NO_CONTENT,
        [
            (
                "access-control-allow-methods",
                "GET, POST, PUT, DELETE, OPTIONS".to_string(),
            ),
            (
                "access-control-allow-headers",
                "authorization, content-type".to_string(),
            ),
            ("access-control-allow-origin", allowed),
            ("access-control-max-age", "600".to_string()),
        ],
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(origin: Option<&str>, token: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(origin) = origin {
            headers.insert(
                axum::http::header::ORIGIN,
                HeaderValue::from_str(origin).unwrap(),
            );
        }
        if let Some(token) = token {
            headers.insert(
                axum::http::header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
            );
        }
        headers
    }

    fn state() -> AppState {
        AppState::for_tests()
    }

    #[test]
    fn the_packaged_app_and_the_development_server_are_allowed() {
        let allowed = default_allowed_origins();
        for origin in [
            "tauri://localhost",
            "http://localhost:1420",
            "http://127.0.0.1:1420",
        ] {
            assert!(
                origin_allowed(&allowed, origin),
                "{origin} should be allowed"
            );
        }
    }

    #[test]
    fn an_unknown_origin_is_refused_even_with_the_right_token() {
        let state = state();
        for hostile in [
            "https://evil.example.com",
            "http://localhost:3000",
            "null",
            "http://otwono.com.evil.example",
        ] {
            let result = check_origin(&state, &headers(Some(hostile), Some(&state.token)));
            assert!(result.is_err(), "{hostile} should have been refused");
        }
    }

    #[test]
    fn a_request_with_no_origin_passes_the_origin_check_but_still_needs_a_token() {
        let state = state();
        assert!(check_origin(&state, &headers(None, None)).is_ok());
        assert!(check_token(&state, &headers(None, None)).is_err());
        assert!(check_token(&state, &headers(None, Some(&state.token))).is_ok());
    }

    #[test]
    fn a_wrong_or_missing_token_is_refused() {
        let state = state();
        assert!(check_token(&state, &headers(None, Some("not-the-token"))).is_err());
        assert!(check_token(&state, &headers(None, Some(""))).is_err());
        assert!(check_token(&state, &HeaderMap::new()).is_err());

        let mut malformed = HeaderMap::new();
        malformed.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&state.token).unwrap(),
        );
        assert!(
            check_token(&state, &malformed).is_err(),
            "a bare token without the Bearer scheme is not accepted"
        );
    }

    #[test]
    fn a_token_that_is_a_prefix_of_the_real_one_is_refused() {
        let state = state();
        let truncated = &state.token[..state.token.len() - 3];
        assert!(check_token(&state, &headers(None, Some(truncated))).is_err());
    }
}

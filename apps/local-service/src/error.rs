//! API errors.
//!
//! Every failure becomes a JSON body with a stable machine code and a sentence
//! written for the person reading the screen.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ApiErrorBody {
    pub error: ApiErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct ApiErrorDetail {
    /// Stable identifier the client can branch on.
    pub code: &'static str,
    /// A sentence for the user.
    pub message: String,
    /// True when trying again might work.
    pub retryable: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    Conflict(String),

    #[error("{0}")]
    Forbidden(String),

    #[error("authentication is required")]
    Unauthorised,

    #[error("{0}")]
    Upstream(String),

    #[error("{0}")]
    Internal(#[from] anyhow::Error),
}

impl ApiError {
    pub fn not_found(what: impl std::fmt::Display) -> Self {
        Self::NotFound(format!("{what} was not found."))
    }

    fn parts(&self) -> (StatusCode, &'static str, bool) {
        match self {
            Self::NotFound(_) => (StatusCode::NOT_FOUND, "not_found", false),
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request", false),
            Self::Conflict(_) => (StatusCode::CONFLICT, "conflict", false),
            Self::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden", false),
            Self::Unauthorised => (StatusCode::UNAUTHORIZED, "unauthorised", false),
            Self::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream_failed", true),
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error", true),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, retryable) = self.parts();
        let message = match &self {
            // An internal error's cause is logged, not shown: it may name a
            // path or a query the user did not ask about.
            Self::Internal(error) => {
                tracing::error!(%error, "unhandled internal error");
                "Something went wrong inside OTWONO. The details are in the application log."
                    .to_string()
            }
            other => other.to_string(),
        };
        (
            status,
            Json(ApiErrorBody {
                error: ApiErrorDetail {
                    code,
                    message,
                    retryable,
                },
            }),
        )
            .into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// A response whose body is text with an explicit content type. Named rather
/// than returned as `impl IntoResponse` so callers — including tests — can see
/// and destructure it.
pub type TextResponse = ([(axum::http::HeaderName, &'static str); 1], String);

pub fn markdown(body: String) -> TextResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/markdown; charset=utf-8",
        )],
        body,
    )
}

pub fn plain_text(body: String) -> TextResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_of(error: ApiError) -> (StatusCode, serde_json::Value) {
        let response = error.into_response();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn a_not_found_error_carries_a_stable_code_and_a_readable_message() {
        let (status, body) = body_of(ApiError::not_found("That agent")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "not_found");
        assert_eq!(body["error"]["message"], "That agent was not found.");
        assert_eq!(body["error"]["retryable"], false);
    }

    #[tokio::test]
    async fn an_internal_error_does_not_leak_its_cause_to_the_client() {
        let (status, body) = body_of(ApiError::Internal(anyhow::anyhow!(
            "no such file: /home/u/secret/diary.md"
        )))
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let message = body["error"]["message"].as_str().unwrap();
        assert!(!message.contains("diary.md"), "leaked a path: {message}");
        assert!(message.contains("application log"));
        assert_eq!(body["error"]["retryable"], true);
    }

    #[tokio::test]
    async fn upstream_failures_are_marked_retryable_and_authentication_is_not() {
        let (status, body) = body_of(ApiError::Upstream("Ollama did not answer.".into())).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["error"]["retryable"], true);

        let (status, body) = body_of(ApiError::Unauthorised).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["retryable"], false);
    }
}

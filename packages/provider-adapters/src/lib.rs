//! Provider adapters.
//!
//! One trait, three implementations, and a capability model that refuses to
//! assume. Nothing here reads or writes the database; connections and
//! credentials are passed in by the caller.

pub mod capability;
pub mod detect;
pub mod ollama;
pub mod openai_compatible;

use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::Stream;
use serde::{Deserialize, Serialize};

use otwono_types::provider::{ConnectionTest, ModelInfo, ProviderKind};

/// One turn in a request to a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatTurn {
    pub role: String,
    pub content: String,
}

impl ChatTurn {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatTurn>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub stop: Vec<String>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatTurn>) -> Self {
        Self {
            model: model.into(),
            messages,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            stop: Vec::new(),
        }
    }
}

/// A piece of a streamed response.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatDelta {
    Text(String),
    Done {
        finish_reason: String,
        token_estimate: Option<u32>,
    },
}

pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatDelta>> + Send>>;

/// Why a request failed, in terms the UI can act on.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("could not reach {endpoint}: {detail}")]
    Unreachable { endpoint: String, detail: String },

    #[error("{endpoint} requires a credential")]
    AuthenticationRequired { endpoint: String },

    #[error("model {model:?} is not available on this connection")]
    ModelNotFound { model: String },

    #[error("{endpoint} returned {status}: {body}")]
    Upstream {
        endpoint: String,
        status: u16,
        body: String,
    },

    #[error("this connection does not support {feature}")]
    Unsupported { feature: &'static str },
}

impl ProviderError {
    /// Whether the UI should offer a Retry button.
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Unreachable { .. } | Self::Upstream { .. })
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn endpoint(&self) -> &str;

    /// Models the runtime is serving, with discovered capabilities.
    async fn list_models(&self) -> Result<Vec<ModelInfo>>;

    /// Reachability plus a short capability test. Never returns `Err` for an
    /// unreachable endpoint — that is a result, not a failure.
    async fn test(&self) -> ConnectionTest;

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream>;

    /// Embeddings, when the model supports them.
    async fn embed(&self, model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Build the adapter for a connection.
pub fn adapter_for(
    kind: ProviderKind,
    endpoint: &str,
    api_key: Option<String>,
) -> Box<dyn Provider> {
    match kind {
        ProviderKind::Ollama => Box::new(ollama::OllamaProvider::new(endpoint)),
        ProviderKind::LmStudio => Box::new(openai_compatible::OpenAiCompatibleProvider::new(
            endpoint,
            api_key,
            ProviderKind::LmStudio,
        )),
        ProviderKind::OpenAiCompatible => {
            Box::new(openai_compatible::OpenAiCompatibleProvider::new(
                endpoint,
                api_key,
                ProviderKind::OpenAiCompatible,
            ))
        }
    }
}

/// Shared HTTP client. Timeouts are deliberate: a hung local runtime must not
/// hang the application.
pub(crate) fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(300))
        .user_agent(concat!("OTWONO-AI/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

/// A rough token count used for budgeting and for the UI's context meter.
/// It is an estimate and is labelled as one everywhere it is shown.
pub fn estimate_tokens(text: &str) -> u32 {
    // Approximately four characters per token for English prose; whitespace
    // runs are collapsed so indentation does not inflate the figure.
    let significant = text.split_whitespace().map(|w| w.len() + 1).sum::<usize>();
    ((significant as f32) / 4.0).ceil() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimates_grow_with_text_and_never_panic() {
        assert_eq!(estimate_tokens(""), 0);
        let short = estimate_tokens("hello world");
        let long = estimate_tokens(&"hello world ".repeat(100));
        assert!(short > 0);
        assert!(long > short * 50);
    }

    #[test]
    fn unreachable_and_upstream_failures_are_worth_retrying() {
        assert!(ProviderError::Unreachable {
            endpoint: "http://127.0.0.1:11434".into(),
            detail: "connection refused".into()
        }
        .is_retryable());
        assert!(!ProviderError::AuthenticationRequired {
            endpoint: "https://api.example.com".into()
        }
        .is_retryable());
        assert!(!ProviderError::Unsupported {
            feature: "embeddings"
        }
        .is_retryable());
    }

    #[test]
    fn the_right_adapter_is_built_for_each_kind() {
        assert_eq!(
            adapter_for(ProviderKind::Ollama, "http://127.0.0.1:11434", None).kind(),
            ProviderKind::Ollama
        );
        assert_eq!(
            adapter_for(ProviderKind::LmStudio, "http://127.0.0.1:1234", None).kind(),
            ProviderKind::LmStudio
        );
        assert_eq!(
            adapter_for(
                ProviderKind::OpenAiCompatible,
                "https://api.example.com/v1",
                Some("k".into())
            )
            .kind(),
            ProviderKind::OpenAiCompatible
        );
    }
}

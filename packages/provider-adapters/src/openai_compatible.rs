//! The OpenAI-compatible HTTP API, used by LM Studio, llama.cpp's server,
//! vLLM, LocalAI and hosted gateways.
//!
//! Endpoints used: `/models`, `/chat/completions` (server-sent events) and
//! `/embeddings`, all relative to the configured base URL.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;

use otwono_types::provider::{
    CapabilitySource, ConnectionHealth, ConnectionTest, ModelInfo, ProviderKind,
};

use crate::{capability, ChatDelta, ChatRequest, ChatStream, Provider, ProviderError};

pub struct OpenAiCompatibleProvider {
    /// Base URL including any `/v1` segment.
    endpoint: String,
    api_key: Option<String>,
    kind: ProviderKind,
    client: reqwest::Client,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    total_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    #[serde(default)]
    data: Vec<EmbeddingEntry>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingEntry {
    #[serde(default)]
    embedding: Vec<f32>,
}

impl OpenAiCompatibleProvider {
    pub fn new(endpoint: &str, api_key: Option<String>, kind: ProviderKind) -> Self {
        let trimmed = endpoint.trim_end_matches('/');
        // LM Studio is usually given as `http://127.0.0.1:1234`; the API lives
        // under `/v1`. Accept either form so the user cannot get it wrong.
        let endpoint = if trimmed.ends_with("/v1") {
            trimmed.to_string()
        } else {
            format!("{trimmed}/v1")
        };
        Self {
            endpoint,
            api_key,
            kind,
            client: crate::http_client().expect("HTTP client construction cannot fail"),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.endpoint)
    }

    fn request(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => builder.bearer_auth(key),
            None => builder,
        }
    }

    /// Ask the endpoint to embed one short string. A success proves embeddings
    /// work; anything else means we must not claim they do.
    async fn probe_embeddings(&self, model: &str) -> bool {
        let response = self
            .request(self.client.post(self.url("/embeddings")))
            .json(&json!({ "model": model, "input": "probe" }))
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => response
                .json::<EmbeddingsResponse>()
                .await
                .map(|parsed| parsed.data.first().is_some_and(|e| !e.embedding.is_empty()))
                .unwrap_or(false),
            _ => false,
        }
    }
}

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .request(self.client.get(self.url("/models")))
            .send()
            .await
            .map_err(|e| ProviderError::Unreachable {
                endpoint: self.endpoint.clone(),
                detail: e.to_string(),
            })?;

        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ProviderError::AuthenticationRequired {
                endpoint: self.endpoint.clone(),
            }
            .into());
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Upstream {
                endpoint: self.endpoint.clone(),
                status: status.as_u16(),
                body: body.chars().take(400).collect(),
            }
            .into());
        }

        let parsed: ModelsResponse = response.json().await?;
        let mut models: Vec<ModelInfo> = parsed
            .data
            .into_iter()
            .map(|entry| ModelInfo {
                // The protocol advertises no capabilities, so everything here
                // is an inference and is labelled as one.
                capabilities: capability::infer_from_name(&entry.id, true),
                capability_source: CapabilitySource::Inferred,
                display_name: entry.id.clone(),
                id: entry.id,
                parameter_size: None,
                quantization: None,
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }

    async fn test(&self) -> ConnectionTest {
        let started = std::time::Instant::now();
        match self.list_models().await {
            Ok(mut models) => {
                // Confirm by probing rather than guessing, for the first model
                // that looks like an embedding model.
                if let Some(index) = models
                    .iter()
                    .position(|m| capability::looks_like_embedding_model(&m.id))
                {
                    let works = self.probe_embeddings(&models[index].id).await;
                    let (capabilities, source) = capability::with_probed_embeddings(
                        models[index].capabilities.clone(),
                        works,
                        models[index].capability_source,
                    );
                    models[index].capabilities = capabilities;
                    models[index].capability_source = source;
                }

                let detail = if models.is_empty() {
                    format!(
                        "{} answered, but is not serving any model. Load a model in {} and \
                         test again.",
                        self.endpoint,
                        self.kind.display_name()
                    )
                } else {
                    format!(
                        "{} is serving {} model{}. Capabilities other than chat are inferred \
                         from model names, because this API does not report them.",
                        self.kind.display_name(),
                        models.len(),
                        if models.len() == 1 { "" } else { "s" }
                    )
                };
                ConnectionTest {
                    health: ConnectionHealth::Reachable,
                    detail,
                    models,
                    latency_ms: Some(started.elapsed().as_millis() as u64),
                }
            }
            Err(error) => {
                let health = if error
                    .downcast_ref::<ProviderError>()
                    .is_some_and(|e| matches!(e, ProviderError::AuthenticationRequired { .. }))
                {
                    ConnectionHealth::AuthenticationRequired
                } else {
                    ConnectionHealth::Unreachable
                };
                let detail = match health {
                    ConnectionHealth::AuthenticationRequired => format!(
                        "{} needs an API key. Add one in Connections; it is stored in your \
                         operating system's credential manager, not in the OTWONO database.",
                        self.endpoint
                    ),
                    _ => format!("Could not reach {}: {error}", self.endpoint),
                };
                ConnectionTest {
                    health,
                    detail,
                    models: Vec::new(),
                    latency_ms: None,
                }
            }
        }
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(request.model.clone()));
        body.insert("messages".into(), json!(request.messages));
        body.insert("stream".into(), json!(true));
        if let Some(temperature) = request.temperature {
            body.insert("temperature".into(), json!(temperature));
        }
        if let Some(top_p) = request.top_p {
            body.insert("top_p".into(), json!(top_p));
        }
        if let Some(max_tokens) = request.max_output_tokens {
            body.insert("max_tokens".into(), json!(max_tokens));
        }
        if !request.stop.is_empty() {
            body.insert("stop".into(), json!(request.stop));
        }

        let response = self
            .request(self.client.post(self.url("/chat/completions")))
            .json(&serde_json::Value::Object(body))
            .send()
            .await
            .map_err(|e| ProviderError::Unreachable {
                endpoint: self.endpoint.clone(),
                detail: e.to_string(),
            })?;

        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ProviderError::AuthenticationRequired {
                endpoint: self.endpoint.clone(),
            }
            .into());
        }
        if status.as_u16() == 404 {
            return Err(ProviderError::ModelNotFound {
                model: request.model,
            }
            .into());
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Upstream {
                endpoint: self.endpoint.clone(),
                status: status.as_u16(),
                body: body.chars().take(400).collect(),
            }
            .into());
        }

        let stream = response.bytes_stream();
        let parsed = futures_util::stream::unfold(
            (stream, String::new(), false),
            |(mut stream, mut buffer, mut finished)| async move {
                loop {
                    if finished {
                        return None;
                    }
                    if let Some(newline) = buffer.find('\n') {
                        let line: String = buffer.drain(..=newline).collect();
                        let line = line.trim().to_string();
                        // Server-sent events: blank lines and comments are
                        // keep-alives and carry no data.
                        if line.is_empty() || line.starts_with(':') {
                            continue;
                        }
                        let Some(payload) = line.strip_prefix("data:") else {
                            continue;
                        };
                        let payload = payload.trim();
                        if payload == "[DONE]" {
                            return Some((
                                Ok(ChatDelta::Done {
                                    finish_reason: "stop".into(),
                                    token_estimate: None,
                                }),
                                (stream, buffer, true),
                            ));
                        }
                        match serde_json::from_str::<StreamChunk>(payload) {
                            Ok(chunk) => {
                                if let Some(choice) = chunk.choices.first() {
                                    if let Some(text) =
                                        choice.delta.as_ref().and_then(|d| d.content.clone())
                                    {
                                        if !text.is_empty() {
                                            return Some((
                                                Ok(ChatDelta::Text(text)),
                                                (stream, buffer, finished),
                                            ));
                                        }
                                    }
                                    if let Some(reason) = choice.finish_reason.clone() {
                                        return Some((
                                            Ok(ChatDelta::Done {
                                                finish_reason: reason,
                                                token_estimate: chunk
                                                    .usage
                                                    .and_then(|u| u.total_tokens),
                                            }),
                                            (stream, buffer, true),
                                        ));
                                    }
                                }
                                continue;
                            }
                            Err(error) => {
                                return Some((
                                    Err(anyhow!("could not read the streamed response: {error}")),
                                    (stream, buffer, true),
                                ));
                            }
                        }
                    }

                    match stream.next().await {
                        Some(Ok(bytes)) => buffer.push_str(&String::from_utf8_lossy(&bytes)),
                        Some(Err(error)) => {
                            return Some((
                                Err(anyhow!("the connection was interrupted: {error}")),
                                (stream, buffer, true),
                            ));
                        }
                        None => {
                            finished = true;
                            if buffer.trim().is_empty() {
                                return Some((
                                    Ok(ChatDelta::Done {
                                        finish_reason: "incomplete".into(),
                                        token_estimate: None,
                                    }),
                                    (stream, buffer, true),
                                ));
                            }
                            buffer.push('\n');
                        }
                    }
                }
            },
        );

        Ok(Box::pin(parsed))
    }

    async fn embed(&self, model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        let response = self
            .request(self.client.post(self.url("/embeddings")))
            .json(&json!({ "model": model, "input": inputs }))
            .send()
            .await
            .map_err(|e| ProviderError::Unreachable {
                endpoint: self.endpoint.clone(),
                detail: e.to_string(),
            })?;

        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ProviderError::AuthenticationRequired {
                endpoint: self.endpoint.clone(),
            }
            .into());
        }
        if !status.is_success() {
            return Err(ProviderError::Unsupported {
                feature: "embeddings",
            }
            .into());
        }

        let parsed: EmbeddingsResponse = response.json().await?;
        if parsed.data.len() != inputs.len() {
            return Err(anyhow!(
                "the endpoint returned {} embeddings for {} inputs",
                parsed.data.len(),
                inputs.len()
            ));
        }
        Ok(parsed.data.into_iter().map(|e| e.embedding).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_v1_segment_is_added_once_and_only_once() {
        for given in [
            "http://127.0.0.1:1234",
            "http://127.0.0.1:1234/",
            "http://127.0.0.1:1234/v1",
            "http://127.0.0.1:1234/v1/",
        ] {
            let provider = OpenAiCompatibleProvider::new(given, None, ProviderKind::LmStudio);
            assert_eq!(
                provider.endpoint(),
                "http://127.0.0.1:1234/v1",
                "given {given}"
            );
        }
    }
}

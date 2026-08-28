//! Ollama's native HTTP API.
//!
//! Endpoints used: `/api/version` (health), `/api/tags` (models),
//! `/api/show` (capabilities and context length), `/api/chat` (NDJSON stream)
//! and `/api/embeddings`.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;

use otwono_types::provider::{
    CapabilitySource, ConnectionHealth, ConnectionTest, ModelInfo, ProviderKind,
};

use crate::{capability, ChatDelta, ChatRequest, ChatStream, Provider, ProviderError};

pub struct OllamaProvider {
    endpoint: String,
    client: reqwest::Client,
}

#[derive(Debug, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagModel>,
}

#[derive(Debug, Deserialize)]
struct TagModel {
    name: String,
    #[serde(default)]
    details: Option<TagDetails>,
}

#[derive(Debug, Deserialize)]
struct TagDetails {
    #[serde(default)]
    parameter_size: Option<String>,
    #[serde(default)]
    quantization_level: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ShowResponse {
    /// Present on Ollama 0.5 and later.
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    model_info: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    message: Option<ChatChunkMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    eval_count: Option<u32>,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatChunkMessage {
    #[serde(default)]
    content: String,
}

#[derive(Debug, Deserialize)]
struct EmbeddingResponse {
    #[serde(default)]
    embedding: Vec<f32>,
}

impl OllamaProvider {
    pub fn new(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            client: crate::http_client().expect("HTTP client construction cannot fail"),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.endpoint)
    }

    async fn show(&self, model: &str) -> Option<ShowResponse> {
        let response = self
            .client
            .post(self.url("/api/show"))
            .json(&json!({ "model": model }))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        response.json::<ShowResponse>().await.ok()
    }

    /// `model_info` keys are architecture-prefixed, e.g.
    /// `llama.context_length`. Find whichever one is present.
    fn context_length_from(info: &serde_json::Map<String, serde_json::Value>) -> Option<u32> {
        info.iter()
            .find(|(key, _)| key.ends_with(".context_length"))
            .and_then(|(_, value)| value.as_u64())
            .map(|v| v as u32)
    }

    async fn describe_model(&self, name: &str, details: Option<TagDetails>) -> ModelInfo {
        let (capabilities, source) = match self.show(name).await {
            Some(show) => {
                let context_length = Self::context_length_from(&show.model_info);
                match capability::from_reported(&show.capabilities, context_length) {
                    Some(reported) => (reported, CapabilitySource::Reported),
                    None => {
                        let mut inferred = capability::infer_from_name(name, true);
                        inferred.context_length = context_length;
                        // The context length itself did come from the runtime.
                        (inferred, CapabilitySource::Inferred)
                    }
                }
            }
            None => (
                capability::infer_from_name(name, true),
                CapabilitySource::Inferred,
            ),
        };

        ModelInfo {
            id: name.to_string(),
            display_name: name.to_string(),
            capabilities,
            capability_source: source,
            parameter_size: details.as_ref().and_then(|d| d.parameter_size.clone()),
            quantization: details.as_ref().and_then(|d| d.quantization_level.clone()),
        }
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .client
            .get(self.url("/api/tags"))
            .send()
            .await
            .map_err(|e| ProviderError::Unreachable {
                endpoint: self.endpoint.clone(),
                detail: e.to_string(),
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Upstream {
                endpoint: self.endpoint.clone(),
                status,
                body: body.chars().take(400).collect(),
            }
            .into());
        }

        let tags: TagsResponse = response.json().await?;
        let mut models = Vec::with_capacity(tags.models.len());
        for model in tags.models {
            models.push(self.describe_model(&model.name, model.details).await);
        }
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }

    async fn test(&self) -> ConnectionTest {
        let started = std::time::Instant::now();
        let version = self.client.get(self.url("/api/version")).send().await;

        match version {
            Err(error) => ConnectionTest {
                health: ConnectionHealth::Unreachable,
                detail: format!(
                    "No Ollama service answered at {}. Start Ollama and try again. ({error})",
                    self.endpoint
                ),
                models: Vec::new(),
                latency_ms: None,
            },
            Ok(response) if !response.status().is_success() => ConnectionTest {
                health: ConnectionHealth::Unreachable,
                detail: format!(
                    "{} answered with {} — that does not look like Ollama.",
                    self.endpoint,
                    response.status()
                ),
                models: Vec::new(),
                latency_ms: None,
            },
            Ok(_) => match self.list_models().await {
                Ok(models) => {
                    let latency = started.elapsed().as_millis() as u64;
                    let detail = if models.is_empty() {
                        "Ollama is running but has no models installed. Pull one, for example \
                         `ollama pull llama3.1`, then test again."
                            .to_string()
                    } else {
                        format!(
                            "Ollama is running with {} model{} available.",
                            models.len(),
                            if models.len() == 1 { "" } else { "s" }
                        )
                    };
                    ConnectionTest {
                        health: ConnectionHealth::Reachable,
                        detail,
                        models,
                        latency_ms: Some(latency),
                    }
                }
                Err(error) => ConnectionTest {
                    health: ConnectionHealth::Reachable,
                    detail: format!("Ollama answered, but listing models failed: {error}"),
                    models: Vec::new(),
                    latency_ms: Some(started.elapsed().as_millis() as u64),
                },
            },
        }
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        let mut options = serde_json::Map::new();
        if let Some(temperature) = request.temperature {
            options.insert("temperature".into(), json!(temperature));
        }
        if let Some(top_p) = request.top_p {
            options.insert("top_p".into(), json!(top_p));
        }
        if let Some(max_tokens) = request.max_output_tokens {
            options.insert("num_predict".into(), json!(max_tokens));
        }
        if !request.stop.is_empty() {
            options.insert("stop".into(), json!(request.stop));
        }

        let body = json!({
            "model": request.model,
            "messages": request.messages,
            "stream": true,
            "options": options,
        });

        let response = self
            .client
            .post(self.url("/api/chat"))
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Unreachable {
                endpoint: self.endpoint.clone(),
                detail: e.to_string(),
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            if status.as_u16() == 404 {
                return Err(ProviderError::ModelNotFound {
                    model: request.model,
                }
                .into());
            }
            return Err(ProviderError::Upstream {
                endpoint: self.endpoint.clone(),
                status: status.as_u16(),
                body: body.chars().take(400).collect(),
            }
            .into());
        }

        // Ollama streams newline-delimited JSON. Bytes may split a line, so
        // carry a buffer across chunks.
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
                        if line.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<ChatChunk>(&line) {
                            Ok(chunk) => {
                                if let Some(error) = chunk.error {
                                    return Some((
                                        Err(anyhow!("Ollama reported: {error}")),
                                        (stream, buffer, true),
                                    ));
                                }
                                if chunk.done {
                                    let tokens = chunk
                                        .eval_count
                                        .map(|c| c + chunk.prompt_eval_count.unwrap_or(0));
                                    return Some((
                                        Ok(ChatDelta::Done {
                                            finish_reason: chunk
                                                .done_reason
                                                .unwrap_or_else(|| "stop".into()),
                                            token_estimate: tokens,
                                        }),
                                        (stream, buffer, true),
                                    ));
                                }
                                if let Some(message) = chunk.message {
                                    if !message.content.is_empty() {
                                        return Some((
                                            Ok(ChatDelta::Text(message.content)),
                                            (stream, buffer, finished),
                                        ));
                                    }
                                }
                                continue;
                            }
                            Err(error) => {
                                return Some((
                                    Err(anyhow!(
                                        "could not read the response from Ollama: {error}"
                                    )),
                                    (stream, buffer, true),
                                ));
                            }
                        }
                    }

                    match stream.next().await {
                        Some(Ok(bytes)) => {
                            buffer.push_str(&String::from_utf8_lossy(&bytes));
                        }
                        Some(Err(error)) => {
                            return Some((
                                Err(anyhow!("the connection to Ollama was interrupted: {error}")),
                                (stream, buffer, true),
                            ));
                        }
                        None => {
                            // The stream ended without a `done` frame.
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
        let mut out = Vec::with_capacity(inputs.len());
        for input in inputs {
            let response = self
                .client
                .post(self.url("/api/embeddings"))
                .json(&json!({ "model": model, "prompt": input }))
                .send()
                .await
                .map_err(|e| ProviderError::Unreachable {
                    endpoint: self.endpoint.clone(),
                    detail: e.to_string(),
                })?;
            if !response.status().is_success() {
                return Err(ProviderError::Unsupported {
                    feature: "embeddings",
                }
                .into());
            }
            let parsed: EmbeddingResponse = response.json().await?;
            if parsed.embedding.is_empty() {
                return Err(ProviderError::Unsupported {
                    feature: "embeddings",
                }
                .into());
            }
            out.push(parsed.embedding);
        }
        Ok(out)
    }
}

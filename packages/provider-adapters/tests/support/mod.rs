//! A fake local AI runtime.
//!
//! It speaks the real Ollama and OpenAI-compatible wire protocols so that the
//! shipped adapters can be tested without a mock provider existing anywhere in
//! the application itself (see `DECISIONS.md` D-006).

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Clone, Default)]
pub struct Behaviour {
    /// Reply 401 to everything unless a bearer token is presented.
    pub require_auth: bool,
    /// Names the runtime serves.
    pub models: Vec<String>,
    /// Text the chat endpoint streams, one delta per element.
    pub chat_deltas: Vec<String>,
    /// Return this status from the chat endpoint instead of streaming.
    pub chat_status: Option<u16>,
    /// Refuse embedding requests.
    pub embeddings_fail: bool,
    /// Capabilities reported by Ollama's `/api/show`.
    pub reported_capabilities: Vec<String>,
    /// Context length reported by Ollama's `/api/show`.
    pub context_length: Option<u32>,
    /// Emit a malformed line in the middle of the stream.
    pub corrupt_stream: bool,
}

impl Behaviour {
    pub fn ollama_default() -> Self {
        Self {
            models: vec!["llama3.1:8b".into(), "nomic-embed-text:latest".into()],
            chat_deltas: vec!["Hello".into(), ", ".into(), "world".into()],
            reported_capabilities: vec!["completion".into(), "tools".into()],
            context_length: Some(131_072),
            ..Default::default()
        }
    }

    pub fn openai_default() -> Self {
        Self {
            models: vec!["local-model".into(), "nomic-embed-text".into()],
            chat_deltas: vec!["Hello".into(), ", ".into(), "world".into()],
            ..Default::default()
        }
    }
}

#[derive(Clone)]
pub struct FakeRuntime {
    pub base_url: String,
    behaviour: Arc<Mutex<Behaviour>>,
    /// Paths that were requested, for asserting on protocol use.
    pub requests: Arc<Mutex<Vec<String>>>,
}

impl FakeRuntime {
    pub async fn start(behaviour: Behaviour) -> Self {
        let state = AppState {
            behaviour: Arc::new(Mutex::new(behaviour)),
            requests: Arc::new(Mutex::new(Vec::new())),
        };

        let app = Router::new()
            // Ollama
            .route("/api/version", get(ollama_version))
            .route("/api/tags", get(ollama_tags))
            .route("/api/show", post(ollama_show))
            .route("/api/chat", post(ollama_chat))
            .route("/api/embeddings", post(ollama_embeddings))
            // OpenAI-compatible
            .route("/v1/models", get(openai_models))
            .route("/v1/chat/completions", post(openai_chat))
            .route("/v1/embeddings", post(openai_embeddings))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        Self {
            base_url: format!("http://{addr}"),
            behaviour: state.behaviour,
            requests: state.requests,
        }
    }

    pub fn requested(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    pub fn set(&self, f: impl FnOnce(&mut Behaviour)) {
        let mut behaviour = self.behaviour.lock().unwrap();
        f(&mut behaviour);
    }
}

#[derive(Clone)]
struct AppState {
    behaviour: Arc<Mutex<Behaviour>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl AppState {
    fn behaviour(&self) -> Behaviour {
        self.behaviour.lock().unwrap().clone()
    }
    fn note(&self, path: &str) {
        self.requests.lock().unwrap().push(path.to_string());
    }
}

fn unauthorised() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": { "message": "missing api key" } })),
    )
        .into_response()
}

fn authorised(state: &AppState, headers: &HeaderMap) -> bool {
    if !state.behaviour().require_auth {
        return true;
    }
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("Bearer ") && v.len() > 8)
}

// ---------------------------------------------------------------- Ollama

async fn ollama_version(State(state): State<AppState>) -> Json<Value> {
    state.note("/api/version");
    Json(json!({ "version": "0.5.7" }))
}

async fn ollama_tags(State(state): State<AppState>) -> Json<Value> {
    state.note("/api/tags");
    let models: Vec<Value> = state
        .behaviour()
        .models
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "size": 4_000_000_000u64,
                "details": { "parameter_size": "8B", "quantization_level": "Q4_K_M" }
            })
        })
        .collect();
    Json(json!({ "models": models }))
}

async fn ollama_show(State(state): State<AppState>, Json(_body): Json<Value>) -> Json<Value> {
    state.note("/api/show");
    let behaviour = state.behaviour();
    let mut model_info = serde_json::Map::new();
    if let Some(context) = behaviour.context_length {
        model_info.insert("llama.context_length".into(), json!(context));
    }
    Json(json!({
        "capabilities": behaviour.reported_capabilities,
        "model_info": model_info,
    }))
}

async fn ollama_chat(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    state.note("/api/chat");
    let behaviour = state.behaviour();
    if let Some(status) = behaviour.chat_status {
        return (
            StatusCode::from_u16(status).unwrap(),
            Json(json!({ "error": "requested failure" })),
        )
            .into_response();
    }
    let model = body["model"].as_str().unwrap_or_default();
    if !behaviour.models.iter().any(|m| m == model) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("model '{model}' not found") })),
        )
            .into_response();
    }

    let mut lines = String::new();
    for (index, delta) in behaviour.chat_deltas.iter().enumerate() {
        if behaviour.corrupt_stream && index == 1 {
            lines.push_str("{ this is not json\n");
            continue;
        }
        lines.push_str(&format!(
            "{}\n",
            json!({ "model": model, "message": { "role": "assistant", "content": delta }, "done": false })
        ));
    }
    lines.push_str(&format!(
        "{}\n",
        json!({
            "model": model, "done": true, "done_reason": "stop",
            "prompt_eval_count": 9, "eval_count": 12
        })
    ));

    ([("content-type", "application/x-ndjson")], lines).into_response()
}

async fn ollama_embeddings(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    state.note("/api/embeddings");
    let behaviour = state.behaviour();
    if behaviour.embeddings_fail {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "not an embedding model" })),
        )
            .into_response();
    }
    let prompt = body["prompt"].as_str().unwrap_or_default();
    Json(json!({ "embedding": deterministic_vector(prompt) })).into_response()
}

// ------------------------------------------------------- OpenAI-compatible

async fn openai_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    state.note("/v1/models");
    if !authorised(&state, &headers) {
        return unauthorised();
    }
    let data: Vec<Value> = state
        .behaviour()
        .models
        .iter()
        .map(|id| json!({ "id": id, "object": "model", "owned_by": "local" }))
        .collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}

async fn openai_chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.note("/v1/chat/completions");
    if !authorised(&state, &headers) {
        return unauthorised();
    }
    let behaviour = state.behaviour();
    if let Some(status) = behaviour.chat_status {
        return (
            StatusCode::from_u16(status).unwrap(),
            Json(json!({ "error": { "message": "requested failure" } })),
        )
            .into_response();
    }
    let model = body["model"].as_str().unwrap_or_default();
    if !behaviour.models.iter().any(|m| m == model) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": { "message": "model not found" } })),
        )
            .into_response();
    }

    let mut out = String::new();
    // A comment line: a real SSE keep-alive that the parser must ignore.
    out.push_str(": keep-alive\n\n");
    for delta in &behaviour.chat_deltas {
        out.push_str(&format!(
            "data: {}\n\n",
            json!({ "choices": [ { "index": 0, "delta": { "content": delta } } ] })
        ));
    }
    out.push_str(&format!(
        "data: {}\n\n",
        json!({ "choices": [ { "index": 0, "delta": {}, "finish_reason": "stop" } ],
                "usage": { "total_tokens": 21 } })
    ));
    out.push_str("data: [DONE]\n\n");

    ([("content-type", "text/event-stream")], out).into_response()
}

async fn openai_embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.note("/v1/embeddings");
    if !authorised(&state, &headers) {
        return unauthorised();
    }
    if state.behaviour().embeddings_fail {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "message": "this model does not embed" } })),
        )
            .into_response();
    }
    let inputs: Vec<String> = match &body["input"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items
            .iter()
            .map(|i| i.as_str().unwrap_or_default().to_string())
            .collect(),
        _ => Vec::new(),
    };
    let data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            json!({ "object": "embedding", "index": index, "embedding": deterministic_vector(input) })
        })
        .collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}

/// A stable vector so tests can assert on values without a real model.
fn deterministic_vector(input: &str) -> Vec<f32> {
    let mut vector = vec![0.0f32; 8];
    for (index, byte) in input.bytes().enumerate() {
        vector[index % 8] += (byte as f32) / 255.0;
    }
    vector
}

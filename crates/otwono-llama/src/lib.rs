//! llama.cpp as an OTWONO AI backend.
//!
//! # Shape
//!
//! ```text
//!   otwono-aid  ──NDJSON JSON-RPC on stdio──▶  otwono-llama-backend  ──HTTP on a
//!   (daemon)        (otwono_ai::supervisor)        (this crate)         Unix socket──▶  llama-server
//! ```
//!
//! Three processes, and each boundary is there for a reason:
//!
//! * **The daemon does not link an inference engine.** `otwono-aid` stays a small Rust
//!   binary that keeps answering `ai.capabilities` when a model load segfaults. Linking
//!   llama.cpp into it would also mean `cargo test --workspace` needed a C++ toolchain and
//!   an engine build on every machine, which would put the whole workspace behind the
//!   slowest dependency in it.
//! * **The adapter does not reimplement llama.cpp's server.** `llama-server` already does
//!   model loading, KV-cache reuse across requests, slot management and sampling. We
//!   translate; we do not re-solve (CLAUDE.md §2.3).
//! * **The engine is reachable only through a Unix socket.** Not a loopback TCP port —
//!   see `http` for why that distinction is a security boundary and not a preference.
//!
//! # Why an adapter process at all, rather than the daemon speaking HTTP directly
//!
//! It is a fair question, and the answer is that llama.cpp is one backend of several.
//! whisper.cpp has no server; Piper reads text on stdin; ONNX Runtime is a library. Each
//! needs a different translation, and the point of the supervisor protocol is that
//! `otwono-aid` learns exactly one of them. Putting llama.cpp's HTTP dialect in the daemon
//! would mean the daemon grows a second dialect for the next backend, and a third after
//! that.
//!
//! # STATUS
//!
//! `IMPLEMENTED`. Exercised end to end against a real `llama-server` and a real GGUF model
//! by `tests/end_to_end.rs`, which is skipped unless the environment names an engine
//! binary — see that file. Not yet built into any OS image, so a booted node still reports
//! no local inference.

#![forbid(unsafe_code)]

pub mod engine;
pub mod http;
pub mod protocol;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use otwono_proto::message::{Request, Response, RpcError};
use serde_json::{json, Value};

pub use engine::{Engine, EngineConfig, EngineError, LoadRequest};
pub use protocol::{
    InferParams, InferResult, LoadParams, LoadResult, StatusResult, StopReason, Timings, METHOD_INFER,
    METHOD_LOAD, METHOD_STATUS, METHOD_UNLOAD, SCHEMA_VERSION,
};

pub const ENGINE_NAME: &str = "llama.cpp";

/// How long a single completion may take before the adapter gives up on the engine.
///
/// Generous: a long generation on a cold Pi is legitimately slow, and a timeout that fires
/// on slow-but-working hardware is worse than no timeout at all, because it is
/// indistinguishable from a hang. The engine is killed when it does fire, so the bound
/// still has to exist.
pub const DEFAULT_INFER_TIMEOUT: Duration = Duration::from_secs(600);

/// The adapter: one engine configuration, at most one loaded model.
pub struct Adapter {
    config: EngineConfig,
    engine: Option<Engine>,
    engine_version: String,
    infer_timeout: Duration,
}

impl Adapter {
    pub fn new(config: EngineConfig) -> Adapter {
        let engine_version = detect_engine_version(&config.binary);
        Adapter {
            config,
            engine: None,
            engine_version,
            infer_timeout: DEFAULT_INFER_TIMEOUT,
        }
    }

    pub fn with_infer_timeout(mut self, timeout: Duration) -> Adapter {
        self.infer_timeout = timeout;
        self
    }

    pub fn engine_version(&self) -> &str {
        &self.engine_version
    }

    /// Handle one request. Never panics and never fails the process: a bad request from
    /// the daemon is an error response, because an adapter that exits on malformed input
    /// turns a caller's bug into a model reload.
    pub fn handle(&mut self, request: Request) -> Response {
        let id = request.id.clone();
        let result = match request.method.as_str() {
            METHOD_LOAD => self.load(request.params),
            METHOD_INFER => self.infer(request.params),
            METHOD_STATUS => Ok(self.status()),
            METHOD_UNLOAD => Ok(self.unload()),
            other => Err(RpcError::method_not_found(format!(
                "{other} is not a backend method"
            ))),
        };
        match result {
            Ok(value) => Response::ok(id, value),
            Err(e) => Response::err(id, e),
        }
    }

    fn load(&mut self, params: Value) -> Result<Value, RpcError> {
        let p: LoadParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("{METHOD_LOAD}: {e}")))?;
        if p.context_tokens == 0 {
            return Err(RpcError::invalid_params("context_tokens must be at least 1"));
        }
        if p.sequences == 0 {
            return Err(RpcError::invalid_params("sequences must be at least 1"));
        }

        // Drop the old engine before starting the new one. Both resident at once would
        // need twice the memory admission control budgeted for, on the machines least able
        // to spare it.
        self.engine = None;

        let started = Instant::now();
        let engine = Engine::start(
            &self.config,
            &LoadRequest {
                model_path: PathBuf::from(&p.model_path),
                context_tokens: p.context_tokens,
                sequences: p.sequences,
                threads: p.threads,
                gpu_layers: p.gpu_layers,
            },
        )
        .map_err(engine_error)?;

        let result = LoadResult {
            schema_version: SCHEMA_VERSION.to_string(),
            model_path: p.model_path,
            context_tokens: engine.context_tokens(),
            sequences: engine.sequences(),
            load_ms: started.elapsed().as_millis() as u64,
            engine_pid: engine.pid(),
        };
        self.engine = Some(engine);
        Ok(serde_json::to_value(result).expect("LoadResult serializes"))
    }

    fn infer(&mut self, params: Value) -> Result<Value, RpcError> {
        let p: InferParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("{METHOD_INFER}: {e}")))?;
        if p.max_tokens == 0 {
            return Err(RpcError::invalid_params("max_tokens must be at least 1"));
        }
        let timeout = self.infer_timeout;
        let engine = self
            .engine
            .as_mut()
            .ok_or_else(|| RpcError::unavailable("no model is loaded; call backend.load first"))?;

        let mut body = json!({
            "prompt": p.prompt,
            "n_predict": p.max_tokens,
            "stream": false,
            // Reuse the KV cache across requests with a shared prefix. This is most of the
            // difference between a usable and an unusable assistant on small hardware,
            // where re-evaluating a long system prompt every turn dominates the wall clock.
            "cache_prompt": true,
        });
        let map = body.as_object_mut().expect("object literal");
        if let Some(v) = p.temperature {
            map.insert("temperature".into(), json!(v));
        }
        if let Some(v) = p.top_p {
            map.insert("top_p".into(), json!(v));
        }
        if let Some(v) = p.top_k {
            map.insert("top_k".into(), json!(v));
        }
        if let Some(v) = p.seed {
            map.insert("seed".into(), json!(v));
        }
        if !p.stop.is_empty() {
            map.insert("stop".into(), json!(p.stop));
        }

        let response = engine.post("/completion", &body, timeout).map_err(engine_error)?;
        Ok(serde_json::to_value(completion_to_result(&response)).expect("InferResult serializes"))
    }

    fn status(&self) -> Value {
        let result = StatusResult {
            schema_version: SCHEMA_VERSION.to_string(),
            engine: ENGINE_NAME.to_string(),
            engine_version: self.engine_version.clone(),
            model_path: self.engine.as_ref().map(|e| e.model_path().display().to_string()),
            context_tokens: self.engine.as_ref().map(|e| e.context_tokens()),
            sequences: self.engine.as_ref().map(|e| e.sequences()),
        };
        serde_json::to_value(result).expect("StatusResult serializes")
    }

    fn unload(&mut self) -> Value {
        let was_loaded = self.engine.take().is_some();
        json!({ "schema_version": SCHEMA_VERSION, "unloaded": was_loaded })
    }
}

/// Translate `llama-server`'s `/completion` response into ours.
///
/// Every field is read defensively. The engine is a separately-versioned upstream project,
/// and a missing key must degrade one field rather than fail the whole request — a
/// completion that arrived is not worth discarding because the timings block changed shape.
///
/// That is not a hypothetical. This function was first written against the
/// `stopped_eos` / `stopped_word` / `stopped_limit` booleans, and the engine it was then
/// run against reports a single `stop_type` string instead. Both are read, newest first.
pub fn completion_to_result(response: &Value) -> InferResult {
    let stop_reason = stop_reason(response);

    InferResult {
        schema_version: SCHEMA_VERSION.to_string(),
        text: response["content"].as_str().unwrap_or_default().to_string(),
        tokens_predicted: response["tokens_predicted"].as_u64().unwrap_or(0) as u32,
        tokens_evaluated: response["tokens_evaluated"].as_u64().unwrap_or(0) as u32,
        stop_reason,
        prompt_truncated: response["truncated"].as_bool().unwrap_or(false),
        timings: Timings {
            prompt_ms: response
                .pointer("/timings/prompt_ms")
                .and_then(as_ms)
                .unwrap_or(0),
            predicted_ms: response
                .pointer("/timings/predicted_ms")
                .and_then(as_ms)
                .unwrap_or(0),
        },
    }
}

fn as_ms(v: &Value) -> Option<u64> {
    v.as_f64().map(|f| f.max(0.0) as u64)
}

/// Why generation stopped, across two generations of the engine's response shape.
///
/// Current builds send `"stop_type": "eos" | "word" | "limit" | "none"`. Older ones send
/// three booleans. Reading both costs a dozen lines and means an engine upgrade cannot
/// silently turn every completion into `Other` — which would look like working software,
/// because the text still arrives.
fn stop_reason(response: &Value) -> StopReason {
    match response["stop_type"].as_str() {
        Some("eos") => return StopReason::EndOfSequence,
        Some("word") => return StopReason::StopString,
        Some("limit") => return StopReason::TokenLimit,
        Some(_) => return StopReason::Other,
        None => {}
    }
    if response["stopped_eos"].as_bool() == Some(true) {
        StopReason::EndOfSequence
    } else if response["stopped_word"].as_bool() == Some(true) {
        StopReason::StopString
    } else if response["stopped_limit"].as_bool() == Some(true) {
        StopReason::TokenLimit
    } else {
        StopReason::Other
    }
}

/// Map an engine failure onto the control plane's error taxonomy.
///
/// The distinction that matters: a request the engine *refused* is the caller's problem and
/// an engine that *died* is the node's. Collapsing both into "internal error" would send
/// an operator hunting a crash that never happened.
fn engine_error(e: EngineError) -> RpcError {
    match e {
        EngineError::Refused { .. } | EngineError::Protocol(_) => RpcError::invalid_params(e.to_string()),
        EngineError::MissingBinary { .. }
        | EngineError::MissingModel { .. }
        | EngineError::SocketPathTooLong { .. }
        | EngineError::Spawn { .. }
        | EngineError::Died { .. }
        | EngineError::StartupTimeout { .. }
        | EngineError::Http(_) => RpcError::unavailable(e.to_string()),
    }
}

/// Ask the engine binary what version it is.
///
/// Best effort by design. A version string we cannot read is a worse status message, not a
/// reason to refuse to run: the engine either works or it does not, and that is discovered
/// at load.
pub fn detect_engine_version(binary: &std::path::Path) -> String {
    let Ok(output) = Command::new(binary).arg("--version").output() else {
        return "unknown".to_string();
    };
    // llama-server prints its version banner on stderr; check both rather than depend on it.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    text.lines()
        .find_map(|line| line.trim().strip_prefix("version:"))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> Adapter {
        Adapter {
            config: EngineConfig {
                binary: PathBuf::from("/nonexistent/llama-server"),
                runtime_dir: std::env::temp_dir(),
                startup_timeout: Duration::from_millis(200),
                extra_args: Vec::new(),
            },
            engine: None,
            engine_version: "test".to_string(),
            infer_timeout: Duration::from_secs(1),
        }
    }

    fn call(a: &mut Adapter, method: &str, params: Value) -> Result<Value, RpcError> {
        a.handle(Request::new(1, method, params)).into_result()
    }

    #[test]
    fn status_before_any_load_reports_no_model() {
        let mut a = adapter();
        let s = call(&mut a, METHOD_STATUS, json!({})).unwrap();
        assert_eq!(s["engine"], "llama.cpp");
        assert!(s["model_path"].is_null());
        assert!(s["context_tokens"].is_null());
    }

    #[test]
    fn infer_without_a_loaded_model_says_so_instead_of_starting_one() {
        // Implicit loading would let a caller sidestep admission control entirely: the
        // load would happen with whatever context the infer call implied, un-budgeted.
        let mut a = adapter();
        let err = call(&mut a, METHOD_INFER, json!({"prompt": "hi", "max_tokens": 4})).unwrap_err();
        assert!(err.message.contains("no model is loaded"), "{err:?}");
    }

    #[test]
    fn an_unknown_method_is_method_not_found_not_a_crash() {
        let mut a = adapter();
        let err = call(&mut a, "backend.teleport", json!({})).unwrap_err();
        assert_eq!(err.code, RpcError::method_not_found("x").code);
    }

    #[test]
    fn a_zero_context_load_is_refused_before_spawning_anything() {
        let mut a = adapter();
        let err = call(
            &mut a,
            METHOD_LOAD,
            json!({"model_path": "/m.gguf", "context_tokens": 0}),
        )
        .unwrap_err();
        assert!(err.message.contains("context_tokens"), "{err:?}");
    }

    #[test]
    fn zero_max_tokens_is_refused_as_a_parameter_error() {
        let mut a = adapter();
        let err = call(&mut a, METHOD_INFER, json!({"prompt": "hi", "max_tokens": 0})).unwrap_err();
        assert!(err.message.contains("max_tokens"), "{err:?}");
    }

    #[test]
    fn unloading_when_nothing_is_loaded_succeeds_and_says_nothing_happened() {
        let mut a = adapter();
        let r = call(&mut a, METHOD_UNLOAD, json!({})).unwrap();
        assert_eq!(r["unloaded"], false);
    }

    #[test]
    fn a_load_against_a_missing_engine_is_unavailable_not_invalid_params() {
        // The taxonomy matters: this is the node's problem, not the caller's.
        let mut a = adapter();
        let err = call(
            &mut a,
            METHOD_LOAD,
            json!({"model_path": "/m.gguf", "context_tokens": 512}),
        )
        .unwrap_err();
        assert_eq!(err.code, RpcError::unavailable("x").code, "{err:?}");
    }

    #[test]
    fn a_completion_hitting_the_token_limit_is_reported_as_truncated_output() {
        // The caller has to be able to tell "it finished" from "it ran out of budget".
        let response = json!({
            "content": "half an answ",
            "tokens_predicted": 8, "tokens_evaluated": 13,
            "stop_type": "limit",
            "timings": {"prompt_ms": 12.5, "predicted_ms": 480.0}
        });
        let r = completion_to_result(&response);
        assert_eq!(r.stop_reason, StopReason::TokenLimit);
        assert_eq!(r.text, "half an answ");
        assert_eq!(r.tokens_predicted, 8);
        assert_eq!(r.timings.prompt_ms, 12);
        assert_eq!(r.timings.predicted_ms, 480);
    }

    #[test]
    fn each_stop_type_maps_to_its_own_reason() {
        // The shape current llama.cpp actually sends.
        let with = |t: &str| json!({ "content": "x", "stop_type": t });
        assert_eq!(
            completion_to_result(&with("eos")).stop_reason,
            StopReason::EndOfSequence
        );
        assert_eq!(
            completion_to_result(&with("word")).stop_reason,
            StopReason::StopString
        );
        assert_eq!(
            completion_to_result(&with("limit")).stop_reason,
            StopReason::TokenLimit
        );
        assert_eq!(completion_to_result(&with("none")).stop_reason, StopReason::Other);
    }

    #[test]
    fn the_older_boolean_stop_fields_are_still_understood() {
        // An engine predating stop_type must not have every completion read as Other.
        let base = |key: &str| {
            json!({ "content": "x", "stopped_eos": key == "eos",
                    "stopped_word": key == "word", "stopped_limit": key == "limit" })
        };
        assert_eq!(
            completion_to_result(&base("eos")).stop_reason,
            StopReason::EndOfSequence
        );
        assert_eq!(
            completion_to_result(&base("word")).stop_reason,
            StopReason::StopString
        );
        assert_eq!(
            completion_to_result(&base("limit")).stop_reason,
            StopReason::TokenLimit
        );
        assert_eq!(completion_to_result(&base("none")).stop_reason, StopReason::Other);
    }

    #[test]
    fn stop_type_wins_over_the_legacy_booleans_when_both_are_present() {
        let r = completion_to_result(&json!({
            "content": "x", "stop_type": "eos",
            "stopped_eos": false, "stopped_limit": true
        }));
        assert_eq!(r.stop_reason, StopReason::EndOfSequence);
    }

    #[test]
    fn a_response_missing_every_optional_field_still_yields_the_text() {
        // Upstream is separately versioned. Losing the timings block must not lose the
        // completion that arrived with it.
        let r = completion_to_result(&json!({ "content": "still here" }));
        assert_eq!(r.text, "still here");
        assert_eq!(r.tokens_predicted, 0);
        assert_eq!(r.stop_reason, StopReason::Other);
        assert_eq!(r.timings, Timings::default());
    }

    #[test]
    fn a_response_that_is_not_an_object_does_not_panic() {
        let r = completion_to_result(&json!("surprise"));
        assert_eq!(r.text, "");
    }

    #[test]
    fn the_version_of_a_binary_that_is_not_there_is_unknown_not_a_failure() {
        assert_eq!(
            detect_engine_version(std::path::Path::new("/nonexistent/x")),
            "unknown"
        );
    }
}

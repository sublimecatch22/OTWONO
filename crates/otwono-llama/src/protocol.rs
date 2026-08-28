//! The wire contract between `otwono-aid` and an AI backend adapter.
//!
//! Newline-delimited JSON-RPC 2.0 on the adapter's stdin/stdout, framed and supervised by
//! `otwono-ai::supervisor`. The message types are `otwono_proto`'s, not new ones: the
//! control plane already defines what a request, a response and an error look like
//! (ADR-0003), and a second shape for the same job would be one more thing to keep in
//! step for no benefit.
//!
//! # Why these types live here and not in `otwono-ai`
//!
//! `otwono-ai` must not depend on any engine. It is the crate that decides *whether* a
//! model may load, and it has to stay buildable and testable on a machine with no
//! inference engine anywhere on it. So the backend-side contract lives with the adapter,
//! and `otwono-aid` depends on both.
//!
//! # Not covered yet: streaming
//!
//! Every method here is one request, one response. Real interactive use wants tokens as
//! they are produced, which means several frames per request and a control plane that can
//! carry them to the caller. Neither exists yet. `llama-server` can stream, so the gap is
//! ours, not the engine's — see `docs/ai/AI-RUNTIME.md`.

use serde::{Deserialize, Serialize};

/// Method names. Namespaced so a future second adapter kind reads the same.
pub const METHOD_LOAD: &str = "backend.load";
pub const METHOD_INFER: &str = "backend.infer";
pub const METHOD_STATUS: &str = "backend.status";
pub const METHOD_UNLOAD: &str = "backend.unload";

pub const SCHEMA_VERSION: &str = "1.0.0";

/// Load a model. Replaces whatever was loaded before.
///
/// One model at a time, deliberately. Holding several would mean the adapter deciding how
/// to divide memory between them, and that decision belongs to admission control, which
/// sits above and has the node's whole budget in view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadParams {
    pub model_path: String,
    /// Context window to reserve. Admission control already charged the node's memory
    /// budget for this exact number, so it is not a hint the engine may round up.
    pub context_tokens: u32,
    #[serde(default = "one")]
    pub sequences: u32,
    #[serde(default)]
    pub threads: Option<u32>,
    #[serde(default)]
    pub gpu_layers: Option<u32>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadResult {
    pub schema_version: String,
    pub model_path: String,
    pub context_tokens: u32,
    pub sequences: u32,
    /// Milliseconds from spawn to the engine reporting healthy. Load time is the number
    /// users notice on small hardware, so it is reported rather than inferred.
    pub load_ms: u64,
    pub engine_pid: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InferParams {
    pub prompt: String,
    /// Upper bound on generated tokens. Required: a request with no bound is a request
    /// that can occupy the node's only engine indefinitely.
    pub max_tokens: u32,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub top_k: Option<u32>,
    /// Fixed seed for a reproducible sample. `None` lets the engine choose.
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub stop: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model emitted its end-of-sequence token.
    EndOfSequence,
    /// `max_tokens` was reached. The answer is cut off, and a caller that renders it
    /// without saying so is lying to the user by omission.
    TokenLimit,
    /// One of the caller's stop strings matched.
    StopString,
    /// The engine stopped for a reason it did not classify.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferResult {
    pub schema_version: String,
    pub text: String,
    pub tokens_predicted: u32,
    pub tokens_evaluated: u32,
    pub stop_reason: StopReason,
    /// Whether the engine had to drop part of the prompt to fit the context window.
    pub prompt_truncated: bool,
    pub timings: Timings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Timings {
    pub prompt_ms: u64,
    pub predicted_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResult {
    pub schema_version: String,
    pub engine: String,
    pub engine_version: String,
    /// `None` when no model is loaded, which is the state the adapter starts in.
    pub model_path: Option<String>,
    pub context_tokens: Option<u32>,
    pub sequences: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_params_default_to_a_single_sequence() {
        let p: LoadParams =
            serde_json::from_str(r#"{"model_path":"/m.gguf","context_tokens":4096}"#).unwrap();
        assert_eq!(p.sequences, 1);
        assert_eq!(p.threads, None);
    }

    #[test]
    fn max_tokens_is_required_and_its_absence_is_an_error() {
        // Not a defaulted field on purpose: an unbounded generation holds the node's only
        // engine for as long as the model feels like talking.
        let err = serde_json::from_str::<InferParams>(r#"{"prompt":"hi"}"#).unwrap_err();
        assert!(err.to_string().contains("max_tokens"), "{err}");
    }

    #[test]
    fn stop_reasons_round_trip_as_snake_case() {
        for (reason, text) in [
            (StopReason::EndOfSequence, "\"end_of_sequence\""),
            (StopReason::TokenLimit, "\"token_limit\""),
            (StopReason::StopString, "\"stop_string\""),
            (StopReason::Other, "\"other\""),
        ] {
            assert_eq!(serde_json::to_string(&reason).unwrap(), text);
            assert_eq!(serde_json::from_str::<StopReason>(text).unwrap(), reason);
        }
    }

    #[test]
    fn an_unloaded_adapter_reports_no_model_rather_than_an_empty_string() {
        let s = StatusResult {
            schema_version: SCHEMA_VERSION.to_string(),
            engine: "llama.cpp".into(),
            engine_version: "b10588".into(),
            model_path: None,
            context_tokens: None,
            sequences: None,
        };
        let json = serde_json::to_value(&s).unwrap();
        assert!(json["model_path"].is_null());
    }
}

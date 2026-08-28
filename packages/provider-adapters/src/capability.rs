//! Capability discovery.
//!
//! The rule is that a capability is only claimed when it was reported by the
//! runtime or proven by a probe. Everything else is marked `Inferred` and the
//! UI says so, because a wrong guess here means a feature that fails at the
//! moment the user tries to use it.

use otwono_types::provider::{Capabilities, CapabilitySource};

/// Model families that are known to serve embeddings rather than chat.
const EMBEDDING_FAMILIES: &[&str] = &[
    "nomic-embed",
    "mxbai-embed",
    "all-minilm",
    "bge-",
    "gte-",
    "e5-",
    "snowflake-arctic-embed",
    "text-embedding",
    "embeddinggemma",
];

/// Model families that accept images.
const VISION_FAMILIES: &[&str] = &[
    "llava",
    "bakllava",
    "moondream",
    "-vision",
    "vision-",
    "minicpm-v",
    "qwen2-vl",
    "qwen2.5-vl",
    "pixtral",
    "internvl",
    "gemma3",
];

/// Model families documented to support tool/function calling.
const TOOL_FAMILIES: &[&str] = &[
    "llama3.1",
    "llama3.2",
    "llama3.3",
    "llama-3.1",
    "llama-3.3",
    "qwen2.5",
    "qwen3",
    "mistral-nemo",
    "mistral-large",
    "firefunction",
    "command-r",
    "hermes3",
    "granite3",
    "gpt-4",
    "gpt-5",
    "claude-",
];

fn matches_any(model: &str, families: &[&str]) -> bool {
    let lowered = model.to_ascii_lowercase();
    families.iter().any(|family| lowered.contains(family))
}

pub fn looks_like_embedding_model(model: &str) -> bool {
    matches_any(model, EMBEDDING_FAMILIES)
}

pub fn looks_like_vision_model(model: &str) -> bool {
    matches_any(model, VISION_FAMILIES)
}

pub fn looks_like_tool_model(model: &str) -> bool {
    matches_any(model, TOOL_FAMILIES)
}

/// Capabilities inferred from a model name alone. Always paired with
/// `CapabilitySource::Inferred`.
pub fn infer_from_name(model: &str, streaming: bool) -> Capabilities {
    let embedding = looks_like_embedding_model(model);
    Capabilities {
        // An embedding model does not chat, whatever the endpoint implies.
        chat: !embedding,
        streaming: streaming && !embedding,
        tool_calling: !embedding && looks_like_tool_model(model),
        structured_output: !embedding && looks_like_tool_model(model),
        vision: !embedding && looks_like_vision_model(model),
        embeddings: embedding,
        context_length: None,
    }
}

/// Translate a runtime-reported capability list (Ollama ≥ 0.5 style) into our
/// model. Returns `None` when the runtime reported nothing useful.
pub fn from_reported(reported: &[String], context_length: Option<u32>) -> Option<Capabilities> {
    if reported.is_empty() {
        return None;
    }
    let has = |name: &str| reported.iter().any(|c| c.eq_ignore_ascii_case(name));
    Some(Capabilities {
        chat: has("completion") || has("chat"),
        streaming: has("completion") || has("chat"),
        tool_calling: has("tools"),
        structured_output: has("tools") || has("completion"),
        vision: has("vision"),
        embeddings: has("embedding") || has("embeddings"),
        context_length,
    })
}

/// Merge a probe result into a capability set, upgrading its source.
pub fn with_probed_embeddings(
    mut capabilities: Capabilities,
    embeddings_work: bool,
    source: CapabilitySource,
) -> (Capabilities, CapabilitySource) {
    capabilities.embeddings = embeddings_work;
    // A probe is stronger evidence than an inference, but weaker than the
    // runtime telling us directly about everything else.
    let source = match source {
        CapabilitySource::Inferred => CapabilitySource::Probed,
        other => other,
    };
    (capabilities, source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_embedding_model_is_not_offered_for_chat() {
        for model in [
            "nomic-embed-text",
            "mxbai-embed-large:335m",
            "text-embedding-3-small",
        ] {
            let capabilities = infer_from_name(model, true);
            assert!(capabilities.embeddings, "{model} should embed");
            assert!(
                !capabilities.chat,
                "{model} must not be offered as a chat model"
            );
            assert!(!capabilities.streaming);
        }
    }

    #[test]
    fn a_chat_model_is_not_offered_for_embeddings() {
        let capabilities = infer_from_name("llama3.1:8b", true);
        assert!(capabilities.chat);
        assert!(capabilities.streaming);
        assert!(!capabilities.embeddings);
    }

    #[test]
    fn vision_and_tool_families_are_recognised() {
        assert!(looks_like_vision_model("llava:13b"));
        assert!(looks_like_vision_model("llama3.2-vision:11b"));
        assert!(!looks_like_vision_model("llama3.1:8b"));

        assert!(looks_like_tool_model("qwen2.5:14b"));
        assert!(!looks_like_tool_model("tinyllama:1.1b"));
    }

    #[test]
    fn an_unknown_model_claims_nothing_beyond_chat() {
        let capabilities = infer_from_name("some-private-model:latest", true);
        assert!(capabilities.chat);
        assert!(capabilities.streaming);
        assert!(!capabilities.tool_calling);
        assert!(!capabilities.vision);
        assert!(!capabilities.embeddings);
        assert!(capabilities.context_length.is_none());
    }

    #[test]
    fn a_runtime_report_is_preferred_over_a_guess() {
        let reported = from_reported(
            &["completion".into(), "tools".into(), "vision".into()],
            Some(128_000),
        )
        .unwrap();
        assert!(reported.chat && reported.tool_calling && reported.vision);
        assert!(!reported.embeddings);
        assert_eq!(reported.context_length, Some(128_000));
    }

    #[test]
    fn an_empty_report_falls_through_to_inference() {
        assert!(from_reported(&[], None).is_none());
    }

    #[test]
    fn a_probe_upgrades_an_inference_but_not_a_report() {
        let (caps, source) = with_probed_embeddings(
            infer_from_name("mystery", true),
            true,
            CapabilitySource::Inferred,
        );
        assert!(caps.embeddings);
        assert_eq!(source, CapabilitySource::Probed);

        let (_, source) =
            with_probed_embeddings(Capabilities::default(), false, CapabilitySource::Reported);
        assert_eq!(source, CapabilitySource::Reported);
    }
}

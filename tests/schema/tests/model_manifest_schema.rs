//! Contract test: every model manifest OTWONO reads or writes must validate against the
//! published JSON Schema.
//!
//! The manifest crosses a trust boundary — it arrives from a catalog, a peer, or a file the
//! user dropped in — so the schema and the Rust type disagreeing is not a cosmetic problem.
//! It is the difference between refusing a malformed manifest and admitting a model on the
//! strength of a field that was silently absent.

use otwono_ai::manifest::{Footprint, ModelCapability, ModelFormat, ModelManifest, Signature};
use otwono_ai::BackendId;
use otwono_capability::Tier;
use std::path::{Path, PathBuf};

fn schema_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/model-manifest.schema.json")
}

fn load_schema() -> serde_json::Value {
    let text = std::fs::read_to_string(schema_path()).expect("schema file must exist");
    serde_json::from_str(&text).expect("schema must be valid JSON")
}

fn validator() -> jsonschema::Validator {
    jsonschema::validator_for(&load_schema()).expect("the schema must be a valid JSON Schema")
}

fn assert_valid(v: &jsonschema::Validator, instance: &serde_json::Value, what: &str) {
    let errors: Vec<String> = v
        .iter_errors(instance)
        .map(|e| format!("{}: {e}", e.instance_path))
        .collect();
    assert!(
        errors.is_empty(),
        "{what} failed schema validation:\n  {}",
        errors.join("\n  ")
    );
}

fn sample(signed: bool) -> ModelManifest {
    ModelManifest {
        schema_version: otwono_ai::manifest::SCHEMA_VERSION.to_string(),
        id: "qwen3-4b-instruct-q4_k_m".into(),
        family: "qwen3".into(),
        parameters: 4_000_000_000,
        quantization: "Q4_K_M".into(),
        format: ModelFormat::Gguf,
        blake3: "c".repeat(64),
        size_bytes: 2_500_000_000,
        min_tier: Tier::T1Edge,
        footprint: Footprint {
            weights_bytes: 2_500_000_000,
            kv_per_1k_ctx_bytes: 130_000_000,
            overhead_bytes: 134_217_728,
        },
        max_context: 32768,
        capabilities: vec![ModelCapability::Chat, ModelCapability::Tools],
        license: "apache-2.0".into(),
        backends: vec![BackendId::LlamaCppCpu, BackendId::LlamaCppVulkan],
        signature: signed.then(|| Signature {
            algorithm: "ed25519".into(),
            public_key: "AA==".into(),
            signature: "AA==".into(),
        }),
    }
}

#[test]
fn the_schema_itself_compiles() {
    validator();
}

#[test]
fn a_manifest_the_code_emits_validates() {
    let v = validator();
    for signed in [true, false] {
        let json = serde_json::to_value(sample(signed)).unwrap();
        assert_valid(
            &v,
            &json,
            if signed {
                "signed manifest"
            } else {
                "unsigned manifest"
            },
        );
    }
}

#[test]
fn every_tier_and_backend_the_code_knows_is_in_the_schema() {
    // The failure this catches: adding a backend to the Rust enum and forgetting the
    // schema, so a manifest naming it is rejected by every other language's validator.
    let v = validator();
    for tier in Tier::ALL {
        let mut m = sample(true);
        m.min_tier = tier;
        assert_valid(&v, &serde_json::to_value(m).unwrap(), tier.as_str());
    }
    for backend in [
        BackendId::LlamaCppCpu,
        BackendId::LlamaCppVulkan,
        BackendId::LlamaCppCuda,
        BackendId::LlamaCppRocm,
        BackendId::OnnxRuntime,
        BackendId::WhisperCpp,
        BackendId::Piper,
        BackendId::Vllm,
    ] {
        let mut m = sample(true);
        m.backends = vec![backend];
        assert_valid(&v, &serde_json::to_value(m).unwrap(), backend.as_str());
    }
}

#[test]
fn every_format_and_capability_the_code_knows_is_in_the_schema() {
    let v = validator();
    for format in [
        ModelFormat::Gguf,
        ModelFormat::Onnx,
        ModelFormat::Safetensors,
        ModelFormat::Rknn,
    ] {
        let mut m = sample(true);
        m.format = format;
        assert_valid(&v, &serde_json::to_value(m).unwrap(), &format!("{format:?}"));
    }
    for cap in [
        ModelCapability::Chat,
        ModelCapability::Completion,
        ModelCapability::Tools,
        ModelCapability::Embedding,
        ModelCapability::Vision,
        ModelCapability::Asr,
        ModelCapability::Tts,
    ] {
        let mut m = sample(true);
        m.capabilities = vec![cap];
        assert_valid(&v, &serde_json::to_value(m).unwrap(), &format!("{cap:?}"));
    }
}

#[test]
fn the_schema_rejects_what_the_rust_validator_rejects() {
    // Both gates must agree. A manifest one accepts and the other refuses means a node's
    // behaviour depends on which check happened to run.
    let v = validator();

    let mut uppercase_hash = sample(true);
    uppercase_hash.blake3 = "C".repeat(64);
    assert!(
        !v.is_valid(&serde_json::to_value(&uppercase_hash).unwrap()),
        "the schema must require lowercase hex, as the Rust validator does"
    );
    assert!(uppercase_hash.validate().is_err());

    let mut bad_id = sample(true);
    bad_id.id = "../escape".into();
    assert!(!v.is_valid(&serde_json::to_value(&bad_id).unwrap()));
    assert!(bad_id.validate().is_err());

    let mut no_backends = sample(true);
    no_backends.backends.clear();
    assert!(!v.is_valid(&serde_json::to_value(&no_backends).unwrap()));
    assert!(no_backends.validate().is_err());
}

#[test]
fn an_unknown_field_is_rejected() {
    // additionalProperties: false throughout, so a field added to the struct without a
    // schema change fails here rather than silently crossing a language boundary.
    let v = validator();
    let mut json = serde_json::to_value(sample(true)).unwrap();
    json.as_object_mut()
        .unwrap()
        .insert("surprise".into(), serde_json::json!(1));
    assert!(!v.is_valid(&json));
}

#[test]
fn a_manifest_from_the_schemas_own_example_shape_round_trips_through_rust() {
    // The direction that catches a schema loosened beyond what the code can parse.
    let json = serde_json::json!({
        "schema_version": "1.0.0",
        "id": "qwen3-4b-instruct-q4_k_m",
        "family": "qwen3",
        "parameters": 4000000000u64,
        "quantization": "Q4_K_M",
        "format": "gguf",
        "blake3": "d".repeat(64),
        "size_bytes": 2500000000u64,
        "min_tier": "T1_EDGE",
        "footprint": { "weights_bytes": 2500000000u64, "kv_per_1k_ctx_bytes": 130000000u64 },
        "max_context": 32768,
        "capabilities": ["chat", "tools"],
        "license": "apache-2.0",
        "backends": ["llama-cpp-cpu", "llama-cpp-vulkan", "llama-cpp-cuda"]
    });
    assert_valid(&validator(), &json, "the documented example shape");
    let parsed: ModelManifest = serde_json::from_value(json).expect("Rust must parse it");
    assert_eq!(parsed.footprint.overhead_bytes, 0, "absent means zero");
    assert_eq!(parsed.validate(), Ok(()));
}

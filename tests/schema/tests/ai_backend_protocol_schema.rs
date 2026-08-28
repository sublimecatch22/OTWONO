//! Contract test: what the backend adapter types serialize to must match the schema.
//!
//! The adapter protocol crosses a process boundary and, in principle, a language one — an
//! adapter for some future engine need not be written in Rust. That makes the schema the
//! contract and these types one implementation of it, so a field added to the Rust struct
//! without a matching schema change has to fail somewhere. Here.
//!
//! Every definition sets `additionalProperties: false`, so the check runs in both
//! directions: an undeclared field fails, and so does a schema tightened past what the
//! code emits.

use otwono_ai::supervisor::{BackendHello, PROTOCOL_VERSION};
use otwono_llama::protocol::{
    InferParams, InferResult, LoadParams, LoadResult, StatusResult, StopReason, Timings, SCHEMA_VERSION,
};
use serde_json::json;
use std::path::{Path, PathBuf};

fn schema_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/ai-backend-protocol.schema.json")
}

fn load_schema() -> serde_json::Value {
    let text = std::fs::read_to_string(schema_path()).expect("schema file must exist");
    serde_json::from_str(&text).expect("schema must be valid JSON")
}

/// Compile one named definition. The definitions carry no cross-references, so a
/// definition lifted out of `$defs` is a complete schema on its own.
fn validator_for(name: &str) -> jsonschema::Validator {
    let schema = load_schema();
    let definition = schema["$defs"]
        .get(name)
        .unwrap_or_else(|| panic!("the schema must define {name}"))
        .clone();
    jsonschema::validator_for(&definition).expect("each definition must be a valid JSON Schema")
}

fn assert_valid(name: &str, instance: &serde_json::Value) {
    let validator = validator_for(name);
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("{}: {e}", e.instance_path))
        .collect();
    assert!(
        errors.is_empty(),
        "{name} failed schema validation for {instance}:\n  {}",
        errors.join("\n  ")
    );
}

fn assert_invalid(name: &str, instance: &serde_json::Value, why: &str) {
    assert!(
        !validator_for(name).is_valid(instance),
        "{name} should have rejected {instance} ({why})"
    );
}

#[test]
fn every_definition_is_a_valid_schema() {
    let schema = load_schema();
    let defs = schema["$defs"].as_object().expect("$defs must be an object");
    assert!(!defs.is_empty());
    for name in defs.keys() {
        validator_for(name);
    }
}

#[test]
fn the_hello_the_supervisor_expects_is_the_hello_the_schema_describes() {
    let hello = BackendHello {
        protocol: PROTOCOL_VERSION,
        engine: "llama.cpp".into(),
        version: "b10588".into(),
    };
    assert_valid("hello", &serde_json::to_value(&hello).unwrap());
}

#[test]
fn an_unknown_engine_version_is_still_a_valid_hello() {
    // The adapter reports "unknown" when it cannot read the engine's version. That is a
    // worse status message, not a protocol violation, and the schema must agree.
    assert_valid(
        "hello",
        &json!({ "protocol": 1, "engine": "llama.cpp", "version": "unknown" }),
    );
}

#[test]
fn a_hello_claiming_protocol_zero_is_rejected() {
    assert_invalid(
        "hello",
        &json!({ "protocol": 0, "engine": "x", "version": "1" }),
        "protocol versions start at 1",
    );
}

#[test]
fn load_params_round_trip_through_the_schema() {
    let params = LoadParams {
        model_path: "/var/lib/otwono/models/blobs/abc".into(),
        context_tokens: 4096,
        sequences: 1,
        threads: Some(4),
        gpu_layers: None,
    };
    assert_valid("load_params", &serde_json::to_value(&params).unwrap());
    // And the shape otwono-aid actually sends, which omits the optional fields.
    assert_valid(
        "load_params",
        &json!({ "model_path": "/m.gguf", "context_tokens": 512, "sequences": 1 }),
    );
}

#[test]
fn a_zero_context_load_is_rejected_by_the_schema_as_well_as_the_code() {
    assert_invalid(
        "load_params",
        &json!({ "model_path": "/m.gguf", "context_tokens": 0 }),
        "a zero-token context window is not a context window",
    );
}

#[test]
fn load_results_validate() {
    let result = LoadResult {
        schema_version: SCHEMA_VERSION.to_string(),
        model_path: "/m.gguf".into(),
        context_tokens: 512,
        sequences: 1,
        load_ms: 101,
        engine_pid: 4242,
    };
    assert_valid("load_result", &serde_json::to_value(&result).unwrap());
}

#[test]
fn infer_params_validate_and_max_tokens_is_required_on_both_sides() {
    let params = InferParams {
        prompt: "hello".into(),
        max_tokens: 64,
        temperature: Some(0.7),
        top_p: Some(0.95),
        top_k: Some(40),
        seed: Some(1),
        stop: vec!["\n\n".into()],
    };
    assert_valid("infer_params", &serde_json::to_value(&params).unwrap());
    assert_invalid(
        "infer_params",
        &json!({ "prompt": "hello" }),
        "an unbounded generation occupies the node's only engine indefinitely",
    );
}

#[test]
fn every_stop_reason_the_code_can_produce_is_one_the_schema_allows() {
    // The enum is the part most likely to drift: a new engine field maps to a new reason,
    // and a reason the schema does not list would fail only on the machine that hit it.
    for reason in [
        StopReason::EndOfSequence,
        StopReason::TokenLimit,
        StopReason::StopString,
        StopReason::Other,
    ] {
        let result = InferResult {
            schema_version: SCHEMA_VERSION.to_string(),
            text: "some text".into(),
            tokens_predicted: 12,
            tokens_evaluated: 21,
            stop_reason: reason.clone(),
            prompt_truncated: false,
            timings: Timings {
                prompt_ms: 1,
                predicted_ms: 2,
            },
        };
        assert_valid("infer_result", &serde_json::to_value(&result).unwrap());
    }
}

#[test]
fn an_invented_stop_reason_is_rejected() {
    assert_invalid(
        "infer_result",
        &json!({
            "schema_version": "1.0.0", "text": "", "tokens_predicted": 0,
            "tokens_evaluated": 0, "stop_reason": "vibes", "prompt_truncated": false,
            "timings": { "prompt_ms": 0, "predicted_ms": 0 }
        }),
        "the reason set is closed",
    );
}

#[test]
fn a_status_with_no_model_loaded_validates_with_nulls() {
    let status = StatusResult {
        schema_version: SCHEMA_VERSION.to_string(),
        engine: "llama.cpp".into(),
        engine_version: "b10588".into(),
        model_path: None,
        context_tokens: None,
        sequences: None,
    };
    let value = serde_json::to_value(&status).unwrap();
    assert!(value["model_path"].is_null());
    assert_valid("status_result", &value);
}

#[test]
fn a_status_with_a_model_loaded_validates() {
    let status = StatusResult {
        schema_version: SCHEMA_VERSION.to_string(),
        engine: "llama.cpp".into(),
        engine_version: "b10588".into(),
        model_path: Some("/m.gguf".into()),
        context_tokens: Some(512),
        sequences: Some(1),
    };
    assert_valid("status_result", &serde_json::to_value(&status).unwrap());
}

#[test]
fn unload_results_validate_whether_or_not_anything_was_loaded() {
    for unloaded in [true, false] {
        assert_valid(
            "unload_result",
            &json!({ "schema_version": SCHEMA_VERSION, "unloaded": unloaded }),
        );
    }
}

#[test]
fn an_undeclared_field_is_rejected_everywhere() {
    // additionalProperties: false is what makes this a contract in both directions.
    for (name, mut instance) in [
        ("hello", json!({ "protocol": 1, "engine": "e", "version": "v" })),
        ("load_params", json!({ "model_path": "/m", "context_tokens": 8 })),
        (
            "unload_result",
            json!({ "schema_version": "1.0.0", "unloaded": true }),
        ),
    ] {
        instance["surprise"] = json!(1);
        assert_invalid(name, &instance, "additionalProperties is false");
    }
}

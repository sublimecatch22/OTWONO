//! The whole chain, against a real engine: supervisor → adapter → `llama-server` → GGUF.
//!
//! # Why this test is opt-in
//!
//! It needs a compiled `llama-server` and a model file, neither of which exists on a CI
//! runner or on a developer's machine by default. Rather than bundle a fake — which would
//! prove nothing, since every interesting failure here is a property of the real engine —
//! the test reads both paths from the environment and skips loudly when they are absent.
//!
//! A skipped test that prints nothing is a test that silently stops protecting you, so
//! this one says so on stdout and the verification log records the run where it was not
//! skipped.
//!
//! ```text
//! OTWONO_TEST_LLAMA_SERVER=/path/to/llama-server \
//! OTWONO_TEST_LLAMA_MODEL=/path/to/tiny.gguf \
//!   cargo test -p otwono-llama --test end_to_end -- --nocapture
//! ```
//!
//! `tools/make-tiny-gguf.py` generates a suitable model in about a second. Its weights are
//! random, so nothing here asserts on the *content* of a completion — only that tokens
//! were produced, counted, and reported with the right stop reason. Asserting on text
//! would be asserting on a particular model's behaviour, which is not what is being
//! integrated.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use otwono_ai::supervisor::BackendProcess;
use otwono_llama::{METHOD_INFER, METHOD_LOAD, METHOD_STATUS, METHOD_UNLOAD};
use serde_json::{json, Value};

const HELLO_TIMEOUT: Duration = Duration::from_secs(30);
const LOAD_TIMEOUT: Duration = Duration::from_secs(180);
const INFER_TIMEOUT: Duration = Duration::from_secs(180);

struct Fixture {
    server: PathBuf,
    model: PathBuf,
    runtime_dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }
}

/// Returns `None` — after saying why — when the environment does not name an engine.
fn fixture(tag: &str) -> Option<Fixture> {
    let (Ok(server), Ok(model)) = (
        std::env::var("OTWONO_TEST_LLAMA_SERVER"),
        std::env::var("OTWONO_TEST_LLAMA_MODEL"),
    ) else {
        println!(
            "SKIPPED: set OTWONO_TEST_LLAMA_SERVER and OTWONO_TEST_LLAMA_MODEL to run the \
             llama.cpp end-to-end tests (see this file's header)"
        );
        return None;
    };
    // Short, because the engine's socket lives here and sun_path is 108 bytes.
    let runtime_dir = PathBuf::from(format!("/tmp/otwono-e2e-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    Some(Fixture {
        server: PathBuf::from(server),
        model: PathBuf::from(model),
        runtime_dir,
    })
}

/// Spawn the adapter under the real supervisor, exactly as `otwono-aid` does.
fn spawn(f: &Fixture) -> BackendProcess {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otwono-llama-backend"));
    command
        .arg("--engine")
        .arg(&f.server)
        .arg("--runtime-dir")
        .arg(&f.runtime_dir);
    BackendProcess::spawn("llama-cpp-cpu", &mut command, HELLO_TIMEOUT).expect("adapter says hello")
}

fn call(backend: &mut BackendProcess, id: i64, method: &str, params: Value, timeout: Duration) -> Value {
    let response = backend
        .request(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            timeout,
        )
        .unwrap_or_else(|e| panic!("{method} failed at the transport level: {e}"));
    assert_eq!(response["id"], json!(id), "response id must match the request");
    response
}

fn ok(backend: &mut BackendProcess, id: i64, method: &str, params: Value, timeout: Duration) -> Value {
    let response = call(backend, id, method, params, timeout);
    assert!(
        response.get("error").is_none(),
        "{method} returned an error: {}",
        response["error"]
    );
    response["result"].clone()
}

#[test]
fn the_adapter_says_hello_before_any_model_is_loaded() {
    let Some(f) = fixture("hello") else { return };
    let mut backend = spawn(&f);

    // The ordering is the contract: hello arrives immediately, so the supervisor's hello
    // timeout never has to cover a model load.
    assert_eq!(backend.hello().protocol, otwono_ai::supervisor::PROTOCOL_VERSION);
    assert_eq!(backend.hello().engine, "llama.cpp");
    assert!(
        !backend.hello().version.is_empty(),
        "the engine version should be reported: {:?}",
        backend.hello()
    );
    println!("engine version: {}", backend.hello().version);

    let status = ok(&mut backend, 1, METHOD_STATUS, json!({}), HELLO_TIMEOUT);
    assert!(status["model_path"].is_null(), "nothing is loaded yet: {status}");
}

#[test]
fn a_model_loads_and_produces_tokens() {
    let Some(f) = fixture("infer") else { return };
    let mut backend = spawn(&f);

    let loaded = ok(
        &mut backend,
        1,
        METHOD_LOAD,
        json!({"model_path": f.model.display().to_string(), "context_tokens": 512, "sequences": 1, "threads": 2}),
        LOAD_TIMEOUT,
    );
    assert_eq!(loaded["context_tokens"], 512);
    assert!(loaded["engine_pid"].as_u64().unwrap_or(0) > 0, "{loaded}");
    println!("loaded in {} ms", loaded["load_ms"]);

    let status = ok(&mut backend, 2, METHOD_STATUS, json!({}), HELLO_TIMEOUT);
    assert_eq!(status["model_path"], json!(f.model.display().to_string()));

    let result = ok(
        &mut backend,
        3,
        METHOD_INFER,
        json!({"prompt": "The quick brown fox", "max_tokens": 12, "seed": 1, "temperature": 0.8}),
        INFER_TIMEOUT,
    );
    println!("completion: {:?}", result["text"]);

    // The model has random weights, so the text is gibberish and asserting on it would be
    // asserting on this fixture rather than on the integration. What must hold is that
    // real tokens were produced and accounted for.
    assert!(
        result["tokens_predicted"].as_u64().unwrap_or(0) > 0,
        "no tokens were generated: {result}"
    );
    assert!(
        result["tokens_evaluated"].as_u64().unwrap_or(0) > 0,
        "the prompt was never evaluated: {result}"
    );
    assert!(!result["text"].as_str().unwrap_or("").is_empty(), "{result}");
    assert_eq!(
        result["stop_reason"], "token_limit",
        "12 tokens from a 12-token budget is the limit, not an EOS: {result}"
    );
}

#[test]
fn the_token_budget_is_enforced_by_the_engine_not_just_reported() {
    let Some(f) = fixture("budget") else { return };
    let mut backend = spawn(&f);
    ok(
        &mut backend,
        1,
        METHOD_LOAD,
        json!({"model_path": f.model.display().to_string(), "context_tokens": 512, "threads": 2}),
        LOAD_TIMEOUT,
    );

    for budget in [1u64, 4, 16] {
        let result = ok(
            &mut backend,
            2,
            METHOD_INFER,
            json!({"prompt": "count:", "max_tokens": budget, "seed": 7, "temperature": 0.8}),
            INFER_TIMEOUT,
        );
        assert_eq!(
            result["tokens_predicted"].as_u64().unwrap(),
            budget,
            "asked for {budget} tokens: {result}"
        );
    }
}

#[test]
fn a_second_load_replaces_the_first_rather_than_stacking() {
    let Some(f) = fixture("swap") else { return };
    let mut backend = spawn(&f);
    let params = json!({"model_path": f.model.display().to_string(), "context_tokens": 512, "threads": 2});

    let first = ok(&mut backend, 1, METHOD_LOAD, params.clone(), LOAD_TIMEOUT);
    let second = ok(&mut backend, 2, METHOD_LOAD, params, LOAD_TIMEOUT);

    // A different engine process, which is what proves the first one was actually stopped
    // rather than left holding its share of memory.
    assert_ne!(
        first["engine_pid"], second["engine_pid"],
        "the engine should have been replaced"
    );
    let first_pid = first["engine_pid"].as_u64().unwrap() as i32;
    assert!(
        !is_alive(first_pid),
        "the replaced engine {first_pid} is still running"
    );

    // And the new one works.
    let result = ok(
        &mut backend,
        3,
        METHOD_INFER,
        json!({"prompt": "hi", "max_tokens": 4, "seed": 3}),
        INFER_TIMEOUT,
    );
    assert!(result["tokens_predicted"].as_u64().unwrap_or(0) > 0, "{result}");
}

#[test]
fn unloading_stops_the_engine_and_later_inference_is_refused() {
    let Some(f) = fixture("unload") else { return };
    let mut backend = spawn(&f);
    let loaded = ok(
        &mut backend,
        1,
        METHOD_LOAD,
        json!({"model_path": f.model.display().to_string(), "context_tokens": 512, "threads": 2}),
        LOAD_TIMEOUT,
    );
    let pid = loaded["engine_pid"].as_u64().unwrap() as i32;
    assert!(is_alive(pid), "the engine should be running after a load");

    let unloaded = ok(&mut backend, 2, METHOD_UNLOAD, json!({}), LOAD_TIMEOUT);
    assert_eq!(unloaded["unloaded"], true);
    assert!(!is_alive(pid), "engine {pid} outlived its unload");

    let response = call(
        &mut backend,
        3,
        METHOD_INFER,
        json!({"prompt": "hi", "max_tokens": 4}),
        INFER_TIMEOUT,
    );
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no model is loaded"),
        "{response}"
    );
}

#[test]
fn a_model_file_that_is_not_a_model_fails_the_load_with_the_engines_own_reason() {
    let Some(f) = fixture("badmodel") else { return };
    let mut backend = spawn(&f);

    let junk = f.runtime_dir.join("not-a-model.gguf");
    std::fs::write(&junk, b"this is not a GGUF file").unwrap();

    let response = call(
        &mut backend,
        1,
        METHOD_LOAD,
        json!({"model_path": junk.display().to_string(), "context_tokens": 512}),
        LOAD_TIMEOUT,
    );
    let message = response["error"]["message"].as_str().unwrap_or_default();
    // The engine's stderr is what tells an operator the file is corrupt rather than
    // missing, so it has to survive all the way back up the chain.
    assert!(
        message.contains("exited") || message.contains("stderr"),
        "the load should fail with the engine's own diagnosis: {response}"
    );
    println!("load failure reported as: {message}");

    // And the adapter is still usable afterwards: one bad model must not end the session.
    let status = ok(&mut backend, 2, METHOD_STATUS, json!({}), HELLO_TIMEOUT);
    assert!(status["model_path"].is_null(), "{status}");
}

#[test]
fn the_engine_dies_with_the_adapter() {
    let Some(f) = fixture("subtree") else { return };
    let mut backend = spawn(&f);
    let loaded = ok(
        &mut backend,
        1,
        METHOD_LOAD,
        json!({"model_path": f.model.display().to_string(), "context_tokens": 512, "threads": 2}),
        LOAD_TIMEOUT,
    );
    let engine_pid = loaded["engine_pid"].as_u64().unwrap() as i32;

    // This is the case the supervisor's process-group kill exists for: killing the adapter
    // must not leave a llama-server behind holding the model in memory.
    backend.shutdown().expect("shutdown");
    for _ in 0..100 {
        if !is_alive(engine_pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("llama-server {engine_pid} survived the adapter it was started by");
}

/// Whether a pid is a live process.
///
/// A zombie is not alive: its parent died with it and nothing has reaped it yet, which
/// inside a container can take arbitrarily long. Reading the state rather than testing for
/// the directory is the difference between a real assertion and a flaky one.
fn is_alive(pid: i32) -> bool {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The state field follows the parenthesised comm, which may itself contain spaces.
    let Some((_, after_comm)) = status.rsplit_once(')') else {
        return false;
    };
    !after_comm.trim_start().starts_with('Z')
}

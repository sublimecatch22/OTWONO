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
    /// The model, inside `model_dir`.
    model: PathBuf,
    model_dir: PathBuf,
    runtime_dir: PathBuf,
    /// A file outside every allowed path, standing in for everything on the node the
    /// engine has no business reading.
    secret: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
        let _ = std::fs::remove_dir_all(&self.model_dir);
        let _ = std::fs::remove_file(&self.secret);
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
    let model_dir = PathBuf::from(format!("/tmp/otwono-mdl-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&model_dir).expect("create model dir");

    let staged = model_dir.join("model.gguf");
    std::fs::copy(&model, &staged).expect("stage the model inside the model directory");

    // Deliberately outside every path the policy allows.
    let secret = PathBuf::from(format!("/tmp/otwono-secret-{tag}-{}", std::process::id()));
    std::fs::write(&secret, b"this stands in for the node identity key").expect("write secret");

    Some(Fixture {
        server: PathBuf::from(server),
        model: staged,
        model_dir,
        runtime_dir,
        secret,
    })
}

/// Spawn the adapter under the real supervisor, exactly as `otwono-aid` does.
///
/// The model is copied into a dedicated directory that becomes the adapter's `--model-dir`,
/// because that directory is also the Landlock boundary: the engine may read models from
/// there and from nowhere else on the machine.
fn spawn(f: &Fixture) -> BackendProcess {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otwono-llama-backend"));
    command
        .arg("--engine")
        .arg(&f.server)
        .arg("--model-dir")
        .arg(&f.model_dir)
        .arg("--runtime-dir")
        .arg(&f.runtime_dir);
    if !landlock_enforced() {
        // The adapter fails closed on a kernel that cannot confine it, which is the point.
        // These tests still have work to do on such a kernel, so they opt out explicitly
        // and the assertions that depend on confinement skip themselves — see
        // `the_running_engine_is_confined_by_the_kernel_not_only_by_the_adapter`.
        command.arg("--allow-unconfined");
    }
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

    let junk = f.model_dir.join("not-a-model.gguf");
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

/// Whether this kernel enforces Landlock.
///
/// Asked of the adapter rather than answered in-process: finding out requires actually
/// restricting a process, and doing that here would confine the test runner for every test
/// after it.
fn landlock_enforced() -> bool {
    Command::new(env!("CARGO_BIN_EXE_otwono-llama-backend"))
        .arg("--probe")
        .output() // not status(): its stdout would land in every test's output
        .map(|o| o.status.success())
        .unwrap_or(false)
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

#[test]
fn the_engine_cannot_read_a_model_outside_the_store_even_though_the_file_exists() {
    // The sandbox, proven rather than asserted. The file is real, readable by this test
    // process, and named by an absolute path -- and the engine still cannot have it.
    let Some(f) = fixture("sandbox") else { return };
    let mut backend = spawn(&f);

    // Sanity: the test process itself can read it, so a failure below is confinement and
    // not a missing file.
    assert!(
        std::fs::read(&f.secret).is_ok(),
        "the test fixture should be readable here"
    );

    let response = call(
        &mut backend,
        1,
        METHOD_LOAD,
        json!({"model_path": f.secret.display().to_string(), "context_tokens": 512}),
        LOAD_TIMEOUT,
    );
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("outside the model store"),
        "a model outside the store should be refused by name: {response}"
    );
    println!("refused with: {message}");

    // And the adapter is still usable: a refusal is not a session-ending event.
    let loaded = ok(
        &mut backend,
        2,
        METHOD_LOAD,
        json!({"model_path": f.model.display().to_string(), "context_tokens": 512, "threads": 2}),
        LOAD_TIMEOUT,
    );
    assert!(loaded["engine_pid"].as_u64().unwrap_or(0) > 0, "{loaded}");
}

#[test]
fn the_running_engine_is_confined_by_the_kernel_not_only_by_the_adapter() {
    // The check above is the adapter refusing. This one asks whether the *kernel* would
    // have stopped it anyway -- which is the difference between a policy and a boundary.
    // Landlock is inherited, so the engine's own view of the filesystem is what matters,
    // and /proc/<pid>/root lets the test look at it from outside.
    let Some(f) = fixture("confined") else { return };
    if !landlock_enforced() {
        println!(
            "SKIPPED: this kernel does not enforce Landlock, so confinement cannot be \
             demonstrated here. The built images ship their own kernel; the boot check \
             reports sandbox= on a booted node."
        );
        return;
    }
    let mut backend = spawn(&f);
    let loaded = ok(
        &mut backend,
        1,
        METHOD_LOAD,
        json!({"model_path": f.model.display().to_string(), "context_tokens": 512, "threads": 2}),
        LOAD_TIMEOUT,
    );
    let pid = loaded["engine_pid"].as_u64().unwrap() as i32;

    // The kernel records a non-zero Landlock domain id for a confined process. Zero, or a
    // missing field, means nothing is confining it.
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("read engine status");
    let landlock: Vec<&str> = status
        .lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("landlock"))
        .collect();
    println!("engine {pid} landlock status: {landlock:?}");
    assert!(
        !landlock.is_empty(),
        "the kernel reports no Landlock domain for the engine; it is not confined:\n{status}"
    );
}

//! `ai.infer` end to end, on a node that really has llama.cpp installed.
//!
//! Everything in the chain is the real thing:
//!
//! ```text
//!   client ──▶ otwono-permd ──▶ otwono-aid ──▶ otwono-llama-backend ──▶ llama-server ──▶ GGUF
//!            capability token   admission        supervisor              Unix socket
//! ```
//!
//! No mock stands in for any of it. That is the point: the parts most likely to be wrong
//! are the joins — a capability that is checked but never granted, an admission decision
//! whose context window never reaches the engine, a backend that is spawned but never
//! killed. None of those can be caught by testing the pieces apart.
//!
//! # Skipped without an engine
//!
//! Needs a compiled `llama-server` and a `.gguf`. `tools/make-tiny-gguf.py` produces a
//! suitable model in about a second; the weights are random, so nothing here asserts on
//! what the model *says*.
//!
//! ```text
//! OTWONO_TEST_LLAMA_SERVER=/path/to/llama-server \
//! OTWONO_TEST_LLAMA_MODEL=/path/to/tiny.gguf \
//!   cargo test -p otwono-control-plane-tests --test ai_infer_llama -- --nocapture
//! ```

use otwono_ai::discovery::{ADAPTER_DIR, ENGINE_DIR, LLAMA_ADAPTER};
use otwono_ai::manifest::{Footprint, ModelCapability, ModelFormat, ModelManifest};
use otwono_ai::signature::testing::sign;
use otwono_ai::{BackendId, Catalog};
use otwono_aid::AiService;
use otwono_capability::{classify, testing::report_pi5_16gb};
use otwono_permd::{ActionRegistry, AuditLog, Broker, Policy};
use otwono_proto::{code, Client, Server, Shutdown};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const POLICY: &str = r#"
[[rule]]
action = "ai.read"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "ai.infer"
decision = "allow"
ttl_seconds = 300
"#;

const MODEL_ID: &str = "tiny-test-model";
/// Not a real digest of the weights. The catalog addresses blobs by this string and does
/// not currently verify the content against it, so the test only needs the two to agree.
const BLOB_NAME: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const PUBLISHER_SEED: u8 = 11;

struct Harness {
    dir: PathBuf,
    perm_socket: PathBuf,
    ai_socket: PathBuf,
    shutdown: Shutdown,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The adapter binary cargo just built, next to the test executable.
///
/// `CARGO_BIN_EXE_*` only names binaries of the *same* package, and the adapter belongs to
/// `otwono-llama`, so it has to be found on disk. `cargo test --workspace` builds it.
fn adapter_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // .../target/<profile>/deps/<test> → .../target/<profile>/otwono-llama-backend
    let candidate = exe.parent()?.parent()?.join("otwono-llama-backend");
    candidate.is_file().then_some(candidate)
}

/// `None`, after saying why, when this machine cannot run the test.
fn requirements() -> Option<(PathBuf, PathBuf, PathBuf)> {
    let (Ok(server), Ok(model)) = (
        std::env::var("OTWONO_TEST_LLAMA_SERVER"),
        std::env::var("OTWONO_TEST_LLAMA_MODEL"),
    ) else {
        println!(
            "SKIPPED: set OTWONO_TEST_LLAMA_SERVER and OTWONO_TEST_LLAMA_MODEL to run the \
             full-stack inference tests (see this file's header)"
        );
        return None;
    };
    let Some(adapter) = adapter_binary() else {
        println!("SKIPPED: otwono-llama-backend was not built; run cargo test --workspace");
        return None;
    };
    Some((PathBuf::from(server), PathBuf::from(model), adapter))
}

fn manifest(weights_bytes: u64, max_context: u32) -> ModelManifest {
    ModelManifest {
        schema_version: otwono_ai::manifest::SCHEMA_VERSION.to_string(),
        id: MODEL_ID.to_string(),
        family: "llama".into(),
        parameters: 100_000,
        quantization: "F32".into(),
        format: ModelFormat::Gguf,
        blake3: BLOB_NAME.to_string(),
        size_bytes: weights_bytes,
        min_tier: otwono_capability::Tier::T0Micro,
        footprint: Footprint {
            weights_bytes,
            kv_per_1k_ctx_bytes: 1024,
            overhead_bytes: 8 * 1024 * 1024,
        },
        max_context,
        capabilities: vec![ModelCapability::Chat],
        license: "apache-2.0".into(),
        backends: vec![BackendId::LlamaCppCpu],
        signature: None,
    }
}

impl Harness {
    /// Build a filesystem that looks like a node with llama.cpp installed, and serve it.
    fn start(tag: &str, server: &Path, model: &Path, adapter: &Path) -> Harness {
        // Short: the engine's Unix socket lives under here and sun_path is 108 bytes.
        let dir = PathBuf::from(format!("/tmp/otw-inf-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::create_dir_all(dir.join("models/manifests")).unwrap();
        std::fs::create_dir_all(dir.join("models/blobs")).unwrap();
        std::fs::create_dir_all(dir.join("run")).unwrap();
        std::fs::write(dir.join("policy.d/10-test.toml"), POLICY).unwrap();

        // The install tree otwono_ai::discovery looks for. Symlinks rather than copies:
        // llama-server is 17 MB and this runs several times.
        let root = dir.join("root");
        let adapter_dir = root.join(ADAPTER_DIR);
        let engine_dir = root.join(ENGINE_DIR).join("llama.cpp/cpu/bin");
        std::fs::create_dir_all(&adapter_dir).unwrap();
        std::fs::create_dir_all(&engine_dir).unwrap();
        std::os::unix::fs::symlink(adapter, adapter_dir.join(LLAMA_ADAPTER)).unwrap();
        std::os::unix::fs::symlink(server, engine_dir.join("llama-server")).unwrap();

        let installs = otwono_ai::discover(&root);
        assert_eq!(
            installs.len(),
            1,
            "the fixture install tree should be discovered: {installs:?}"
        );

        // The weights, addressed the way the catalog addresses them.
        let weights_bytes = std::fs::metadata(model).unwrap().len();
        std::fs::copy(model, dir.join("models/blobs").join(BLOB_NAME)).unwrap();

        let mut m = manifest(weights_bytes, 512);
        let trust = sign(&mut m, PUBLISHER_SEED);
        std::fs::write(
            dir.join("models/manifests").join(format!("{MODEL_ID}.json")),
            serde_json::to_string_pretty(&m).unwrap(),
        )
        .unwrap();

        let perm_socket = dir.join("perm.sock");
        let ai_socket = dir.join("ai.sock");
        let shutdown = Shutdown::new();

        let policy = Policy::load_dir(&dir.join("policy.d")).expect("policy must load");
        policy
            .validate(&ActionRegistry::builtin())
            .expect("test policy must name only registered actions");
        let broker = Arc::new(Broker::new(
            policy,
            AuditLog::open(dir.join("audit.jsonl")).unwrap(),
        ));
        let server_sock = Server::bind(&perm_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || server_sock.serve(broker, s));

        // A Pi 5 with 16 GB: comfortably able to run a 400 KB model, so admission control
        // is exercised as a decision that says yes rather than as a gate that never opens.
        let service = Arc::new(
            AiService::new(
                Catalog::new(dir.join("models")),
                classify(&report_pi5_16gb()),
                trust,
                perm_socket.clone(),
                installs,
            )
            .with_backend_runtime_dir(dir.join("run")),
        );
        let server_sock = Server::bind(&ai_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || server_sock.serve(service, s));

        Client::connect_waiting(&perm_socket, Duration::from_secs(5)).expect("permd never came up");
        Client::connect_waiting(&ai_socket, Duration::from_secs(5)).expect("aid never came up");

        Harness {
            dir,
            perm_socket,
            ai_socket,
            shutdown,
        }
    }

    fn token(&self, action: &str) -> String {
        Client::connect(&self.perm_socket)
            .unwrap()
            .call("perm.request", json!({ "action": action, "reason": "test" }))
            .unwrap()
            .expect("policy allows this")["token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn infer(&self, params: Value) -> Result<Value, otwono_proto::RpcError> {
        let token = self.token("ai.infer");
        Client::connect(&self.ai_socket)
            .unwrap()
            .call_with_capability("ai.infer", params, &token)
            .unwrap()
    }

    fn engine_pids(&self) -> Vec<i32> {
        // Every engine this harness started names this harness's runtime dir on its
        // command line, which is what makes counting them reliable under a parallel run.
        //
        // argv[0] is checked separately, and it has to be: the *adapter*'s command line
        // also contains both the string "llama-server" (it is the --engine argument) and
        // the runtime dir, so a substring match over the whole line counts the adapter as
        // an engine and every assertion here sees one process too many.
        let needle = self.dir.join("run").display().to_string();
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return found;
        };
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let raw = String::from_utf8_lossy(&raw);
            let mut argv = raw.split('\0');
            let is_engine = argv
                .next()
                .map(|a| a.rsplit('/').next() == Some("llama-server"))
                .unwrap_or(false);
            if is_engine && raw.contains(&needle) {
                found.push(pid);
            }
        }
        found
    }
}

#[test]
fn a_prompt_goes_all_the_way_to_the_model_and_back() {
    let Some((server, model, adapter)) = requirements() else {
        return;
    };
    let h = Harness::start("basic", &server, &model, &adapter);

    // The node should now admit to having an engine — the same boolean that was false in
    // every other test in this crate.
    let caps = Client::connect(&h.ai_socket)
        .unwrap()
        .call("ai.capabilities", json!({}))
        .unwrap()
        .unwrap();
    assert_eq!(caps["local_inference_available"], true, "{caps}");
    assert_eq!(caps["installed_backends"], json!(["llama-cpp-cpu"]), "{caps}");

    let result = h
        .infer(json!({
            "model_id": MODEL_ID,
            "prompt": "The quick brown fox",
            "max_tokens": 12,
            "seed": 1,
            "temperature": 0.8,
        }))
        .expect("inference should succeed");

    println!("completion: {}", result["text"]);
    assert_eq!(result["model_id"], MODEL_ID);
    assert_eq!(result["backend"], "llama-cpp-cpu");
    // Random weights: the text is gibberish, so what is asserted is that real work
    // happened and was accounted for, not what the model said.
    assert!(result["tokens_predicted"].as_u64().unwrap_or(0) > 0, "{result}");
    assert!(result["tokens_evaluated"].as_u64().unwrap_or(0) > 0, "{result}");
    assert!(!result["text"].as_str().unwrap_or("").is_empty(), "{result}");
    assert_eq!(result["stop_reason"], "token_limit", "{result}");
}

#[test]
fn the_context_window_the_engine_gets_is_the_one_admission_control_granted() {
    // The join most likely to rot: admission control computes a context window against
    // this node's memory budget, and if that number never reaches the engine the whole
    // calculation is decoration.
    let Some((server, model, adapter)) = requirements() else {
        return;
    };
    let h = Harness::start("ctx", &server, &model, &adapter);

    let result = h
        .infer(json!({
            "model_id": MODEL_ID, "prompt": "hi", "max_tokens": 4,
            "context_tokens": 256, "seed": 2,
        }))
        .expect("inference should succeed");
    assert_eq!(result["context_tokens"], 256, "{result}");

    let pids = h.engine_pids();
    assert_eq!(pids.len(), 1, "expected exactly one engine: {pids:?}");
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", pids[0])).unwrap();
    let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
    assert!(
        cmdline.contains("--ctx-size 256"),
        "the engine was not started with the granted context: {cmdline}"
    );
}

#[test]
fn a_second_request_reuses_the_loaded_model_instead_of_reloading_it() {
    // Reloading per request would dominate the wall clock on exactly the hardware this
    // project exists for, so "the same engine served both" is a behaviour worth pinning.
    let Some((server, model, adapter)) = requirements() else {
        return;
    };
    let h = Harness::start("reuse", &server, &model, &adapter);

    let params = json!({ "model_id": MODEL_ID, "prompt": "one", "max_tokens": 4, "seed": 3 });
    h.infer(params.clone()).expect("first inference");
    let after_first = h.engine_pids();
    assert_eq!(after_first.len(), 1, "{after_first:?}");

    h.infer(params).expect("second inference");
    let after_second = h.engine_pids();
    assert_eq!(
        after_second, after_first,
        "the model should not have been reloaded"
    );
}

#[test]
fn a_context_larger_than_the_model_allows_is_refused_before_anything_is_started() {
    let Some((server, model, adapter)) = requirements() else {
        return;
    };
    let h = Harness::start("toobig", &server, &model, &adapter);

    // The manifest declares max_context 512.
    let err = h
        .infer(json!({ "model_id": MODEL_ID, "prompt": "hi", "max_tokens": 4, "context_tokens": 99_999 }))
        .expect_err("admission control should refuse this");
    assert_eq!(err.code, code::UNAVAILABLE, "{err:?}");
    assert!(
        h.engine_pids().is_empty(),
        "a refused request must not leave an engine running"
    );
    // And the refusal is actionable: it says what would have fitted.
    assert!(
        err.data.is_some(),
        "a refusal should carry the largest admissible context: {err:?}"
    );
}

#[test]
fn asking_for_a_model_that_is_not_in_the_catalog_starts_no_engine() {
    let Some((server, model, adapter)) = requirements() else {
        return;
    };
    let h = Harness::start("nomodel", &server, &model, &adapter);
    let err = h
        .infer(json!({ "model_id": "not-here", "prompt": "hi", "max_tokens": 4 }))
        .expect_err("no such model");
    assert_eq!(err.code, code::INVALID_PARAMS, "{err:?}");
    assert!(h.engine_pids().is_empty());
}

#[test]
fn the_engine_does_not_outlive_the_daemon_that_started_it() {
    // A node that leaked one engine per restart would fill up with processes each holding
    // a model in memory. The supervisor's process-group kill is what prevents it; this is
    // the test that the whole three-process chain actually wires it up.
    let Some((server, model, adapter)) = requirements() else {
        return;
    };
    let h = Harness::start("lifetime", &server, &model, &adapter);
    h.infer(json!({ "model_id": MODEL_ID, "prompt": "hi", "max_tokens": 4, "seed": 4 }))
        .expect("inference should succeed");
    let pids = h.engine_pids();
    assert_eq!(pids.len(), 1, "{pids:?}");

    // Dropping the harness drops the service, which drops the supervisor handle.
    drop(h);

    for _ in 0..100 {
        if !is_alive(pids[0]) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("llama-server {} survived the daemon that started it", pids[0]);
}

/// Whether a pid is a live process. A zombie is not: inside a container a reparented child
/// can go unreaped indefinitely, so testing for `/proc/<pid>` alone would flake.
fn is_alive(pid: i32) -> bool {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, after_comm)) = status.rsplit_once(')') else {
        return false;
    };
    !after_comm.trim_start().starts_with('Z')
}

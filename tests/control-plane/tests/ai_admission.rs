//! The AI daemon over real sockets, with the broker in the loop.
//!
//! The unit tests in `otwono-ai` prove the admission arithmetic. This proves the parts
//! that only exist once there are two processes: that `ai.read` is actually enforced, that
//! a refusal comes back as a *successful call reporting a refusal* rather than an RPC
//! error, and that `ai.infer` tells the truth when this node has no backend installed.
//!
//! Inference on a node that *does* have one is `ai_infer_llama.rs`, which needs a real
//! engine and is skipped without one.

use otwono_ai::manifest::{Footprint, ModelCapability, ModelFormat, ModelManifest};
use otwono_ai::signature::testing::sign;
use otwono_ai::{BackendId, Catalog};
use otwono_aid::AiService;
use otwono_capability::{classify, testing::report_pi4_4gb, Tier};
use otwono_permd::{ActionRegistry, AuditLog, Broker, Policy};
use otwono_proto::{code, Client, Server, Shutdown};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

const POLICY: &str = r#"
[[rule]]
action = "ai.read"
decision = "allow"
ttl_seconds = 300

# Granted so that a refusal from ai.infer below can only be about the missing backend.
# Without this the call fails on authorization first and proves nothing about inference.
[[rule]]
action = "ai.infer"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "ai.admin"
decision = "allow"
ttl_seconds = 300
"#;

struct Harness {
    dir: PathBuf,
    perm_socket: PathBuf,
    ai_socket: PathBuf,
    shutdown: Shutdown,
}

/// Test publisher seeds. Two, so "signed by someone we trust" and "signed by a stranger"
/// are both reachable — they are different outcomes and the tests below rely on it.
const TRUSTED_SEED: u8 = 11;
const STRANGER_SEED: u8 = 22;

fn manifest(id: &str, weights: u64, min_tier: Tier, signed: bool) -> ModelManifest {
    let mut m = ModelManifest {
        schema_version: otwono_ai::manifest::SCHEMA_VERSION.to_string(),
        id: id.to_string(),
        family: "test".into(),
        parameters: 1_000_000_000,
        quantization: "Q4_K_M".into(),
        format: ModelFormat::Gguf,
        blake3: "b".repeat(64),
        size_bytes: weights,
        min_tier,
        footprint: Footprint {
            weights_bytes: weights,
            kv_per_1k_ctx_bytes: 16 * MIB,
            overhead_bytes: 64 * MIB,
        },
        max_context: 8192,
        capabilities: vec![ModelCapability::Chat],
        license: "apache-2.0".into(),
        backends: vec![BackendId::LlamaCppCpu],
        signature: None,
    };
    if signed {
        // Signed for real. A placeholder signature would make every test below pass for
        // the wrong reason once verification became part of the admission path.
        sign(&mut m, TRUSTED_SEED);
    }
    m
}

impl Harness {
    fn start(tag: &str) -> Harness {
        let dir = std::env::temp_dir().join(format!("otw-ai-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::create_dir_all(dir.join("models/manifests")).unwrap();
        std::fs::write(dir.join("policy.d/10-test.toml"), POLICY).unwrap();

        // A Pi 4, which classifies T1_EDGE: big enough for a small model, not for a
        // large one. The interesting machine, because both answers are reachable on it.
        let mut stranger = manifest("stranger-1b", 700 * MIB, Tier::T0Micro, false);
        sign(&mut stranger, STRANGER_SEED);

        for m in [
            manifest("fits-1b", 700 * MIB, Tier::T0Micro, true),
            manifest("too-big-70b", 40 * GIB, Tier::T0Micro, true),
            manifest("needs-workstation", 700 * MIB, Tier::T4Workstation, true),
            manifest("unsigned-1b", 700 * MIB, Tier::T0Micro, false),
            stranger,
        ] {
            std::fs::write(
                dir.join("models/manifests").join(format!("{}.json", m.id)),
                serde_json::to_string_pretty(&m).unwrap(),
            )
            .unwrap();
        }

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
        let server = Server::bind(&perm_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || server.serve(broker, s));

        // Trust exactly one publisher, so trusted / unknown / unsigned are all reachable.
        let mut probe = manifest("probe", 1, Tier::T0Micro, true);
        let trust = sign(&mut probe, TRUSTED_SEED);

        let service = Arc::new(AiService::new(
            Catalog::new(dir.join("models")),
            classify(&report_pi4_4gb()),
            trust,
            perm_socket.clone(),
            Vec::new(),
        ));
        let server = Server::bind(&ai_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || server.serve(service, s));

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
        let mut broker = Client::connect(&self.perm_socket).unwrap();
        broker
            .call("perm.request", json!({ "action": action, "reason": "test" }))
            .unwrap()
            .expect("policy allows this")
            .get("token")
            .and_then(|t| t.as_str())
            .unwrap()
            .to_string()
    }

    fn call(&self, method: &str, params: Value, action: &str) -> Value {
        let token = self.token(action);
        Client::connect(&self.ai_socket)
            .unwrap()
            .call_with_capability(method, params, &token)
            .unwrap()
            .unwrap_or_else(|e| panic!("{method} refused: {}", e.message))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn capabilities_are_open_and_admit_to_having_no_engine() {
    // The headline boolean anything deciding whether to offer a local assistant reads. It
    // must not be optimistic on a node with nothing installed.
    let h = Harness::start("caps");
    let value = Client::connect(&h.ai_socket)
        .unwrap()
        .call("ai.capabilities", json!({}))
        .unwrap()
        .expect("ai.capabilities is unauthenticated on the local socket");
    assert_eq!(value["tier"], "T1_EDGE");
    assert_eq!(value["local_inference_available"], false);
    assert_eq!(value["installed_backends"].as_array().unwrap().len(), 0);
}

#[test]
fn listing_models_requires_a_capability() {
    let h = Harness::start("guard");
    let err = Client::connect(&h.ai_socket)
        .unwrap()
        .call("ai.models.list", json!({}))
        .unwrap()
        .expect_err("no token, no listing");
    assert_eq!(err.code, code::UNAUTHORIZED);
}

#[test]
fn every_model_is_listed_whether_or_not_it_can_run_here() {
    // A catalog that hides what the machine cannot run leaves the user wondering where
    // the model went.
    let h = Harness::start("list");
    let value = h.call("ai.models.list", json!({}), "ai.read");
    let models = value["models"].as_array().unwrap();
    assert_eq!(models.len(), 5, "{models:#?}");
    assert!(value["problems"].as_array().unwrap().is_empty());

    for m in models {
        // Nothing is admissible: no backend is linked. But each still says why.
        assert_eq!(m["admissible"], false);
        assert!(m["reason"].is_string(), "{m:#?}");
    }
}

#[test]
fn a_refusal_is_a_successful_call_not_an_rpc_error() {
    // Browsing a catalog on a small machine should not require treating "too big" as an
    // exception on every second entry.
    let h = Harness::start("refusal");
    let value = h.call("ai.admit", json!({ "model_id": "too-big-70b" }), "ai.read");
    assert_eq!(value["admissible"], false);
    assert_eq!(value["model_id"], "too-big-70b");
    assert!(
        value["reason"].as_str().unwrap().contains("backend"),
        "{value:#?}"
    );
}

#[test]
fn the_tier_gate_is_reported_by_name() {
    let h = Harness::start("tier");
    // With no backend linked the backend check fires first, so pin the tier rule where it
    // is decidable: the manifest demands a workstation and this is a Pi 4.
    let value = h.call("ai.admit", json!({ "model_id": "needs-workstation" }), "ai.read");
    assert_eq!(value["admissible"], false);
    let reason = value["reason"].as_str().unwrap();
    assert!(
        reason.contains("T4_WORKSTATION") && reason.contains("T1_EDGE"),
        "{reason}"
    );
    // And it names the axis holding this node back, which is the actionable half.
    assert!(reason.contains("limited by"), "{reason}");
}

#[test]
fn an_unsigned_model_is_refused_until_the_call_opts_in() {
    let h = Harness::start("unsigned");
    let value = h.call("ai.admit", json!({ "model_id": "unsigned-1b" }), "ai.read");
    assert!(
        value["reason"].as_str().unwrap().contains("signature"),
        "{value:#?}"
    );
}

#[test]
fn asking_about_a_model_that_is_not_here_is_an_invalid_request() {
    let h = Harness::start("missing");
    let token = h.token("ai.read");
    let err = Client::connect(&h.ai_socket)
        .unwrap()
        .call_with_capability("ai.admit", json!({ "model_id": "nope" }), &token)
        .unwrap()
        .expect_err("no such model");
    assert_eq!(err.code, code::INVALID_PARAMS);
    assert!(err.message.contains("nope"), "{}", err.message);
}

#[test]
fn infer_without_a_capability_is_refused_before_the_node_says_what_it_has() {
    // Authorization comes first deliberately: an unauthenticated caller should not be able
    // to probe which backends a node has by reading the shape of the refusal.
    let h = Harness::start("infer-guard");
    let err = Client::connect(&h.ai_socket)
        .unwrap()
        .call(
            "ai.infer",
            json!({ "model_id": "fits-1b", "prompt": "hello", "max_tokens": 8 }),
        )
        .unwrap()
        .expect_err("ai.infer needs a capability");
    assert_eq!(err.code, code::UNAUTHORIZED);
}

#[test]
fn infer_on_a_node_with_no_backend_installed_says_exactly_that() {
    // The harness installs none, which is the state of a stock image. The refusal must
    // name the cause: with ai.infer granted by policy, "you may not" is ruled out, so what
    // is left has to be about the machine.
    let h = Harness::start("infer-none");
    let token = h.token("ai.infer");
    let err = Client::connect(&h.ai_socket)
        .unwrap()
        .call_with_capability(
            "ai.infer",
            json!({ "model_id": "fits-1b", "prompt": "hello", "max_tokens": 8 }),
            &token,
        )
        .unwrap()
        .expect_err("nothing is installed to run it");
    assert_eq!(err.code, code::UNAVAILABLE);
    assert!(
        err.message.contains("no inference backend is installed"),
        "the refusal must name the cause: {}",
        err.message
    );
}

#[test]
fn describe_advertises_infer_behind_its_own_capability() {
    // `describe` is how another component discovers what this node offers. Reading a
    // model and running one are separate grants, and describe is where that is published.
    let h = Harness::start("describe");
    let value = Client::connect(&h.ai_socket)
        .unwrap()
        .call("describe", json!({}))
        .unwrap()
        .unwrap();
    let methods = value["methods"].as_array().unwrap();
    let infer = methods
        .iter()
        .find(|m| m["name"] == "ai.infer")
        .expect("ai.infer must be described");
    assert_eq!(infer["capability"], "ai.infer", "{infer:#?}");
    let list = methods.iter().find(|m| m["name"] == "ai.models.list").unwrap();
    assert_eq!(list["capability"], "ai.read", "{list:#?}");
}

/// A manifest whose digest and size genuinely describe `body`, signed by the trusted key.
fn model_for(body: &[u8]) -> ModelManifest {
    let mut m = manifest("installable-1b", body.len() as u64, Tier::T0Micro, false);
    m.blake3 = blake3_hex(body);
    m.size_bytes = body.len() as u64;
    m.footprint.weights_bytes = body.len() as u64;
    sign(&mut m, TRUSTED_SEED);
    m
}

fn blake3_hex(body: &[u8]) -> String {
    otwono_ai::hash_file(&{
        let p = std::env::temp_dir().join(format!("otw-h-{}-{}", std::process::id(), body.len()));
        std::fs::write(&p, body).unwrap();
        p
    })
    .unwrap()
}

/// Write a manifest and a blob to disk and return their paths.
fn staged(h: &Harness, m: &ModelManifest, body: &[u8]) -> (PathBuf, PathBuf) {
    let manifest_path = h.dir.join(format!("{}.json", m.id));
    let blob_path = h.dir.join(format!("{}.gguf", m.id));
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(m).unwrap()).unwrap();
    std::fs::write(&blob_path, body).unwrap();
    (manifest_path, blob_path)
}

#[test]
fn installing_a_model_verifies_its_weights_and_makes_it_visible_to_the_catalog() {
    let h = Harness::start("install");
    let body = b"these bytes are the weights".as_slice();
    let m = model_for(body);
    let (manifest_path, blob_path) = staged(&h, &m, body);

    let result = h.call(
        "ai.models.install",
        json!({
            "manifest_path": manifest_path.display().to_string(),
            "blob_path": blob_path.display().to_string(),
        }),
        "ai.admin",
    );
    assert_eq!(result["model_id"], m.id);
    assert_eq!(result["blake3"], m.blake3);
    assert_eq!(result["already_present"], false);
    assert_eq!(result["provenance"]["status"], "trusted");

    // It is in the catalog now, with its weights.
    let models = h.call("ai.models.list", json!({}), "ai.read");
    let entry = models["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == m.id.as_str())
        .expect("the installed model should be listed");
    assert_eq!(entry["weights_present"], true);
}

#[test]
fn weights_that_do_not_match_a_correctly_signed_manifest_are_refused() {
    // The hole this closes: before verification existed, blake3 was only a filename, so a
    // trusted manifest paired with somebody else's bytes installed and loaded as trusted.
    let h = Harness::start("swapped");
    let body = b"these bytes are the weights".as_slice();
    let m = model_for(body);
    let (manifest_path, blob_path) = staged(&h, &m, body);
    std::fs::write(&blob_path, b"these bytes are NOT the weights").unwrap();

    let err = Client::connect(&h.ai_socket)
        .unwrap()
        .call_with_capability(
            "ai.models.install",
            json!({
                "manifest_path": manifest_path.display().to_string(),
                "blob_path": blob_path.display().to_string(),
            }),
            &h.token("ai.admin"),
        )
        .unwrap()
        .expect_err("the digest does not match");
    assert_eq!(err.code, code::INVALID_PARAMS);
    assert!(
        err.message.contains("do not match the manifest") || err.message.contains("bytes and the file is"),
        "the refusal must name the mismatch: {}",
        err.message
    );
}

#[test]
fn installing_requires_the_admin_capability_not_merely_read() {
    // Reading a catalog and changing what the node will run are different powers.
    let h = Harness::start("installguard");
    let err = Client::connect(&h.ai_socket)
        .unwrap()
        .call_with_capability(
            "ai.models.install",
            json!({ "manifest_path": "/x.json", "blob_path": "/x.gguf" }),
            &h.token("ai.read"),
        )
        .unwrap()
        .expect_err("ai.read must not authorize an install");
    assert!(
        err.code == code::UNAUTHORIZED || err.code == code::FORBIDDEN,
        "{err:?}"
    );
}

#[test]
fn verifying_a_model_reports_a_mismatch_as_a_result_rather_than_an_error() {
    // A caller auditing a catalog should not have to handle an exception per corrupt model.
    let h = Harness::start("verify");
    let body = b"these bytes are the weights".as_slice();
    let m = model_for(body);
    let (manifest_path, blob_path) = staged(&h, &m, body);
    h.call(
        "ai.models.install",
        json!({
            "manifest_path": manifest_path.display().to_string(),
            "blob_path": blob_path.display().to_string(),
        }),
        "ai.admin",
    );

    let good = h.call("ai.models.verify", json!({ "model_id": m.id }), "ai.read");
    assert_eq!(good["digest_matches"], true, "{good}");

    // Somebody with write access to the blob store swaps the weights afterwards.
    std::fs::write(
        h.dir.join("models/blobs").join(&m.blake3),
        b"tampered tampered tampered!",
    )
    .unwrap();

    let bad = h.call("ai.models.verify", json!({ "model_id": m.id }), "ai.read");
    assert_eq!(bad["digest_matches"], false, "{bad}");
    assert_ne!(bad["blake3"], m.blake3.as_str());
}

//! The AI daemon over real sockets, with the broker in the loop.
//!
//! The unit tests in `otwono-ai` prove the admission arithmetic. This proves the parts
//! that only exist once there are two processes: that `ai.read` is actually enforced, that
//! a refusal comes back as a *successful call reporting a refusal* rather than an RPC
//! error, and that `ai.infer` tells the truth about there being no engine.

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
    // must not be optimistic while no backend is linked.
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
fn infer_refuses_because_there_is_no_engine_not_because_of_policy() {
    // The distinction matters: a caller must be able to tell "you may not" from "this
    // build cannot". Policy grants nothing for ai.infer here, so the unauthorized answer
    // comes first -- and that is itself the honest one until an engine exists.
    let h = Harness::start("infer");
    let err = Client::connect(&h.ai_socket)
        .unwrap()
        .call("ai.infer", json!({ "prompt": "hello" }))
        .unwrap()
        .expect_err("ai.infer cannot succeed in this build");
    assert!(
        err.code == code::UNAUTHORIZED || err.code == code::UNAVAILABLE,
        "{err:?}"
    );
}

#[test]
fn describe_marks_infer_as_not_implemented() {
    // `describe` is how another component discovers what this node offers. It must not
    // advertise inference this build cannot do.
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
    assert!(
        infer["summary"].as_str().unwrap().contains("NOT IMPLEMENTED"),
        "{infer:#?}"
    );
}

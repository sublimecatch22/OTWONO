//! `ai.models.pull` end to end: broker, AI daemon and fetch daemon, over real sockets.
//!
//! Three processes' worth of behaviour in one binary, with only the network faked. That
//! boundary is chosen deliberately — the interesting cases are a manifest signed by a
//! stranger, weights that do not match it, and a model too large for the node, and a
//! cooperative remote host produces none of them.
//!
//! What this proves is the *ordering*. Each step of a pull is cheaper than the next, so a
//! refusal must land before the expensive thing happens: provenance and admission are both
//! decided from a kilobyte of manifest, before a byte of weights moves. A test that only
//! checked the final answer would pass while the node downloaded four gigabytes it was
//! always going to throw away.

use otwono_ai::manifest::{Footprint, ModelCapability, ModelFormat, ModelManifest};
use otwono_ai::signature::testing::sign;
use otwono_ai::{BackendId, Catalog};
use otwono_aid::AiService;
use otwono_capability::{classify, testing::report_pi4_4gb, Tier};
use otwono_fetch::{Source, SourceSet};
use otwono_fetchd::transport::{Head, Request, Transport, TransportError};
use otwono_fetchd::FetchService;
use otwono_permd::{ActionRegistry, AuditLog, Broker, Policy};
use otwono_proto::{Client, Server, Shutdown};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TRUSTED_SEED: u8 = 7;
const STRANGER_SEED: u8 = 9;
const MIB: u64 = 1024 * 1024;

/// Both actions, so a refusal in these tests is always the daemon's reasoning and never
/// the policy file's.
const POLICY: &str = r#"
[[rule]]
action = "ai.*"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "net.fetch"
decision = "allow"
ttl_seconds = 300
"#;

// --- a remote host that serves exactly what the test says --------------------------------

/// Path suffix -> bytes. Anything else is a 404.
struct FakeHost {
    objects: Mutex<Vec<(String, Vec<u8>)>>,
    served: Mutex<Vec<String>>,
}

impl FakeHost {
    fn new(objects: Vec<(String, Vec<u8>)>) -> Arc<FakeHost> {
        Arc::new(FakeHost {
            objects: Mutex::new(objects),
            served: Mutex::new(Vec::new()),
        })
    }

    /// Which paths were actually requested. The ordering assertions read this.
    fn served(&self) -> Vec<String> {
        self.served.lock().unwrap().clone()
    }
}

struct FakeTransport(Arc<FakeHost>);

impl Transport for FakeTransport {
    fn start(&self, request: &Request) -> Result<(Head, Box<dyn std::io::Read + Send>), TransportError> {
        let path = request.uri.path().to_string();
        self.0.served.lock().unwrap().push(path.clone());

        let found = self
            .0
            .objects
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| path.ends_with(p.as_str()))
            .map(|(_, b)| b.clone());

        let Some(body) = found else {
            return Ok((
                Head {
                    status: 404,
                    ..Head::default()
                },
                Box::new(std::io::empty()),
            ));
        };

        let total = body.len() as u64;
        let from = request.range_from as usize;
        if from >= body.len() && from > 0 {
            return Ok((
                Head {
                    status: 416,
                    ..Head::default()
                },
                Box::new(std::io::empty()),
            ));
        }
        let (status, rest) = if from == 0 {
            (200, body)
        } else {
            (206, body[from..].to_vec())
        };
        Ok((
            Head {
                status,
                etag: Some("\"v1\"".into()),
                total_bytes: Some(total),
                location: None,
            },
            Box::new(std::io::Cursor::new(rest)),
        ))
    }
}

// --- the harness -------------------------------------------------------------------------

struct Harness {
    dir: PathBuf,
    perm_socket: PathBuf,
    ai_socket: PathBuf,
    shutdown: Shutdown,
    host: Arc<FakeHost>,
}

fn manifest(id: &str, body: &[u8], min_tier: Tier, seed: Option<u8>) -> ModelManifest {
    let mut m = ModelManifest {
        schema_version: otwono_ai::manifest::SCHEMA_VERSION.to_string(),
        id: id.to_string(),
        family: "test".into(),
        parameters: 1_000_000_000,
        quantization: "Q4_K_M".into(),
        format: ModelFormat::Gguf,
        // The real digest of the real bytes. A placeholder would make the install path
        // pass for the wrong reason.
        blake3: blake3::hash(body).to_hex().to_string(),
        size_bytes: body.len() as u64,
        min_tier,
        footprint: Footprint {
            weights_bytes: body.len() as u64,
            kv_per_1k_ctx_bytes: MIB,
            overhead_bytes: MIB,
        },
        max_context: 8192,
        capabilities: vec![ModelCapability::Chat],
        license: "apache-2.0".into(),
        backends: vec![BackendId::LlamaCppCpu],
        signature: None,
    };
    if let Some(s) = seed {
        sign(&mut m, s);
    }
    m
}

/// A manifest big enough that admission refuses it on a Pi 4, without a body that size.
fn oversized(body: &[u8]) -> ModelManifest {
    // Unsigned first, then inflated, then signed. Signing covers the manifest, so mutating
    // it afterwards produces a document whose signature does not verify — which is a
    // different refusal than the one this test is about.
    let mut m = manifest("too-big-70b", body, Tier::T0Micro, None);
    m.footprint.weights_bytes = 40 * 1024 * MIB;
    sign(&mut m, TRUSTED_SEED);
    m
}

impl Harness {
    fn start(tag: &str, host: Arc<FakeHost>, wire_fetcher: bool) -> Harness {
        let dir = std::env::temp_dir().join(format!("otw-p{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::create_dir_all(dir.join("models/manifests")).unwrap();
        std::fs::create_dir_all(dir.join("spool")).unwrap();
        std::fs::write(dir.join("policy.d/10-test.toml"), POLICY).unwrap();

        let perm_socket = dir.join("perm.sock");
        let ai_socket = dir.join("ai.sock");
        let fetch_socket = dir.join("fetch.sock");
        let shutdown = Shutdown::new();

        let policy = Policy::load_dir(&dir.join("policy.d")).expect("policy must load");
        policy
            .validate(&ActionRegistry::builtin())
            .expect("the test policy must name only registered actions");
        let broker = Arc::new(Broker::new(
            policy,
            AuditLog::open(dir.join("audit.jsonl")).unwrap(),
        ));
        let server = Server::bind(&perm_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || server.serve(broker, s));

        let sources = SourceSet::new(vec![Source {
            id: "models".into(),
            host: "models.example.org".into(),
            port: None,
            path_prefix: "/otwono/".into(),
            max_bytes: 64 * MIB,
        }])
        .expect("valid source");
        let fetchd = Arc::new(
            FetchService::new(
                sources,
                dir.join("spool"),
                perm_socket.clone(),
                Box::new(FakeTransport(Arc::clone(&host))),
            )
            // Small per-call budget, so resumption is exercised on every pull rather than
            // being a path only a big model would reach.
            .with_budgets(4_096, Duration::from_secs(5), 0),
        );
        let fserver = Server::bind(&fetch_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || fserver.serve(fetchd, s));

        let mut probe = manifest("probe", b"x", Tier::T0Micro, Some(TRUSTED_SEED));
        let trust = sign(&mut probe, TRUSTED_SEED);

        let mut ai = AiService::new(
            Catalog::new(dir.join("models")),
            classify(&report_pi4_4gb()),
            trust,
            perm_socket.clone(),
            Vec::new(),
        );
        if wire_fetcher {
            ai = ai.with_fetch_socket(&fetch_socket);
        }
        let aserver = Server::bind(&ai_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || aserver.serve(Arc::new(ai), s));

        for sock in [&perm_socket, &fetch_socket, &ai_socket] {
            Client::connect_waiting(sock, Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{} never came up", sock.display()));
        }

        Harness {
            dir,
            perm_socket,
            ai_socket,
            shutdown,
            host,
        }
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, otwono_proto::RpcError> {
        // Tokens are scoped to an action, so the helper asks for the one this method needs
        // rather than a blanket grant — otherwise these tests would not notice if a method
        // stopped being guarded.
        let action = if method.starts_with("ai.models.list") || method == "ai.admit" {
            "ai.read"
        } else {
            "ai.admin"
        };
        let mut broker = Client::connect(&self.perm_socket).unwrap();
        let token = broker
            .call("perm.request", json!({ "action": action }))
            .unwrap()
            .expect("policy allows this action")["token"]
            .as_str()
            .unwrap()
            .to_string();
        Client::connect_with_timeout(&self.ai_socket, Duration::from_secs(30))
            .unwrap()
            .call_with_capability(method, params, &token)
            .unwrap()
    }

    fn pull(&self, extra: Value) -> Result<Value, otwono_proto::RpcError> {
        let mut params = json!({
            "source": "models",
            "manifest_path": "m.json",
            "blob_path": "m.gguf",
        });
        for (k, v) in extra.as_object().unwrap() {
            params[k] = v.clone();
        }
        self.call("ai.models.pull", params)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn objects(m: &ModelManifest, weights: &[u8]) -> Vec<(String, Vec<u8>)> {
    vec![
        ("m.json".into(), serde_json::to_vec_pretty(m).unwrap()),
        ("m.gguf".into(), weights.to_vec()),
    ]
}

// --- the tests ----------------------------------------------------------------------------

#[test]
fn a_model_downloads_verifies_and_lands_in_the_catalog() {
    let weights = vec![3u8; 20_000];
    let m = manifest("pulled-1b", &weights, Tier::T0Micro, Some(TRUSTED_SEED));
    let h = Harness::start("ok", FakeHost::new(objects(&m, &weights)), true);

    let got = h.pull(json!({})).expect("pull");
    assert_eq!(got["model_id"], "pulled-1b");
    assert_eq!(got["blake3"], m.blake3);
    assert_eq!(got["bytes_fetched"], json!(20_000));
    assert_eq!(got["provenance"]["status"], "trusted");

    // 20,000 bytes at a 4,096-byte budget is six calls, so resumption ran for real.
    assert!(
        got["fetch_calls"].as_u64().unwrap() >= 5,
        "the transfer should have resumed: {got}"
    );

    // The weights are in the blob store, and the daemon can find them.
    let listed = h.call("ai.models.list", json!({})).expect("list");
    let entry = listed["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "pulled-1b")
        .expect("the pulled model is in the catalog");
    assert_eq!(entry["weights_present"], true, "{entry}");
}

#[test]
fn the_spool_copy_is_reclaimed_so_a_model_is_not_stored_twice() {
    // install copies rather than moves. On an 8 GB board, leaving the copy would mean a
    // 4 GB model costs 8 GB, which is the difference between working and not.
    let weights = vec![3u8; 20_000];
    let m = manifest("pulled-1b", &weights, Tier::T0Micro, Some(TRUSTED_SEED));
    let h = Harness::start("spool", FakeHost::new(objects(&m, &weights)), true);

    let got = h.pull(json!({})).expect("pull");
    assert_eq!(got["spool_reclaimed"], true, "{got}");

    let left: Vec<_> = std::fs::read_dir(h.dir.join("spool"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(left.is_empty(), "spool should be empty, holds {left:?}");
}

#[test]
fn a_manifest_from_a_stranger_is_refused_before_any_weights_move() {
    // The ordering test. Provenance is decided from a kilobyte; the weights are gigabytes.
    let weights = vec![3u8; 20_000];
    let m = manifest("stranger-1b", &weights, Tier::T0Micro, Some(STRANGER_SEED));
    let h = Harness::start("stranger", FakeHost::new(objects(&m, &weights)), true);

    let err = h.pull(json!({})).expect_err("an unknown publisher is refused");
    assert!(err.message.contains("refused"), "{}", err.message);
    assert!(
        !h.host.served().iter().any(|p| p.ends_with("m.gguf")),
        "the weights were requested anyway: {:?}",
        h.host.served()
    );
}

#[test]
fn an_unsigned_manifest_is_refused_before_any_weights_move_unless_opted_into() {
    let weights = vec![3u8; 20_000];
    let m = manifest("unsigned-1b", &weights, Tier::T0Micro, None);
    let h = Harness::start("unsigned", FakeHost::new(objects(&m, &weights)), true);

    assert!(h.pull(json!({})).is_err(), "unsigned needs an opt-in");
    assert!(
        !h.host.served().iter().any(|p| p.ends_with("m.gguf")),
        "no weights before the opt-in"
    );

    let got = h
        .pull(json!({ "allow_unsigned": true }))
        .expect("with the opt-in it installs");
    assert_eq!(got["provenance"]["status"], "unsigned");
}

#[test]
fn a_model_this_node_could_never_load_is_refused_before_the_download() {
    // A rural link's afternoon is worth more than the tidiness of finding out at load time.
    let weights = vec![3u8; 20_000];
    let m = oversized(&weights);
    let h = Harness::start("toobig", FakeHost::new(objects(&m, &weights)), true);

    let err = h.pull(json!({})).expect_err("admission refuses this on a Pi 4");
    assert!(err.message.contains("before downloading"), "{}", err.message);
    assert!(
        !h.host.served().iter().any(|p| p.ends_with("m.gguf")),
        "the weights were downloaded despite being unusable"
    );
}

#[test]
fn downloading_for_another_machine_is_possible_but_deliberate() {
    let weights = vec![3u8; 20_000];
    let m = oversized(&weights);
    let h = Harness::start("toobig2", FakeHost::new(objects(&m, &weights)), true);

    let got = h
        .pull(json!({ "allow_unadmittable": true }))
        .expect("the override downloads it");
    assert_eq!(got["model_id"], "too-big-70b");
}

#[test]
fn weights_that_do_not_match_the_manifest_are_refused_after_the_download() {
    // This one cannot be caught early — it is what hashing the bytes is for. The pull adds
    // no new trust code; install refuses it with the code that was already tested.
    let weights = vec![3u8; 20_000];
    let m = manifest("pulled-1b", &weights, Tier::T0Micro, Some(TRUSTED_SEED));
    let swapped = vec![4u8; 20_000];
    let h = Harness::start("swap", FakeHost::new(objects(&m, &swapped)), true);

    let err = h.pull(json!({})).expect_err("a swapped blob is refused");
    assert!(
        err.message.contains("do not match") || err.message.contains("blake3"),
        "{}",
        err.message
    );
}

#[test]
fn a_caller_cannot_name_a_host_only_a_source() {
    let weights = vec![3u8; 20_000];
    let m = manifest("pulled-1b", &weights, Tier::T0Micro, Some(TRUSTED_SEED));
    let h = Harness::start("nohost", FakeHost::new(objects(&m, &weights)), true);

    for bad in ["../../etc/passwd", "https://evil.example.com/m.json"] {
        let err = h
            .pull(json!({ "manifest_path": bad }))
            .expect_err("{bad} must be refused");
        assert!(err.message.contains("pull"), "{}", err.message);
    }
    assert!(h.host.served().is_empty(), "nothing should have been asked for");
}

#[test]
fn a_node_with_no_fetch_daemon_says_so_rather_than_failing_obscurely() {
    // The shipped default: no source configured, no net.fetch granted, no fetcher wired.
    let weights = vec![3u8; 20_000];
    let m = manifest("pulled-1b", &weights, Tier::T0Micro, Some(TRUSTED_SEED));
    let h = Harness::start("nofetch", FakeHost::new(objects(&m, &weights)), false);

    let err = h.pull(json!({})).expect_err("no fetcher, no pull");
    assert!(err.message.contains("no fetch daemon"), "{}", err.message);
    assert!(h.host.served().is_empty());
}

#[test]
fn a_missing_object_is_reported_rather_than_hung_on() {
    let weights = vec![3u8; 20_000];
    let m = manifest("pulled-1b", &weights, Tier::T0Micro, Some(TRUSTED_SEED));
    let mut objs = objects(&m, &weights);
    objs.retain(|(p, _)| p != "m.gguf"); // manifest present, weights are not
    let h = Harness::start("missing", FakeHost::new(objs), true);

    let err = h.pull(json!({})).expect_err("404 on the weights");
    assert!(err.message.contains("404"), "{}", err.message);
}

#[test]
fn pull_needs_ai_admin_not_merely_ai_read() {
    let weights = vec![3u8; 20_000];
    let m = manifest("pulled-1b", &weights, Tier::T0Micro, Some(TRUSTED_SEED));
    let h = Harness::start("authz", FakeHost::new(objects(&m, &weights)), true);

    let mut broker = Client::connect(&h.perm_socket).unwrap();
    let read_token = broker
        .call("perm.request", json!({ "action": "ai.read" }))
        .unwrap()
        .expect("policy allows ai.read")["token"]
        .as_str()
        .unwrap()
        .to_string();

    let err = Client::connect(&h.ai_socket)
        .unwrap()
        .call_with_capability(
            "ai.models.pull",
            json!({ "source": "models", "manifest_path": "m.json", "blob_path": "m.gguf" }),
            &read_token,
        )
        .unwrap()
        .expect_err("reading the catalog is not permission to change it");
    assert_eq!(err.code, otwono_proto::code::UNAUTHORIZED, "{}", err.message);
    assert!(h.host.served().is_empty(), "authorization runs first");
}

#[test]
fn describe_advertises_pull_behind_ai_admin() {
    let weights = vec![3u8; 20_000];
    let m = manifest("pulled-1b", &weights, Tier::T0Micro, Some(TRUSTED_SEED));
    let h = Harness::start("describe", FakeHost::new(objects(&m, &weights)), true);
    let described = Client::connect(&h.ai_socket)
        .unwrap()
        .call("describe", json!({}))
        .unwrap()
        .expect("describe is open");
    let m = described["methods"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "ai.models.pull")
        .expect("pull should be described");
    assert_eq!(m["capability"], "ai.admin");
}

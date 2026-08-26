//! The cluster cache over the real control plane, and over a real link.
//!
//! Three claims, each of which is only worth anything if it is checked at the boundary
//! rather than in the function that implements it:
//!
//! 1. **`PRIVATE` cannot enter the cache by any path**, including a caller that lies about
//!    the label and a label this build has never heard of.
//! 2. **A cached object is servable to the next peer**, which is the entire point of
//!    ADR-0015 — one node fetches from origin and the street gets it from that node.
//! 3. **`otwono-netd` cannot write the user's own store.** It holds `cache.write` and not
//!    `store.write`, and the policy here denies `store.write` outright to prove it.

use otwono_permd::{ActionRegistry, AuditLog, Broker, Policy};
use otwono_proto::{code, Client, Server, Shutdown};
use otwono_store::{Cache, StorageKey, Store};
use otwono_stored::StoreService;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Everything the cache needs, and deliberately **not** `store.write`: the daemon under
/// test must be able to fill the cache without being able to write the user's store.
const POLICY: &str = r#"
[[rule]]
action = "cache.read"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "cache.write"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "store.serve"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "store.read"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "store.write"
decision = "deny"
"#;

/// A separate harness whose policy *does* allow `store.write`, for the tests that need to
/// put something in the user's own store to compare against.
const POLICY_WITH_WRITE: &str = r#"
[[rule]]
action = "cache.*"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "store.*"
decision = "allow"
ttl_seconds = 300
"#;

struct Harness {
    dir: PathBuf,
    perm_socket: PathBuf,
    store_socket: PathBuf,
    shutdown: Shutdown,
}

impl Harness {
    fn start(tag: &str, policy_toml: &str, budget: u64) -> Harness {
        let dir = std::env::temp_dir().join(format!("otw-nc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::write(dir.join("policy.d/10-test.toml"), policy_toml).unwrap();

        let perm_socket = dir.join("perm.sock");
        let store_socket = dir.join("store.sock");
        let shutdown = Shutdown::new();

        let policy = Policy::load_dir(&dir.join("policy.d")).expect("policy must load");
        policy
            .validate(&ActionRegistry::builtin())
            .expect("the test policy must name only registered actions");
        let broker = Arc::new(Broker::new(
            policy,
            AuditLog::open(dir.join("audit.jsonl")).unwrap(),
        ));
        let s = shutdown.clone();
        let server = Server::bind(&perm_socket).unwrap();
        std::thread::spawn(move || server.serve(broker, s));

        // Store and cache are separate directories under one daemon, as on a real node.
        let (key, _) = StorageKey::load_or_generate(&dir.join("storage.key")).unwrap();
        let store = Store::encrypted(dir.join("store"), key);
        store.ensure_layout().unwrap();
        let (cache_key, _) = StorageKey::load_or_generate(&dir.join("storage.key")).unwrap();
        let cache = Cache::at(dir.join("cache"), cache_key, budget).unwrap();
        let service = Arc::new(StoreService::new(store, perm_socket.clone()).with_cache(cache));
        let s = shutdown.clone();
        let server = Server::bind(&store_socket).unwrap();
        std::thread::spawn(move || server.serve(service, s));

        for sock in [&perm_socket, &store_socket] {
            Client::connect_waiting(sock, Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{} never came up", sock.display()));
        }
        Harness {
            dir,
            perm_socket,
            store_socket,
            shutdown,
        }
    }

    fn token(&self, action: &str) -> String {
        let mut broker = Client::connect(&self.perm_socket).unwrap();
        broker
            .call(
                "perm.request",
                json!({ "action": action, "reason": "cluster cache test" }),
            )
            .unwrap()
            .unwrap_or_else(|e| panic!("{action} refused: {}", e.message))
            .get("token")
            .and_then(Value::as_str)
            .unwrap()
            .to_string()
    }

    fn call(&self, method: &str, params: Value, action: &str) -> Result<Value, otwono_proto::RpcError> {
        let token = self.token(action);
        let mut client = Client::connect(&self.store_socket).unwrap();
        client.call_with_capability(method, params, &token).unwrap()
    }

    /// A raw call with a token for a *different* action, to check the guard.
    fn call_unauthorized(&self, method: &str, params: Value) -> otwono_proto::RpcError {
        let mut client = Client::connect(&self.store_socket).unwrap();
        client.call(method, params).unwrap().expect_err("must be refused")
    }

    fn cache_put(&self, bytes: &[u8], visibility: &str) -> Result<Value, otwono_proto::RpcError> {
        self.call(
            "cache.put",
            json!({
                "data": data_encoding::BASE64.encode(bytes),
                "visibility": visibility,
            }),
            "cache.write",
        )
    }

    fn status(&self) -> Value {
        self.call("cache.status", json!({}), "cache.read").unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

#[test]
fn public_content_fetched_from_a_peer_can_be_cached_and_read_back() {
    let h = Harness::start("roundtrip", POLICY, 4 * 1024 * 1024);
    let bytes = payload(300 * 1024, 1);
    let put = h
        .cache_put(&bytes, "public")
        .expect("public content is cacheable");
    let id = put["content_id"].as_str().unwrap().to_string();
    assert_eq!(put["cached"], true);

    let got = h
        .call("cache.get", json!({ "content_id": id }), "cache.read")
        .unwrap();
    assert_eq!(
        data_encoding::BASE64
            .decode(got["data"].as_str().unwrap().as_bytes())
            .unwrap(),
        bytes
    );
}

#[test]
fn a_cached_object_is_servable_to_the_next_peer() {
    // ADR-0015's whole point: one node fetches from origin, and the street gets it from
    // that node. Without this, the cache is a private download folder.
    let h = Harness::start("serve", POLICY, 4 * 1024 * 1024);
    let bytes = payload(300 * 1024, 2);
    let id = h.cache_put(&bytes, "replicated").unwrap()["content_id"]
        .as_str()
        .unwrap()
        .to_string();

    let manifest = h
        .call(
            "store.serve_manifest",
            json!({ "content_id": id, "max_chunks": 64 }),
            "store.serve",
        )
        .expect("a cached object must be servable");
    assert_eq!(manifest["visibility"], "replicated");
    let total = manifest["total_chunks"].as_u64().unwrap();
    assert!(total > 1, "the fixture must span several chunks");

    // And the chunks themselves come back, from the cache, byte for byte.
    let mut assembled = Vec::new();
    for entry in manifest["chunks"].as_array().unwrap() {
        let digest = entry["blake3"].as_str().unwrap();
        let part = h
            .call(
                "store.serve_chunk",
                json!({ "content_id": id, "digest": digest, "max_bytes": 262144 }),
                "store.serve",
            )
            .expect("a cached chunk must be servable");
        assembled.extend(
            data_encoding::BASE64
                .decode(part["data"].as_str().unwrap().as_bytes())
                .unwrap(),
        );
    }
    assert_eq!(assembled, bytes);
}

#[test]
fn private_content_cannot_be_cached_however_it_is_labelled() {
    // A caller that says `private`, a caller that says `shared`, and a caller that invents a
    // label this build has never seen. The third is the one that matters: an unparseable
    // label is `Private`, so a future label cannot sneak in as permissive.
    let h = Harness::start("private", POLICY, 4 * 1024 * 1024);
    for label in ["private", "shared", "world-readable", "", "publik", "null"] {
        let err = h.cache_put(b"the user's private notes", label).unwrap_err();
        assert_eq!(err.code, code::INVALID_PARAMS, "label {label:?}: {}", err.message);
        assert!(
            !err.message.contains("private notes"),
            "the refusal echoed the content: {}",
            err.message
        );
    }
    assert_eq!(h.status()["objects"], 0, "something got in");
    assert_eq!(h.status()["used_bytes"], 0);
}

#[test]
fn label_parsing_is_case_insensitive_but_never_generous() {
    // Two halves of one contract, worth pinning together because getting either wrong is
    // silent. `Visibility::parse` folds case and trims, so " PUBLIC " is public — a config
    // file a person typed should not fail on a capital letter. But anything it does not
    // recognise is `Private`, so a label from a future version, or a typo, is refused rather
    // than treated as the nearest permissive thing.
    //
    // Note the asymmetry with the wire: `otwono_netd::content::may_leave_a_node` accepts
    // only exact lowercase, because bytes from a stranger get no such benefit of the doubt.
    let h = Harness::start("case", POLICY, 4 * 1024 * 1024);
    for spelled in ["PUBLIC", " public ", "Public", "REPLICATED"] {
        h.cache_put(format!("bytes for {spelled}").as_bytes(), spelled)
            .unwrap_or_else(|e| panic!("{spelled:?} should parse as cacheable: {}", e.message));
    }
    for spelled in ["PRIVATE", " Shared ", "pubic", "public-ish"] {
        h.cache_put(format!("bytes for {spelled}").as_bytes(), spelled)
            .expect_err(&format!("{spelled:?} must not be cacheable"));
    }
}

#[test]
fn the_cache_daemon_cannot_write_the_users_own_store() {
    // The reason cache.write is its own capability rather than store.write. Under this
    // policy the caching path works and store.put is refused at the broker.
    let h = Harness::start("separation", POLICY, 4 * 1024 * 1024);
    h.cache_put(b"cacheable", "public").expect("caching must work");

    let mut broker = Client::connect(&h.perm_socket).unwrap();
    let refused = broker
        .call(
            "perm.request",
            json!({ "action": "store.write", "reason": "prove the policy denies it" }),
        )
        .unwrap();
    assert!(refused.is_err(), "the test policy must deny store.write");
}

#[test]
fn every_cache_method_is_guarded() {
    let h = Harness::start("guards", POLICY, 4 * 1024 * 1024);
    for (method, params) in [
        ("cache.put", json!({ "data": "", "visibility": "public" })),
        ("cache.get", json!({ "content_id": "0".repeat(64) })),
        ("cache.status", json!({})),
        (
            "cache.pin",
            json!({ "content_id": "0".repeat(64), "pinned": true }),
        ),
        ("cache.purge", json!({})),
    ] {
        let err = h.call_unauthorized(method, params);
        assert_eq!(err.code, code::UNAUTHORIZED, "{method} was not guarded");
    }
}

#[test]
fn the_status_call_always_says_that_holding_is_publishing() {
    // ADR-0015 requires the operator be told. A UI that has to remember to say it is a UI
    // that will forget, so the daemon says it on every call.
    let h = Harness::start("note", POLICY, 4 * 1024 * 1024);
    let note = h.status()["note"].as_str().unwrap().to_string();
    assert!(note.contains("holding is publishing"), "{note}");
}

#[test]
fn the_cache_evicts_over_the_control_plane_and_reports_what_it_dropped() {
    let budget = 256 * 1024;
    let h = Harness::start("evict", POLICY, budget);
    let mut evicted_at_least_once = false;
    for i in 0..10u64 {
        let reply = h.cache_put(&payload(64 * 1024, i + 1), "public").unwrap();
        if reply["evicted_bytes"].as_u64().unwrap() > 0 {
            evicted_at_least_once = true;
        }
        assert!(
            reply["cache_used_bytes"].as_u64().unwrap() <= budget,
            "over budget after {i} inserts: {reply}"
        );
    }
    assert!(evicted_at_least_once, "nothing was ever evicted in a full cache");
}

#[test]
fn a_purge_empties_the_cache_and_leaves_the_users_store_alone() {
    let h = Harness::start("purge", POLICY_WITH_WRITE, 4 * 1024 * 1024);
    let mine = h
        .call(
            "store.put",
            json!({ "data": data_encoding::BASE64.encode(b"the user's own file"), "visibility": "public" }),
            "store.write",
        )
        .unwrap()["content_id"]
        .as_str()
        .unwrap()
        .to_string();
    h.cache_put(b"a neighbour's bytes", "public").unwrap();
    assert_eq!(h.status()["objects"], 1);

    let purged = h.call("cache.purge", json!({}), "cache.write").unwrap();
    assert!(purged["freed_bytes"].as_u64().unwrap() > 0);
    assert_eq!(h.status()["objects"], 0);

    // The user's own object is untouched, and still servable.
    h.call("store.get", json!({ "content_id": mine }), "store.read")
        .expect("a purge must not reach the user's store");
}

#[test]
fn the_users_own_copy_wins_over_a_cached_one_of_the_same_bytes() {
    // Both stores can hold the same content id. Serving must resolve deterministically, and
    // the user's own copy is the one that cannot be evicted underneath a transfer.
    let h = Harness::start("both", POLICY_WITH_WRITE, 4 * 1024 * 1024);
    let bytes = payload(200 * 1024, 5);
    let own = h
        .call(
            "store.put",
            json!({ "data": data_encoding::BASE64.encode(&bytes), "visibility": "public" }),
            "store.write",
        )
        .unwrap();
    let cached = h.cache_put(&bytes, "public").unwrap();
    assert_eq!(own["content_id"], cached["content_id"]);

    let id = own["content_id"].as_str().unwrap().to_string();
    let served = h
        .call("store.serve", json!({ "content_id": id }), "store.serve")
        .unwrap();
    assert_eq!(
        data_encoding::BASE64
            .decode(served["data"].as_str().unwrap().as_bytes())
            .unwrap(),
        bytes
    );

    // Purging the cache must not affect what is served, because the user's copy is what
    // answered.
    h.call("cache.purge", json!({}), "cache.write").unwrap();
    h.call("store.serve", json!({ "content_id": id }), "store.serve")
        .expect("the user's own copy still serves after a purge");
}

#[test]
fn serving_a_cached_object_to_a_peer_does_not_keep_it_alive() {
    // A peer must not be able to steer this node's eviction policy by asking repeatedly.
    let h = Harness::start("noreprieve", POLICY, 2 * 64 * 1024);
    let old = h.cache_put(&payload(64 * 1024, 1), "public").unwrap()["content_id"]
        .as_str()
        .unwrap()
        .to_string();
    h.cache_put(&payload(64 * 1024, 2), "public").unwrap();

    // Ask about the older one as a peer would, repeatedly, then insert something new.
    for _ in 0..5 {
        h.call(
            "store.serve_manifest",
            json!({ "content_id": old, "max_chunks": 8 }),
            "store.serve",
        )
        .unwrap();
    }
    h.cache_put(&payload(64 * 1024, 3), "public").unwrap();

    let held: Vec<String> = h.status()["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["content_id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !held.contains(&old),
        "a peer kept an object alive in this node's cache by asking about it"
    );
}

#[test]
fn a_node_with_no_cache_answers_plainly_rather_than_pretending() {
    let dir = std::env::temp_dir().join(format!("otw-nc-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("policy.d")).unwrap();
    std::fs::write(dir.join("policy.d/10-test.toml"), POLICY).unwrap();
    let perm_socket = dir.join("perm.sock");
    let store_socket = dir.join("store.sock");
    let shutdown = Shutdown::new();

    let policy = Policy::load_dir(&dir.join("policy.d")).unwrap();
    let broker = Arc::new(Broker::new(
        policy,
        AuditLog::open(dir.join("audit.jsonl")).unwrap(),
    ));
    let s = shutdown.clone();
    let server = Server::bind(&perm_socket).unwrap();
    std::thread::spawn(move || server.serve(broker, s));

    let (key, _) = StorageKey::load_or_generate(&dir.join("storage.key")).unwrap();
    let store = Store::encrypted(dir.join("store"), key);
    store.ensure_layout().unwrap();
    // No `.with_cache`: what a storage-constrained machine's daemon looks like.
    let service = Arc::new(StoreService::new(store, perm_socket.clone()));
    let s = shutdown.clone();
    let server = Server::bind(&store_socket).unwrap();
    std::thread::spawn(move || server.serve(service, s));
    Client::connect_waiting(&store_socket, Duration::from_secs(5)).unwrap();

    let mut broker = Client::connect(&perm_socket).unwrap();
    let token = broker
        .call(
            "perm.request",
            json!({ "action": "cache.read", "reason": "test" }),
        )
        .unwrap()
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut client = Client::connect(&store_socket).unwrap();
    let err = client
        .call_with_capability("cache.status", json!({}), &token)
        .unwrap()
        .expect_err("a node with no cache must say so");
    assert_eq!(err.code, code::UNAVAILABLE);
    assert!(err.message.contains("no cluster cache"), "{}", err.message);

    shutdown.trigger();
    let _ = std::fs::remove_dir_all(&dir);
}

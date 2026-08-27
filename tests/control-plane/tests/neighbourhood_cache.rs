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
action = "cache.replicate"
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

    fn replica_room(&self, candidates: &[&str]) -> Value {
        self.call(
            "cache.replica_room",
            json!({ "candidates": candidates }),
            "cache.replicate",
        )
        .unwrap()
    }

    fn take_replica(
        &self,
        bytes: &[u8],
        ttl_days: u32,
        max_size_bytes: u64,
    ) -> Result<Value, otwono_proto::RpcError> {
        self.call(
            "cache.take_replica",
            json!({
                "data": data_encoding::BASE64.encode(bytes),
                "ttl_days": ttl_days,
                "max_size_bytes": max_size_bytes,
                "allow_rereplication": true,
            }),
            "cache.replicate",
        )
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
    // The list comes from `describe`, not from this test. It used to be written out here,
    // and a hand-maintained list is one somebody forgets to extend — the failure mode being
    // an unguarded method that this test reports as fine because it never asked about it.
    //
    // Empty params are enough: every arm authorizes before it parses, so a method that got
    // as far as complaining about its parameters has already failed the thing under test.
    let h = Harness::start("guards", POLICY, 4 * 1024 * 1024);
    let described = {
        let mut client = Client::connect(&h.store_socket).unwrap();
        client
            .describe()
            .unwrap()
            .expect("describe must not need a token")
    };
    let cache_methods: Vec<_> = described
        .methods
        .iter()
        .filter(|m| m.name.starts_with("cache."))
        .collect();
    let methods: Vec<String> = cache_methods.iter().map(|m| m.name.clone()).collect();

    // Declared and enforced are two different claims, and both are worth checking: a method
    // described as open would be a documented hole, and one described as guarded but
    // dispatched without the check would be an undocumented one.
    for m in &cache_methods {
        assert!(
            m.capability.is_some(),
            "{} is described as needing no capability",
            m.name
        );
    }

    // A guard test that found nothing to guard would pass silently.
    assert!(
        methods.len() >= 7,
        "describe named only {} cache methods: {methods:?}",
        methods.len()
    );
    for known in ["cache.put", "cache.replica_room", "cache.take_replica"] {
        assert!(
            methods.iter().any(|m| m == known),
            "{known} is missing from describe: {methods:?}"
        );
    }

    for method in &methods {
        let err = h.call_unauthorized(method, json!({}));
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

/// `cache.replicate` is its own capability, and `cache.write` is not a substitute (ADR-0026 §10).
///
/// The distinction is the whole reason the capability exists: "keep what I fetched" and
/// "keep what a stranger offered" are different decisions, and an operator who grants the
/// first has not granted the second. A guard that accepted either would make the separation
/// documentation rather than enforcement.
#[test]
fn holding_a_replica_needs_more_than_permission_to_cache() {
    let h = Harness::start("repl-guard", POLICY, 8 << 20);
    let bytes = payload(4096, 21);

    // The right capability works.
    let ok = h
        .take_replica(&bytes, 30, 1 << 20)
        .expect("cache.replicate is allowed here");
    assert_eq!(ok["taken"], json!(true));

    // A cache.write token does not.
    let token = h.token("cache.write");
    let mut client = Client::connect(&h.store_socket).unwrap();
    let err = client
        .call_with_capability(
            "cache.take_replica",
            json!({
                "data": data_encoding::BASE64.encode(&payload(4096, 22)),
                "ttl_days": 30,
                "max_size_bytes": 1 << 20,
                "allow_rereplication": true,
            }),
            &token,
        )
        .unwrap()
        .expect_err("a cache.write token must not buy a replica");
    assert_eq!(err.code, code::UNAUTHORIZED, "{}", err.message);

    // And no token at all does not either.
    let err = h.call_unauthorized("cache.replica_room", json!({ "candidates": [] }));
    assert_eq!(err.code, code::UNAUTHORIZED, "{}", err.message);
}

/// A holder answers about what it was asked about, and takes what it agreed to hold.
#[test]
fn a_holder_answers_about_the_offer_and_then_holds_it() {
    let h = Harness::start("repl-room", POLICY, 8 << 20);
    let bytes = payload(4096, 31);
    let absent = "11".repeat(32);

    let before = h.replica_room(&[&absent]);
    assert_eq!(before["replicating"], json!(true));
    assert!(before["room_bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        before["already_held"],
        json!([]),
        "claimed to hold something it has never seen"
    );

    let taken = h.take_replica(&bytes, 30, 1 << 20).expect("take");
    assert_eq!(taken["taken"], json!(true));
    let id = taken["content_id"].as_str().unwrap().to_string();
    assert_eq!(taken["size_bytes"], json!(bytes.len() as u64));

    let after = h.replica_room(&[&id, &absent]);
    assert_eq!(
        after["already_held"],
        json!([id]),
        "an object just taken was not reported as held"
    );
    // The reply is bounded by the question: asking about two ids answers about those two,
    // and never turns into a listing of the cache. That listing is `cache.status`, and it
    // needs `cache.read`, which a replicating peer has no reason to hold.
    assert!(after.get("entries").is_none());

    // And an operator can see the hold. A subsystem that stores a promise the operator
    // cannot see is one they cannot reason about, which is the objection ADR-0015 raised
    // about eviction and applies here for the same reason.
    let status = h.status();
    assert_eq!(status["replicas_held"], json!(1));
    let entry = status["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["content_id"] == json!(id))
        .expect("the replica is in the cache");
    assert_eq!(entry["size_bytes"], json!(bytes.len() as u64));
    assert_eq!(entry["holds_a_replica"], json!(true));
    assert!(
        entry["replica_expires_ms"].as_u64().is_some(),
        "a held replica with no expiry is a promise with no end"
    );
    // A hold is not a pin: they are separate commitments and the status must not conflate
    // them, or a TTL sweep would look like it had unpinned something a person chose to keep.
    assert_eq!(entry["pinned"], json!(false));
}

/// A refusal is `taken: false`, not an error — and the caller must be able to tell the two
/// apart, because one means "try another object" and the other means "something is broken".
#[test]
fn an_object_that_will_not_fit_is_refused_rather_than_failed() {
    let h = Harness::start("repl-full", POLICY, 4096);
    // Larger than the whole budget.
    let outcome = h
        .take_replica(&payload(64 * 1024, 41), 30, 1 << 20)
        .expect("a refusal is not an error");
    assert_eq!(outcome["taken"], json!(false));
    assert_eq!(outcome["cache_used_bytes"], json!(0));
    assert!(outcome.get("content_id").is_none());

    // The owner's own size cap refuses independently of this node's budget: an object that
    // fits here but that its owner said not to replicate above a smaller size is still
    // refused, because the policy is the owner's to set.
    let small = payload(2048, 42);
    let outcome = h
        .take_replica(&small, 30, 1024)
        .expect("a refusal is not an error");
    assert_eq!(
        outcome["taken"],
        json!(false),
        "took an object over the owner's own max_size_bytes"
    );
}

/// A policy with a zero in it is refused rather than normalised.
///
/// A zero TTL is a promise that has already expired and a zero size cap permits nothing;
/// either is a peer offering something no holder can honour, and silently substituting a
/// default would mean inventing terms on the owner's behalf.
#[test]
fn a_policy_that_promises_nothing_is_refused() {
    let h = Harness::start("repl-zero", POLICY, 8 << 20);
    let bytes = payload(2048, 51);

    let err = h
        .take_replica(&bytes, 0, 1 << 20)
        .expect_err("a zero TTL must be refused");
    assert_eq!(err.code, code::INVALID_PARAMS, "{}", err.message);

    let err = h
        .take_replica(&bytes, 30, 0)
        .expect_err("a zero size cap must be refused");
    assert_eq!(err.code, code::INVALID_PARAMS, "{}", err.message);
}

/// A node with no cache says "I do not replicate", rather than failing.
///
/// The distinction matters to the pass: a refusal that reads as an error would invite a
/// retry, and there is nothing to retry — this node is not going to hold anything. It is
/// also why `cache.replica_room` answers here while `cache.take_replica` does not: the first
/// is a question this node can answer truthfully, and the second is an action it cannot take.
#[test]
fn a_node_with_no_cache_answers_that_it_does_not_replicate() {
    let dir = std::env::temp_dir().join(format!("otw-nc-nocache-repl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("policy.d")).unwrap();
    std::fs::write(dir.join("policy.d/10-test.toml"), POLICY_WITH_WRITE).unwrap();
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
    // No `.with_cache(..)`: this node contributes nothing.
    let service = Arc::new(StoreService::new(store, perm_socket.clone()));
    let s = shutdown.clone();
    let server = Server::bind(&store_socket).unwrap();
    std::thread::spawn(move || server.serve(service, s));
    for sock in [&perm_socket, &store_socket] {
        Client::connect_waiting(sock, Duration::from_secs(5)).unwrap();
    }

    let token = {
        let mut broker = Client::connect(&perm_socket).unwrap();
        broker
            .call(
                "perm.request",
                json!({ "action": "cache.replicate", "reason": "test" }),
            )
            .unwrap()
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let mut client = Client::connect(&store_socket).unwrap();
    let room = client
        .call_with_capability("cache.replica_room", json!({ "candidates": [] }), &token)
        .unwrap()
        .expect("asking is not an error on a node with no cache");
    assert_eq!(room["replicating"], json!(false));
    assert_eq!(room["room_bytes"], json!(0));

    let err = client
        .call_with_capability(
            "cache.take_replica",
            json!({ "data": "", "ttl_days": 30, "max_size_bytes": 1024 }),
            &token,
        )
        .unwrap()
        .expect_err("there is nowhere to put it");
    assert!(err.message.contains("no cluster cache"), "{}", err.message);

    shutdown.trigger();
    let _ = std::fs::remove_dir_all(&dir);
}

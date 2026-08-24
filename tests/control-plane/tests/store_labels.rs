//! `otwono-stored` over the real control plane, with the label check under a microscope.
//!
//! `DATA-VISIBILITY.md` §6 says the negative tests are the point of this subsystem, so most
//! of what follows asserts that something does *not* happen. The two properties that matter:
//!
//! 1. A `Private` object must never leave the node, under any code path.
//! 2. A refusal must not be a disclosure — "you may not have this" and "this is not here"
//!    have to be indistinguishable, or a peer learns what this node holds by asking.
//!
//! `store.get` and `store.serve` return the same bytes for a public object and are separate
//! methods with separate capabilities, so a network daemon can serve peers without being
//! able to read the user's private notes. That separation is asserted here too.

use otwono_permd::{ActionRegistry, AuditLog, Broker, Policy};
use otwono_proto::{code, Client, Server, Shutdown};
use otwono_store::{StorageKey, Store};
use otwono_stored::StoreService;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const POLICY: &str = r#"
[[rule]]
action = "store.*"
decision = "allow"
ttl_seconds = 300
"#;

struct Harness {
    dir: PathBuf,
    perm_socket: PathBuf,
    store_socket: PathBuf,
    audit_log: PathBuf,
    shutdown: Shutdown,
}

impl Harness {
    fn start(tag: &str) -> Harness {
        let dir = std::env::temp_dir().join(format!("otw-sl{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::write(dir.join("policy.d/10-test.toml"), POLICY).unwrap();

        let perm_socket = dir.join("perm.sock");
        let store_socket = dir.join("store.sock");
        let audit_log = dir.join("audit.jsonl");
        let shutdown = Shutdown::new();

        let policy = Policy::load_dir(&dir.join("policy.d")).expect("policy must load");
        policy
            .validate(&ActionRegistry::builtin())
            .expect("the test policy must name only registered actions");
        let broker = Arc::new(Broker::new(policy, AuditLog::open(&audit_log).unwrap()));
        let ps = Server::bind(&perm_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || ps.serve(broker, s));

        // Encrypted, as a node's store always is.
        let store = Store::encrypted(dir.join("store"), StorageKey::generate());
        store.ensure_layout().unwrap();
        let service = Arc::new(StoreService::new(store, perm_socket.clone()));
        let ss = Server::bind(&store_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || ss.serve(service, s));

        for sock in [&perm_socket, &store_socket] {
            Client::connect_waiting(sock, Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{} never came up", sock.display()));
        }

        Harness {
            dir,
            perm_socket,
            store_socket,
            audit_log,
            shutdown,
        }
    }

    fn token(&self, action: &str) -> String {
        let mut broker = Client::connect(&self.perm_socket).unwrap();
        broker
            .call("perm.request", json!({ "action": action }))
            .unwrap()
            .unwrap_or_else(|e| panic!("policy allows {action}: {}", e.message))["token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn call(&self, method: &str, params: Value, action: &str) -> Result<Value, otwono_proto::RpcError> {
        let token = self.token(action);
        Client::connect_with_timeout(&self.store_socket, Duration::from_secs(30))
            .unwrap()
            .call_with_capability(method, params, &token)
            .unwrap()
    }

    /// Store `payload` under `label` and return its content id.
    fn put(&self, payload: &[u8], label: &str) -> String {
        let out = self
            .call(
                "store.put",
                json!({
                    "data": data_encoding::BASE64.encode(payload),
                    "visibility": label,
                }),
                "store.write",
            )
            .expect("put");
        assert_eq!(out["visibility"], label);
        out["content_id"].as_str().unwrap().to_string()
    }

    fn audit(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.audit_log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn payload(tag: u8) -> Vec<u8> {
    let mut v = Vec::new();
    while v.len() < 200_000 {
        v.extend_from_slice(&[tag; 997]);
        v.extend_from_slice(b"-boundary-");
    }
    v
}

// --- the negative suite -------------------------------------------------------------------

#[test]
fn a_private_object_is_never_served_to_a_peer() {
    // The headline rule of DATA-VISIBILITY.md, at the one method that can break it.
    let h = Harness::start("private");
    let id = h.put(&payload(1), "private");

    let err = h
        .call(
            "store.serve",
            json!({ "content_id": id, "peer": "otw1:stranger" }),
            "store.serve",
        )
        .expect_err("a private object must not be served");
    assert!(err.message.contains("not available to peers"), "{}", err.message);

    // And it is readable locally, which is what makes the refusal a *label* decision
    // rather than the object being missing.
    let local = h
        .call("store.get", json!({ "content_id": id }), "store.read")
        .expect("local read");
    assert_eq!(
        data_encoding::BASE64
            .decode(local["data"].as_str().unwrap().as_bytes())
            .unwrap(),
        payload(1)
    );
}

#[test]
fn a_shared_object_is_not_served_either_until_per_peer_authorization_exists() {
    // Shared needs a per-peer decision this daemon cannot yet make. Answering "maybe" as
    // "yes" is how data leaks, so it answers no.
    let h = Harness::start("shared");
    let id = h.put(&payload(2), "shared");
    assert!(h
        .call("store.serve", json!({ "content_id": id }), "store.serve")
        .is_err());
}

#[test]
fn public_and_replicated_objects_are_served() {
    let h = Harness::start("public");
    for label in ["public", "replicated"] {
        let body = payload(label.len() as u8);
        let id = h.put(&body, label);
        let out = h
            .call(
                "store.serve",
                json!({ "content_id": id, "peer": "otw1:friend" }),
                "store.serve",
            )
            .unwrap_or_else(|e| panic!("{label} should be served: {}", e.message));
        assert_eq!(
            data_encoding::BASE64
                .decode(out["data"].as_str().unwrap().as_bytes())
                .unwrap(),
            body
        );
        assert_eq!(out["served_to"], "otw1:friend");
    }
}

#[test]
fn a_refusal_does_not_reveal_whether_the_object_exists() {
    // For a content-addressed store, distinguishing "private" from "absent" would confirm
    // that this node holds bytes the asker already guessed. Both answers must be the same
    // string.
    let h = Harness::start("oracle");
    let private = h.put(&payload(3), "private");
    let absent = "f".repeat(64);

    let a = h
        .call("store.serve", json!({ "content_id": private }), "store.serve")
        .expect_err("private");
    let b = h
        .call("store.serve", json!({ "content_id": absent }), "store.serve")
        .expect_err("absent");

    assert_eq!(a.code, b.code, "the codes must match");
    // The messages differ only by the id the caller already supplied.
    assert_eq!(
        a.message.replace(&private, "ID"),
        b.message.replace(&absent, "ID"),
        "a private object and an absent one must be indistinguishable"
    );
}

#[test]
fn an_unlabelled_object_is_private_and_therefore_not_served() {
    // The fail-closed rule reaching the wire: a caller that omits the label gets the safe
    // one, not the convenient one.
    let h = Harness::start("unlabelled");
    let out = h
        .call(
            "store.put",
            json!({ "data": data_encoding::BASE64.encode(&payload(4)) }),
            "store.write",
        )
        .expect("put with no label");
    assert_eq!(out["visibility"], "private");
    assert!(h
        .call(
            "store.serve",
            json!({ "content_id": out["content_id"] }),
            "store.serve"
        )
        .is_err());
}

#[test]
fn a_nonsense_label_is_private_rather_than_an_error() {
    // A caller that sends a label from a newer version must not have its data stored more
    // widely than it meant, and must not fail either.
    let h = Harness::start("nonsense");
    let out = h
        .call(
            "store.put",
            json!({
                "data": data_encoding::BASE64.encode(&payload(5)),
                "visibility": "world-readable",
            }),
            "store.write",
        )
        .expect("put");
    assert_eq!(out["visibility"], "private");
}

// --- capability separation ----------------------------------------------------------------

#[test]
fn serving_a_peer_does_not_require_being_able_to_read_the_store() {
    // The point of a separate capability: otwono-netd holds store.serve and not store.read.
    let h = Harness::start("caps");
    let private = h.put(&payload(6), "private");
    let public = h.put(&payload(7), "public");

    let serve_token = h.token("store.serve");
    let mut c = Client::connect(&h.store_socket).unwrap();
    let err = c
        .call_with_capability("store.get", json!({ "content_id": private }), &serve_token)
        .unwrap()
        .expect_err("store.serve is not store.read");
    assert_eq!(err.code, code::UNAUTHORIZED, "{}", err.message);

    // The same token serves public content perfectly well.
    let mut c = Client::connect(&h.store_socket).unwrap();
    assert!(c
        .call_with_capability("store.serve", json!({ "content_id": public }), &serve_token)
        .unwrap()
        .is_ok());
}

#[test]
fn reading_the_store_does_not_authorize_writing_to_it() {
    let h = Harness::start("readonly");
    let read_token = h.token("store.read");
    let mut c = Client::connect(&h.store_socket).unwrap();
    let err = c
        .call_with_capability(
            "store.put",
            json!({ "data": data_encoding::BASE64.encode(b"x") }),
            &read_token,
        )
        .unwrap()
        .expect_err("read is not write");
    assert_eq!(err.code, code::UNAUTHORIZED);
}

#[test]
fn every_method_needs_a_token() {
    let h = Harness::start("notoken");
    for (method, params) in [
        ("store.put", json!({ "data": "" })),
        ("store.get", json!({ "content_id": "0".repeat(64) })),
        ("store.stat", json!({ "content_id": "0".repeat(64) })),
        ("store.serve", json!({ "content_id": "0".repeat(64) })),
    ] {
        let err = Client::connect(&h.store_socket)
            .unwrap()
            .call(method, params)
            .unwrap()
            .expect_err("no token, no call");
        assert_eq!(err.code, code::UNAUTHORIZED, "{method}");
    }
}

// --- the ordinary path --------------------------------------------------------------------

#[test]
fn bytes_go_in_and_come_back_identical_through_the_control_plane() {
    let h = Harness::start("roundtrip");
    let body = payload(8);
    let id = h.put(&body, "public");
    let out = h
        .call("store.get", json!({ "content_id": id }), "store.read")
        .expect("get");
    assert_eq!(
        data_encoding::BASE64
            .decode(out["data"].as_str().unwrap().as_bytes())
            .unwrap(),
        body
    );
    assert!(out["chunks"].as_u64().unwrap() >= 1);
}

#[test]
fn storing_the_same_bytes_twice_gives_the_same_name() {
    let h = Harness::start("dedup");
    let body = payload(9);
    assert_eq!(h.put(&body, "public"), h.put(&body, "public"));
}

#[test]
fn stat_describes_an_object_without_handing_over_its_bytes() {
    let h = Harness::start("stat");
    let id = h.put(&payload(10), "private");
    let out = h
        .call("store.stat", json!({ "content_id": id }), "store.read")
        .expect("stat");
    assert_eq!(out["complete"], true);
    assert!(out["data"].is_null(), "stat must not return bytes");
}

#[test]
fn a_malformed_content_id_is_a_caller_error() {
    let h = Harness::start("badid");
    for bad in ["", "zz", &"f".repeat(63)] {
        let err = h
            .call("store.get", json!({ "content_id": bad }), "store.read")
            .expect_err("{bad} is not an id");
        assert_eq!(err.code, code::INVALID_PARAMS, "{bad:?}");
    }
}

#[test]
fn every_serve_leaves_a_record_naming_the_action() {
    // "What left this node, and when" has to be answerable from the audit log.
    let h = Harness::start("audit");
    let id = h.put(&payload(11), "public");
    h.call("store.serve", json!({ "content_id": id }), "store.serve")
        .expect("serve");
    assert!(
        h.audit()
            .iter()
            .any(|r| r["action"] == "store.serve" && r["event"] == "token_issued"),
        "no store.serve token in the audit log"
    );
}

// --- provenance and demotion, the last two Section 6 criteria -----------------------------

#[test]
fn derived_content_cannot_launder_a_label_over_the_wire() {
    // The property test DATA-VISIBILITY.md Section 6 asks for, at the daemon: a summary of
    // a private note stays private no matter what the caller requests, and stays unservable.
    let h = Harness::start("derive");
    let private = h.put(&payload(12), "private");
    let public = h.put(&payload(13), "public");

    let out = h
        .call(
            "store.put",
            json!({
                "data": data_encoding::BASE64.encode(b"a summary of both"),
                "visibility": "public",
                "derived_from": [private, public],
            }),
            "store.write",
        )
        .expect("derive");

    assert_eq!(out["visibility"], "private", "{out}");
    assert_eq!(
        out["requested_visibility"], "public",
        "the request is reported back"
    );
    assert!(h
        .call(
            "store.serve",
            json!({ "content_id": out["content_id"] }),
            "store.serve"
        )
        .is_err());
}

#[test]
fn deriving_from_something_this_node_does_not_have_is_refused() {
    // Silently dropping a missing input would make the derived label looser than it should
    // be — the failure that must not be quiet.
    let h = Harness::start("derive-missing");
    let err = h
        .call(
            "store.put",
            json!({
                "data": data_encoding::BASE64.encode(b"x"),
                "visibility": "public",
                "derived_from": ["a".repeat(64)],
            }),
            "store.write",
        )
        .expect_err("an absent input must not be ignored");
    assert_eq!(err.code, code::INVALID_PARAMS, "{}", err.message);
}

#[test]
fn demotion_stops_future_serving() {
    // The fourth Section 6 criterion, end to end: served, demoted, refused.
    let h = Harness::start("demote");
    let id = h.put(&payload(14), "public");
    h.call("store.serve", json!({ "content_id": id }), "store.serve")
        .expect("served while public");

    let out = h
        .call(
            "store.demote",
            json!({ "content_id": id, "visibility": "private" }),
            "store.write",
        )
        .expect("demote");
    assert_eq!(out["visibility"], "private");
    assert_eq!(
        out["recalled_from_peers"], false,
        "the reply must not imply anything was recalled"
    );

    assert!(
        h.call("store.serve", json!({ "content_id": id }), "store.serve")
            .is_err(),
        "serving must stop after demotion"
    );
    // And it is still readable locally, so this was a label change and not a deletion.
    assert!(h
        .call("store.get", json!({ "content_id": id }), "store.read")
        .is_ok());
}

#[test]
fn widening_a_label_is_refused_by_this_daemon() {
    // label.promote always confirms, and this daemon does not hold it. A caller that wants
    // to widen goes to the broker rather than round it.
    let h = Harness::start("promote");
    let id = h.put(&payload(15), "private");
    let err = h
        .call(
            "store.demote",
            json!({ "content_id": id, "visibility": "public" }),
            "store.write",
        )
        .expect_err("widening must be refused");
    assert!(err.message.contains("label.promote"), "{}", err.message);

    // Unchanged, and still not servable.
    assert!(h
        .call("store.serve", json!({ "content_id": id }), "store.serve")
        .is_err());
}

#[test]
fn describe_is_open_and_names_the_capability_each_method_needs() {
    let h = Harness::start("describe");
    let described = Client::connect(&h.store_socket)
        .unwrap()
        .call("describe", json!({}))
        .unwrap()
        .expect("describe is open");
    let methods = described["methods"].as_array().unwrap();
    for (name, cap) in [
        ("store.put", "store.write"),
        ("store.get", "store.read"),
        ("store.stat", "store.read"),
        ("store.serve", "store.serve"),
        ("store.demote", "store.write"),
    ] {
        let m = methods
            .iter()
            .find(|m| m["name"] == name)
            .unwrap_or_else(|| panic!("{name} should be described"));
        assert_eq!(m["capability"], cap, "{name}");
    }
}

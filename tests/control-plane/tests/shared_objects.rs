//! `SHARED` end to end over the real control plane: three daemons, no shortcuts.
//!
//! ADR-0019 splits the work across processes on purpose. `otwono-stored` seals and chunks
//! but never holds the sharing key; `otwono-idd` holds it and hands back one content key at
//! a time; `otwono-permd` decides who may ask for either. A test that stubbed any of them
//! would be testing a design that does not exist, so all three run here over real sockets.
//!
//! What is deliberately *not* here: serving a shared object to a peer. Per-peer
//! authorization is ADR-0019 §4 and is not built, so `SHARED` still fails closed at the
//! link — and one of the tests below pins that rather than leaving it to be assumed.

use otwono_idd::IdentityService;
use otwono_identity::{SharingKeystore, SigningKeystore};
use otwono_permd::{ActionRegistry, AuditLog, Broker, Policy};
use otwono_proto::{Client, Server, Shutdown};
use otwono_store::{StorageKey, Store};
use otwono_stored::StoreService;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Everything a node needs to share and to be shared with.
const POLICY: &str = r#"
[[rule]]
action = "store.*"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "id.unwrap_shared"
decision = "allow"
ttl_seconds = 300
"#;

/// The same, minus the ability to open what was shared with this node.
const NO_UNWRAP_POLICY: &str = r#"
[[rule]]
action = "store.*"
decision = "allow"
ttl_seconds = 300
"#;

struct Harness {
    dir: PathBuf,
    perm_socket: PathBuf,
    store_socket: PathBuf,
    id_socket: PathBuf,
    shutdown: Shutdown,
}

impl Harness {
    fn start(tag: &str, policy_toml: &str) -> Harness {
        let dir = std::env::temp_dir().join(format!("otw-sh{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::write(dir.join("policy.d/10-test.toml"), policy_toml).unwrap();

        let perm_socket = dir.join("perm.sock");
        let store_socket = dir.join("store.sock");
        let id_socket = dir.join("id.sock");
        let shutdown = Shutdown::new();

        let policy = Policy::load_dir(&dir.join("policy.d")).expect("policy must load");
        policy
            .validate(&ActionRegistry::builtin())
            .expect("the test policy must name only registered actions");
        let broker = Arc::new(Broker::new(
            policy,
            AuditLog::open(dir.join("audit.jsonl")).unwrap(),
        ));
        let ps = Server::bind(&perm_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || ps.serve(broker, s));

        let keystore = SigningKeystore::new(dir.join("identity"));
        let (signing, _) = keystore.load_or_generate().unwrap();
        let (sharing, _) = SharingKeystore::new(dir.join("identity"))
            .load_or_generate()
            .unwrap();
        let idd = Arc::new(IdentityService::new(keystore, signing, sharing, perm_socket.clone()).unwrap());
        let is = Server::bind(&id_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || is.serve(idd, s));

        let store = Store::encrypted(dir.join("store"), StorageKey::generate());
        store.ensure_layout().unwrap();
        let service =
            Arc::new(StoreService::new(store, perm_socket.clone()).with_identity(id_socket.clone()));
        let ss = Server::bind(&store_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || ss.serve(service, s));

        for sock in [&perm_socket, &id_socket, &store_socket] {
            Client::connect_waiting(sock, Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{} never came up", sock.display()));
        }

        Harness {
            dir,
            perm_socket,
            store_socket,
            id_socket,
            shutdown,
        }
    }

    fn token(&self, action: &str) -> Option<String> {
        Client::connect(&self.perm_socket)
            .unwrap()
            .call("perm.request", json!({ "action": action }))
            .unwrap()
            .ok()?
            .get("token")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn call(&self, method: &str, params: Value, action: &str) -> Result<Value, otwono_proto::RpcError> {
        let token = self
            .token(action)
            .unwrap_or_else(|| panic!("policy allows {action}"));
        Client::connect_with_timeout(&self.store_socket, Duration::from_secs(30))
            .unwrap()
            .call_with_capability(method, params, &token)
            .unwrap()
    }

    /// This node's own sharing binding, as a peer would obtain it.
    fn my_binding(&self) -> otwono_identity::SharingBinding {
        let value = Client::connect(&self.id_socket)
            .unwrap()
            .call("id.sharing_binding", json!({}))
            .unwrap()
            .unwrap();
        serde_json::from_value(value).unwrap()
    }

    fn node_id(&self) -> String {
        Client::connect(&self.id_socket)
            .unwrap()
            .call("id.fingerprint", json!({}))
            .unwrap()
            .unwrap()["node_id"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A binding for somebody who is not this node, signed by their own key.
fn stranger(seed: u8) -> (otwono_identity::SharingBinding, otwono_identity::SharingKey) {
    let identity = otwono_identity::NodeIdentity::from_seeds(
        &[seed; 32],
        &[seed.wrapping_add(100); 32],
        1_700_000_000_000,
    );
    let sharing = otwono_identity::SharingKey::from_seed(&[seed.wrapping_add(7); 32], 1_700_000_000_000);
    (identity.signing().bind_sharing(&sharing.public()), sharing)
}

fn payload(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[test]
fn a_node_shares_with_itself_and_reads_it_back() {
    // The complete path across three processes: seal and chunk in otwono-stored, unwrap in
    // otwono-idd, and a permission for each half from otwono-permd.
    let h = Harness::start("selfshare", POLICY);
    let plaintext = payload(1, 40_000);

    let out = h
        .call(
            "store.share",
            json!({
                "data": data_encoding::BASE64.encode(&plaintext),
                "recipients": [h.my_binding()],
            }),
            "store.share",
        )
        .expect("share");
    assert_eq!(out["visibility"], "shared");
    assert_eq!(out["sharing"]["plaintext_size_bytes"], plaintext.len() as u64);
    assert_eq!(out["sharing"]["authorized"], json!([h.node_id()]));
    let id = out["content_id"].as_str().unwrap().to_string();

    let opened = h
        .call(
            "store.open_shared",
            json!({ "content_id": id }),
            "id.unwrap_shared",
        )
        .expect("open_shared");
    let back = data_encoding::BASE64
        .decode(opened["data"].as_str().unwrap().as_bytes())
        .unwrap();
    assert_eq!(back, plaintext);
}

#[test]
fn store_get_will_not_read_a_sealed_object_as_if_it_were_plaintext() {
    // A caller who reaches for the wrong method must get ciphertext-shaped nonsense at
    // worst, and ideally an explanation. What it must never get is the plaintext, because
    // that would mean store.read alone opened a shared object with no unwrap at all.
    let h = Harness::start("wrongmethod", POLICY);
    let plaintext = b"the quarterly figures".repeat(200);
    let out = h
        .call(
            "store.share",
            json!({
                "data": data_encoding::BASE64.encode(&plaintext),
                "recipients": [h.my_binding()],
            }),
            "store.share",
        )
        .unwrap();
    let id = out["content_id"].as_str().unwrap().to_string();

    let raw = h
        .call("store.get", json!({ "content_id": id }), "store.read")
        .expect("store.get reads the object's bytes, which are ciphertext");
    let bytes = data_encoding::BASE64
        .decode(raw["data"].as_str().unwrap().as_bytes())
        .unwrap();
    assert_ne!(bytes, plaintext);
    assert!(!bytes.windows(21).any(|w| w == b"the quarterly figures"));
}

#[test]
fn an_object_shared_with_somebody_else_cannot_be_opened_here() {
    // This node holds the ciphertext and the whole envelope, and still cannot read it. If
    // this ever passes trivially -- because the daemon fell back to some other key -- the
    // recipient list means nothing.
    let h = Harness::start("notmine", POLICY);
    let (their_binding, _) = stranger(3);
    let out = h
        .call(
            "store.share",
            json!({
                "data": data_encoding::BASE64.encode(&payload(2, 20_000)),
                "recipients": [their_binding],
            }),
            "store.share",
        )
        .expect("sharing with somebody else is allowed; reading it back is not");
    let id = out["content_id"].as_str().unwrap().to_string();

    let err = h
        .call(
            "store.open_shared",
            json!({ "content_id": id }),
            "id.unwrap_shared",
        )
        .expect_err("there is no copy of the key for this node");
    assert!(
        err.message.contains("not shared with this node"),
        "{}",
        err.message
    );
}

#[test]
fn opening_needs_the_unwrap_capability_not_merely_store_read() {
    // The split ADR-0019 §3 exists for: reading the store and unwrapping a content key are
    // two decisions. A caller with store.read must not get the second for free by asking
    // the store instead of the identity daemon.
    let h = Harness::start("nounwrap", NO_UNWRAP_POLICY);
    let out = h
        .call(
            "store.share",
            json!({
                "data": data_encoding::BASE64.encode(&payload(4, 10_000)),
                "recipients": [h.my_binding()],
            }),
            "store.share",
        )
        .unwrap();
    let id = out["content_id"].as_str().unwrap().to_string();

    assert!(
        h.token("id.unwrap_shared").is_none(),
        "this policy must not grant unwrapping"
    );
    // A store.read token is what such a caller does have. It must not open the object.
    let read_token = h.token("store.read").expect("this policy grants store.read");
    let err = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability("store.open_shared", json!({ "content_id": id }), &read_token)
        .unwrap()
        .expect_err("store.read must not stand in for id.unwrap_shared");
    assert!(err.message.contains("id.unwrap_shared"), "{}", err.message);
}

#[test]
fn a_recipient_whose_binding_does_not_verify_is_refused() {
    // Sealing to an unverified key would seal to whoever claimed to be the recipient. The
    // binding here is well-formed and signed -- by the wrong key for the NodeID it names.
    let h = Harness::start("badbinding", POLICY);
    let (mut binding, _) = stranger(5);
    let (other, _) = stranger(6);
    binding.node_id = other.node_id;

    let err = h
        .call(
            "store.share",
            json!({
                "data": data_encoding::BASE64.encode(&payload(6, 5_000)),
                "recipients": [binding],
            }),
            "store.share",
        )
        .expect_err("a binding that does not check out is not a recipient");
    assert!(err.message.contains("does not check out"), "{}", err.message);
}

#[test]
fn an_object_with_no_recipients_is_refused() {
    let h = Harness::start("norecipients", POLICY);
    let err = h
        .call(
            "store.share",
            json!({
                "data": data_encoding::BASE64.encode(&payload(7, 1_000)),
                "recipients": [],
            }),
            "store.share",
        )
        .expect_err("an object nobody can open is not a shared object");
    assert!(err.message.contains("nobody can open"), "{}", err.message);
}

#[test]
fn the_same_recipient_twice_is_refused_rather_than_deduplicated() {
    // Two copies under one name is either a duplicate or two different keys, and there is
    // no way to tell which. Quietly picking one would be picking a key for the user.
    let h = Harness::start("twice", POLICY);
    let (binding, _) = stranger(8);
    let err = h
        .call(
            "store.share",
            json!({
                "data": data_encoding::BASE64.encode(&payload(8, 1_000)),
                "recipients": [binding.clone(), binding],
            }),
            "store.share",
        )
        .expect_err("one recipient, two keys, no way to choose");
    assert!(err.message.contains("appears twice"), "{}", err.message);
}

#[test]
fn every_named_recipient_gets_a_copy_and_nobody_else_does() {
    let h = Harness::start("several", POLICY);
    let (alice, alice_key) = stranger(9);
    let (bob, _) = stranger(10);
    let (_, outsider_key) = stranger(11);
    let plaintext = payload(9, 30_000);

    let out = h
        .call(
            "store.share",
            json!({
                "data": data_encoding::BASE64.encode(&plaintext),
                "recipients": [alice.clone(), bob.clone()],
            }),
            "store.share",
        )
        .unwrap();
    let authorized = out["sharing"]["authorized"].as_array().unwrap();
    assert_eq!(authorized.len(), 2);
    assert!(authorized.contains(&json!(alice.node_id.to_text())));
    assert!(authorized.contains(&json!(bob.node_id.to_text())));

    // Alice's own copy opens with Alice's key, and nobody else's does.
    let sealed: Vec<otwono_identity::SealedKey> =
        serde_json::from_value(out["sharing"]["sealed_keys"].clone()).unwrap();
    let hers = sealed
        .iter()
        .find(|k| k.recipient == alice.node_id.to_text())
        .expect("alice has a copy");
    assert!(alice_key.open(hers).is_ok());
    for copy in &sealed {
        assert!(outsider_key.open(copy).is_err(), "an outsider opened {copy:?}");
    }
}

#[test]
fn a_shared_object_still_cannot_be_served_to_a_peer() {
    // ADR-0019 §4 is not built. Until per-peer authorization exists, a shared object must
    // be refused exactly like a private one -- and it must be refused with the same words,
    // or the refusal itself says this node holds something.
    let h = Harness::start("noserve", POLICY);
    let out = h
        .call(
            "store.share",
            json!({
                "data": data_encoding::BASE64.encode(&payload(10, 8_000)),
                "recipients": [h.my_binding()],
            }),
            "store.share",
        )
        .unwrap();
    let shared_id = out["content_id"].as_str().unwrap().to_string();
    let absent = "0".repeat(64);

    let refused = h
        .call("store.serve", json!({ "content_id": shared_id }), "store.serve")
        .expect_err("shared must fail closed until §4 exists");
    let missing = h
        .call("store.serve", json!({ "content_id": absent }), "store.serve")
        .expect_err("an absent object is refused too");
    assert_eq!(
        refused.message.replace(&shared_id, "<id>"),
        missing.message.replace(&absent, "<id>"),
        "a held-but-unauthorized object and an absent one must be indistinguishable"
    );
}

#[test]
fn the_two_daemons_name_the_same_capability() {
    // store.open_shared forwards the caller's token straight to id.unwrap_shared. A token
    // names one action, so if these strings ever drift apart the forwarded token stops
    // matching and every open fails -- or, worse, someone "fixes" it by having the store
    // request its own token, which would let anyone with store.read open everything shared
    // with this node.
    assert_eq!(otwono_stored::CAPABILITY_UNWRAP, otwono_idd::CAPABILITY_UNWRAP);
    assert!(otwono_permd::ActionRegistry::builtin()
        .get(otwono_stored::CAPABILITY_UNWRAP)
        .is_some());
}

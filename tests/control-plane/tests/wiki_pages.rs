//! A wiki page written and read back over the real control plane (ADR-0032).
//!
//! The crate next door tests the chain's rules against a map. This tests the part that only
//! shows up against daemons: that a revision signed by `otwono-idd` verifies, that the
//! pointer and the revision agree about what the head is, and that a second write leaves the
//! first reachable as a parent rather than replacing it.
//!
//! It drives the same calls `otwono-wikictl` makes rather than the binary, so a failure
//! points at a method and not at a subprocess.

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

const POLICY: &str = r#"
[[rule]]
action = "store.*"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "pointer.*"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "id.sign"
decision = "allow"
ttl_seconds = 300
"#;

struct Harness {
    dir: PathBuf,
    perm: PathBuf,
    store: PathBuf,
    id: PathBuf,
    shutdown: Shutdown,
}

impl Harness {
    fn start(tag: &str) -> Harness {
        let dir = std::env::temp_dir().join(format!("otw-wiki-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::write(dir.join("policy.d/10-test.toml"), POLICY).unwrap();

        let perm = dir.join("perm.sock");
        let store_socket = dir.join("store.sock");
        let id = dir.join("id.sock");
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
        let server = Server::bind(&perm).unwrap();
        std::thread::spawn(move || server.serve(broker, s));

        let keystore = SigningKeystore::new(dir.join("identity"));
        let sharing = SharingKeystore::new(dir.join("identity"));
        let (signing, _) = keystore.load_or_generate().unwrap();
        let idd = Arc::new(
            IdentityService::new(
                keystore,
                signing,
                sharing.load_or_generate().unwrap().0,
                perm.clone(),
            )
            .unwrap(),
        );
        let s = shutdown.clone();
        let server = Server::bind(&id).unwrap();
        std::thread::spawn(move || server.serve(idd, s));

        let store = Store::encrypted(dir.join("store"), StorageKey::generate());
        store.ensure_layout().unwrap();
        let service = Arc::new(
            StoreService::new(store, perm.clone())
                .with_identity(id.clone())
                .with_pointers(otwono_store::PointerStore::at(dir.join("pointers")).unwrap()),
        );
        let s = shutdown.clone();
        let server = Server::bind(&store_socket).unwrap();
        std::thread::spawn(move || server.serve(service, s));

        for sock in [&perm, &id, &store_socket] {
            Client::connect_waiting(sock, Duration::from_secs(5))
                .unwrap_or_else(|e| panic!("{} never came up: {e}", sock.display()));
        }
        Harness {
            dir,
            perm,
            store: store_socket,
            id,
            shutdown,
        }
    }

    fn token(&self, action: &str) -> String {
        Client::connect(&self.perm)
            .unwrap()
            .call("perm.request", json!({ "action": action, "reason": "wiki test" }))
            .unwrap()
            .expect("the broker must grant")["token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn call(&self, sock: &PathBuf, method: &str, params: Value, action: &str) -> Value {
        Client::connect(sock)
            .unwrap()
            .call_with_capability(method, params, &self.token(action))
            .unwrap()
            .unwrap_or_else(|e| panic!("{method} refused: {}", e.message))
    }

    /// The write path `otwono-wikictl write` performs, as one step.
    fn write(&self, page: &str, text: &str) -> String {
        let body = self.call(
            &self.store,
            "store.put",
            json!({ "data": data_encoding::BASE64.encode(text.as_bytes()), "visibility": "public" }),
            "store.write",
        )["content_id"]
            .as_str()
            .unwrap()
            .to_string();

        let parent = self.head(page);
        let author = self.node_id();
        let mut revision = otwono_wiki::Revision::new(&author, page, body, parent, 1_000);
        revision.signature = self.sign(&revision.payload_for_id_sign().unwrap());

        let head = self.call(
            &self.store,
            "store.put",
            json!({
                "data": data_encoding::BASE64.encode(&serde_json::to_vec(&revision).unwrap()),
                "visibility": "public"
            }),
            "store.write",
        )["content_id"]
            .as_str()
            .unwrap()
            .to_string();

        let next = self.call(
            &self.store,
            "pointer.next_sequence",
            json!({ "service": "wiki", "name": page }),
            "pointer.read",
        )["next_sequence"]
            .as_u64()
            .unwrap();
        let mut pointer =
            otwono_pointer::Pointer::new(&author, "wiki", page, next, Some(head.clone()), 1_000);
        pointer.signature = self.sign(&pointer.payload_for_id_sign().unwrap());
        self.call(
            &self.store,
            "pointer.publish",
            json!({ "record": pointer }),
            "pointer.publish",
        );
        head
    }

    fn head(&self, page: &str) -> Option<String> {
        let out = self.call(
            &self.store,
            "pointer.mine",
            json!({ "service": "wiki", "name": page }),
            "store.serve",
        );
        let record = out.get("record")?;
        if record.is_null() {
            return None;
        }
        let p: otwono_pointer::Pointer = serde_json::from_value(record.clone()).unwrap();
        p.content_id
    }

    fn revision(&self, id: &str) -> otwono_wiki::Revision {
        let out = self.call(
            &self.store,
            "store.get",
            json!({ "content_id": id }),
            "store.read",
        );
        let bytes = data_encoding::BASE64
            .decode(out["data"].as_str().unwrap().as_bytes())
            .unwrap();
        serde_json::from_slice(&bytes).expect("the head must be a revision")
    }

    fn body_text(&self, id: &str) -> String {
        let out = self.call(
            &self.store,
            "store.get",
            json!({ "content_id": id }),
            "store.read",
        );
        String::from_utf8(
            data_encoding::BASE64
                .decode(out["data"].as_str().unwrap().as_bytes())
                .unwrap(),
        )
        .unwrap()
    }

    fn sign(&self, payload: &[u8]) -> String {
        self.call(
            &self.id,
            "id.sign",
            json!({ "payload": data_encoding::BASE64.encode(payload) }),
            "id.sign",
        )["signature"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn node_id(&self) -> otwono_identity::NodeId {
        let v = Client::connect(&self.id)
            .unwrap()
            .call("id.fingerprint", json!({}))
            .unwrap()
            .unwrap();
        otwono_identity::NodeId::parse(v["node_id"].as_str().unwrap()).unwrap()
    }

    fn public_key(&self) -> [u8; 32] {
        let v = Client::connect(&self.id)
            .unwrap()
            .call("id.public_key", json!({}))
            .unwrap()
            .unwrap();
        data_encoding::BASE64
            .decode(v["public_key"].as_str().unwrap().as_bytes())
            .unwrap()
            .try_into()
            .unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_page_written_through_the_daemons_reads_back_as_its_text() {
    let h = Harness::start("roundtrip");
    let head = h.write("Getting-Started", "the first page on this node\n");

    assert_eq!(h.head("Getting-Started").as_deref(), Some(head.as_str()));
    let revision = h.revision(&head);
    assert_eq!(revision.page, "Getting-Started");
    assert!(revision.is_first(), "a new page's first revision has no parent");
    assert_eq!(h.body_text(&revision.body), "the first page on this node\n");
}

#[test]
fn a_revision_signed_by_otwono_idd_verifies_against_the_node_it_names() {
    // The one thing a map-backed test cannot show: that the payload this crate hands
    // `id.sign` and the message `verify` reconstructs are the same bytes. They are built by
    // two functions that differ by the application domain, and the daemon adds it.
    let h = Harness::start("signature");
    let head = h.write("Getting-Started", "signed by the daemon\n");
    h.revision(&head)
        .verify(&h.public_key())
        .expect("a revision otwono-idd signed must verify against this node's key");
}

#[test]
fn a_second_write_keeps_the_first_as_its_parent() {
    let h = Harness::start("chain");
    let first = h.write("Getting-Started", "one\n");
    let second = h.write("Getting-Started", "two\n");

    assert_ne!(first, second);
    assert_eq!(h.head("Getting-Started").as_deref(), Some(second.as_str()));
    assert_eq!(
        h.revision(&second).parent.as_deref(),
        Some(first.as_str()),
        "the second revision must name the first as its parent"
    );
    // And the first is still there, which is what makes the chain a history rather than a
    // pointer that overwrote something.
    assert_eq!(h.body_text(&h.revision(&first).body), "one\n");
}

#[test]
fn the_history_of_a_page_walks_back_to_its_first_revision() {
    let h = Harness::start("history");
    let mut heads = Vec::new();
    for text in ["one\n", "two\n", "three\n"] {
        heads.push(h.write("Getting-Started", text));
    }
    let key = h.public_key();
    let mine = h.node_id().to_text();

    let shelf = |id: &str| Some(h.revision(id));
    let history = otwono_wiki::walk(
        &shelf,
        heads.last().unwrap(),
        "Getting-Started",
        |author| (author == mine).then_some(key),
        64,
    )
    .expect("this node's own chain must verify");

    assert_eq!(history.end, otwono_wiki::WalkEnd::Complete);
    assert_eq!(
        history
            .steps
            .iter()
            .map(|s| s.content_id.clone())
            .collect::<Vec<_>>(),
        heads.iter().rev().cloned().collect::<Vec<_>>(),
        "history must be head first, back to the first revision"
    );
}

#[test]
fn a_name_already_pointing_at_something_that_is_not_a_revision_is_refused() {
    // Found on the first booted run of the wiki check. `wiki/<name>` is an ordinary pointer
    // and anything holding `pointer.publish` can put anything under it — the mesh content
    // check publishes `wiki/Getting-Started` naming a plain text blob, deliberately, because
    // it is testing the pointer primitive and not this service.
    //
    // Chaining onto that produced a page whose history was broken from birth: a first
    // revision with a parent nothing could parse, reported as truncated for ever after. The
    // write is refused instead, and not silently restarted as a fresh chain — ignoring the
    // existing head would move a name somebody else is using and lose what they had there.
    let h = Harness::start("occupied");

    let blob = h.call(
        &h.store,
        "store.put",
        json!({ "data": data_encoding::BASE64.encode(b"not a revision, just words"), "visibility": "public" }),
        "store.write",
    )["content_id"]
        .as_str()
        .unwrap()
        .to_string();
    let author = h.node_id();
    let mut pointer = otwono_pointer::Pointer::new(&author, "wiki", "Occupied", 1, Some(blob), 1_000);
    pointer.signature = h.sign(&pointer.payload_for_id_sign().unwrap());
    h.call(
        &h.store,
        "pointer.publish",
        json!({ "record": pointer }),
        "pointer.publish",
    );

    // What `write` does before adopting a head as its parent: fetch it and require that it
    // parses as a revision of this page.
    let head = h.head("Occupied").expect("the name is taken");
    let out = h.call(&h.store, "store.get", json!({ "content_id": head }), "store.read");
    let bytes = data_encoding::BASE64
        .decode(out["data"].as_str().unwrap().as_bytes())
        .unwrap();
    // And the rule refuses it. Asserting only that the fixture fails to parse would be
    // asserting the fixture, not the behaviour.
    let err = otwono_wiki::may_extend(&bytes, "Occupied")
        .expect_err("a head that is not a revision must not be extendable");
    assert!(matches!(err, otwono_wiki::WikiError::Malformed(_)), "{err}");
}

#[test]
fn a_revision_of_one_page_is_not_a_parent_for_another() {
    // The other half of the same check. Both of these are genuine revisions signed by this
    // node; the head under `Other` simply belongs to a different page, and extending it would
    // graft one page's history onto another's.
    let h = Harness::start("crossed");
    let elsewhere = h.write("Somewhere-Else", "a page of its own\n");

    let author = h.node_id();
    let mut pointer =
        otwono_pointer::Pointer::new(&author, "wiki", "Other", 1, Some(elsewhere.clone()), 1_000);
    pointer.signature = h.sign(&pointer.payload_for_id_sign().unwrap());
    h.call(
        &h.store,
        "pointer.publish",
        json!({ "record": pointer }),
        "pointer.publish",
    );

    let head = h.head("Other").expect("the name is taken");
    let out = h.call(&h.store, "store.get", json!({ "content_id": head }), "store.read");
    let bytes = data_encoding::BASE64
        .decode(out["data"].as_str().unwrap().as_bytes())
        .unwrap();
    // It parses, and is genuinely this node's. It is simply another page's, and grafting one
    // page's history onto another is what the rule refuses.
    assert_eq!(
        otwono_wiki::may_extend(&bytes, "Somewhere-Else").unwrap().page,
        "Somewhere-Else"
    );
    let err = otwono_wiki::may_extend(&bytes, "Other")
        .expect_err("a revision of another page must not be extendable");
    assert!(
        matches!(err, otwono_wiki::WikiError::WrongPage { ref found, .. } if found == "Somewhere-Else"),
        "{err}"
    );
}

#[test]
fn a_page_that_was_never_written_has_no_head() {
    // What `write` relies on to know it is starting a chain rather than continuing one, and
    // what `read` reports as "no such page" rather than an error about a missing object.
    let h = Harness::start("absent");
    assert_eq!(h.head("Never-Written"), None);
}

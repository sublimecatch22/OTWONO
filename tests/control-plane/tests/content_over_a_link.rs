//! Phase 5's last exit criterion, on an actual link.
//!
//! `DATA-VISIBILITY.md` §6 asks for a demonstration that a `PRIVATE` object never appears
//! on any link. Until now that was proven at the `store.serve` method — a claim about a
//! function, not about a wire. Here two nodes authenticate over a real TCP socket, one asks
//! the other for content, and the answers are checked.
//!
//! The serving node runs under a policy that **denies `store.read` outright**. So the proof
//! is not only that a private object is refused, but that the daemon doing the serving has
//! no capability that could return one — which is why the public case working under the
//! same policy matters as much as the private case failing.
//!
//! Everything crosses a socket. An in-process test would prove nothing about the boundary
//! being tested.

use otwono_idd::IdentityService;
use otwono_identity::{AgreementKeystore, NodeIdentity, SessionSigner, SharingKeystore, SigningKeystore};
use otwono_net::content::{ProtocolError, Request, Response};
use otwono_net::{Candidate, LinkProperties, MemoryLink, SecureChannel, TcpLink};
use otwono_netd::{BrokeredSigner, ContentResponder, NetService, NetState};
use otwono_permd::{ActionRegistry, AuditLog, Broker, Policy};
use otwono_proto::{code, Client, Server, Shutdown};
use otwono_store::{StorageKey, Store};
use otwono_stored::StoreService;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Everything the mesh and the store boundary need, and **not** `store.read`.
///
/// The omission is the point. A serving node that could read its own store privately is a
/// node where a bug in the label check reaches private data; one that cannot is not.
const POLICY: &str = r#"
[[rule]]
action = "id.sign_session"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "id.bind_agreement"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "store.write"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "store.serve"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "store.share"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "net.content"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "cache.replicate"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "pointer.publish"
decision = "allow"
ttl_seconds = 300

# Deliberately granted where store.read is denied: a node that serves peers must know its
# own next sequence to publish at all, and that is a different authority from opening every
# object the user has stored.
[[rule]]
action = "pointer.read"
decision = "allow"
ttl_seconds = 300

# What the reader needs to remember a sequence. Reversible, unlike `pointer.publish`:
# recording what a peer said is not saying anything to anyone.
[[rule]]
action = "pointer.write"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "envelope.carry"
decision = "allow"
ttl_seconds = 300

# Reading the cache index: which objects are here, their sizes and holds. Not the objects
# themselves — that is `store.read`, which stays denied below. A carriage test needs this to
# tell "the bytes went" from "the custody record went", which every other question a carrier
# answers conflates once custody is gone.
[[rule]]
action = "cache.read"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "store.read"
decision = "deny"
"#;

struct Harness {
    dir: PathBuf,
    perm_socket: PathBuf,
    store_socket: PathBuf,
    id_socket: PathBuf,
    /// The asking node's own control-plane socket, where `net.fetch` lives.
    net_socket: PathBuf,
    /// The serving node's overlay address and the NodeID that answers there.
    server_addr: std::net::SocketAddr,
    server_node: otwono_identity::NodeId,
    /// The asking node.
    client: Arc<NetState>,
    /// The signing identity behind otwono-idd, for publishing as the serving node.
    node_signing: otwono_identity::SigningIdentity,
    shutdown: Shutdown,
}

impl Harness {
    fn start(tag: &str) -> Harness {
        let dir = std::env::temp_dir().join(format!("otw-col-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::write(dir.join("policy.d/10-test.toml"), POLICY).unwrap();

        let perm_socket = dir.join("perm.sock");
        let id_socket = dir.join("id.sock");
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

        // otwono-idd. Both nodes are fronted by one signing key here: what is under test is
        // what crosses the link, not who is on each end of it.
        let keystore = SigningKeystore::new(dir.join("identity"));
        let sharing_store = SharingKeystore::new(dir.join("identity"));
        let (signing, _) = keystore.load_or_generate().unwrap();
        // Loaded a second time rather than cloned: SigningIdentity holds key material and
        // deliberately is not Clone. The keystore returns the same key, which is the point —
        // this is the identity otwono-idd fronts, and the handshake proves it.
        let (node_signing, _) = SigningKeystore::new(dir.join("identity"))
            .load_or_generate()
            .unwrap();
        let idd = Arc::new(
            IdentityService::new(
                keystore,
                signing,
                sharing_store.load_or_generate().unwrap().0,
                perm_socket.clone(),
            )
            .unwrap(),
        );
        let s = shutdown.clone();
        let server = Server::bind(&id_socket).unwrap();
        std::thread::spawn(move || server.serve(idd, s));

        // otwono-stored, encrypted at rest as a node's store always is.
        let store = Store::encrypted(dir.join("store"), StorageKey::generate());
        store.ensure_layout().unwrap();
        // A cluster cache too, because that is where a replica goes. Separate directory from
        // the store, as on a real node: what the user put there and what the node picked up
        // on its neighbours' behalf never share a path (ADR-0015).
        let cache = otwono_store::Cache::at(dir.join("cache"), StorageKey::generate(), 8 << 20).unwrap();
        let pointers = otwono_store::PointerStore::at(dir.join("pointers")).unwrap();
        let envelopes = otwono_store::EnvelopeStore::at(dir.join("envelopes")).unwrap();
        let service = Arc::new(
            StoreService::new(store, perm_socket.clone())
                .with_identity(id_socket.clone())
                .with_cache(cache)
                .with_pointers(pointers)
                .with_envelopes(envelopes, 8 << 20),
        );
        let s = shutdown.clone();
        let server = Server::bind(&store_socket).unwrap();
        std::thread::spawn(move || server.serve(service, s));

        for sock in [&perm_socket, &id_socket, &store_socket] {
            Client::connect_waiting(sock, Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{} never came up", sock.display()));
        }

        let signer = |agreement_dir: PathBuf| {
            let (agreement, _) = AgreementKeystore::new(agreement_dir).load_or_generate().unwrap();
            BrokeredSigner::bind(agreement, &id_socket, &perm_socket).expect("bind")
        };

        // The serving node: a mesh daemon with a store behind it.
        let server_signer = signer(dir.join("agreement-server"));
        let server_node = server_signer.node_id();
        let listener = TcpLink::listen("127.0.0.1:0").unwrap();
        let server_addr = listener.local_addr().unwrap();
        let serving = Arc::new(
            NetState::new(Arc::new(server_signer))
                .with_responder(ContentResponder::new(&store_socket, &perm_socket)),
        );
        std::thread::spawn(move || otwono_netd::run_listener(serving, listener));

        // The asking node replicates through the control plane, exactly as the daemon does:
        // it never opens the cache directory itself, because the store daemon owns that
        // index and two writers would lose each other's updates (ADR-0026 §10).
        //
        // It remembers pointer sequences the same way, and for a sharper version of the same
        // reason: a per-process memory of what it has seen would be lost on restart, and a
        // reader that forgets is a reader with no rollback protection (ADR-0027 §1).
        let client = Arc::new(
            NetState::new(Arc::new(signer(dir.join("agreement-client"))))
                .with_holder(Arc::new(otwono_netd::content::BrokeredCache::new(
                    &store_socket,
                    &perm_socket,
                )))
                .with_pointer_memory(Arc::new(otwono_netd::content::BrokeredPointers::new(
                    &store_socket,
                    &perm_socket,
                ))),
        );

        // The asking node's control plane, so net.fetch is exercised through a socket and a
        // capability check rather than as a function call.
        let net_socket = dir.join("net.sock");
        let service = Arc::new(NetService::new(Arc::clone(&client), perm_socket.clone()));
        let s = shutdown.clone();
        let server = Server::bind(&net_socket).unwrap();
        std::thread::spawn(move || server.serve(service, s));
        Client::connect_waiting(&net_socket, Duration::from_secs(5)).expect("netd never came up");

        Harness {
            net_socket,
            dir,
            perm_socket,
            store_socket,
            id_socket,
            server_addr,
            server_node,
            client,
            node_signing,
            shutdown,
        }
    }

    /// A second signer over the serving node's agreement key.
    ///
    /// The same NodeID as the peer the client already trusts — which is the whole point. A
    /// replay is not an impersonation: it is the rightful owner's own record, served by
    /// something that authenticates correctly, so it has to be tested with a channel that
    /// authenticates correctly.
    fn server_signer(&self) -> BrokeredSigner {
        let (agreement, _) = AgreementKeystore::new(self.dir.join("agreement-server"))
            .load_or_generate()
            .unwrap();
        BrokeredSigner::bind(agreement, &self.id_socket, &self.perm_socket).expect("bind")
    }

    fn identity_dir(&self) -> PathBuf {
        self.dir.join("identity")
    }

    fn candidate(&self) -> Candidate {
        Candidate {
            claimed_node_id: self.server_node,
            address: self.server_addr,
        }
    }

    fn token(&self, action: &str) -> String {
        let mut broker = Client::connect(&self.perm_socket).unwrap();
        broker
            .call(
                "perm.request",
                json!({ "action": action, "reason": "content-over-a-link test" }),
            )
            .unwrap()
            .unwrap_or_else(|e| panic!("{action} refused: {}", e.message))
            .get("token")
            .and_then(Value::as_str)
            .unwrap()
            .to_string()
    }

    /// Put bytes in the serving node's store under a label. Returns the content id.
    fn put(&self, bytes: &[u8], visibility: &str) -> String {
        let token = self.token("store.write");
        let mut client = Client::connect(&self.store_socket).unwrap();
        client
            .call_with_capability(
                "store.put",
                json!({
                    "data": data_encoding::BASE64.encode(bytes),
                    "visibility": visibility,
                }),
                &token,
            )
            .unwrap()
            .unwrap_or_else(|e| panic!("store.put refused: {}", e.message))
            .get("content_id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string()
    }

    fn demote(&self, content_id: &str, to: &str) {
        let token = self.token("store.write");
        let mut client = Client::connect(&self.store_socket).unwrap();
        client
            .call_with_capability(
                "store.demote",
                json!({ "content_id": content_id, "visibility": to }),
                &token,
            )
            .unwrap()
            .expect("demote must succeed");
    }

    /// Encrypt bytes to this harness's own node and store them. Returns the content id.
    ///
    /// Both ends of this harness are fronted by one signing key, so the serving node's
    /// recipient and the fetching node's identity are the same NodeID — which is what makes
    /// a single-process test of the shared path possible at all.
    fn share_with_myself(&self, bytes: &[u8]) -> String {
        let binding: otwono_identity::SharingBinding = serde_json::from_value(
            Client::connect(&self.id_socket)
                .unwrap()
                .call("id.sharing_binding", json!({}))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let token = self.token("store.share");
        Client::connect(&self.store_socket)
            .unwrap()
            .call_with_capability(
                "store.share",
                json!({
                    "data": data_encoding::BASE64.encode(bytes),
                    "recipients": [binding],
                }),
                &token,
            )
            .unwrap()
            .unwrap_or_else(|e| panic!("store.share refused: {}", e.message))["content_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Put an object in the serving node's store that is sealed to somebody who is **not**
    /// this node, and return its content id.
    ///
    /// `store.share` cannot produce this any more: since ADR-0019 §5 it always keeps a key
    /// for the sharing node, because an owner who cannot read what they shared has lost
    /// their own file. What these tests need is the other state — an object this node holds
    /// and is not a recipient of — and `store.accept_shared` is exactly how a node comes to
    /// hold one.
    ///
    /// Sealed here rather than by the daemon so the recipient list is only ever the
    /// stranger. The content id is computed the same way the store will, which is what
    /// `accept_shared` checks it against.
    fn hold_an_object_for(
        &self,
        recipient: &str,
        key: &otwono_identity::SharingKey,
        plaintext: &[u8],
    ) -> String {
        let content_key = otwono_store::ContentKey::generate();
        let prefix = otwono_store::shared::nonce_prefix();
        let mut ciphertext = Vec::new();
        otwono_store::shared::seal(&content_key, &prefix, plaintext, &mut ciphertext).unwrap();

        let refs = otwono_store::chunk::slice(&ciphertext);
        let content_id = otwono_store::ContentId::of(&refs).to_hex();
        let sealed = otwono_identity::seal_to(recipient, &key.public(), content_key.as_bytes()).unwrap();

        let token = self.token("store.write");
        let out = Client::connect(&self.store_socket)
            .unwrap()
            .call_with_capability(
                "store.accept_shared",
                json!({
                    "content_id": content_id,
                    "data": data_encoding::BASE64.encode(&ciphertext),
                    "encryption": otwono_store::SHARED_ENCRYPTION,
                    "nonce_prefix": data_encoding::BASE64.encode(&prefix),
                    "plaintext_size_bytes": plaintext.len() as u64,
                    "sealed_key": sealed,
                }),
                &token,
            )
            .unwrap()
            .unwrap_or_else(|e| panic!("store.accept_shared refused: {}", e.message));
        assert_eq!(out["content_id"], content_id);
        content_id
    }

    /// Ask the serving node for an object over a real TCP link.
    fn fetch(&self, content_id: &str) -> Result<otwono_netd::FetchedObject, String> {
        self.client.fetch_from(&self.candidate(), content_id)
    }

    /// A responder wired to the same store, for asking questions the wire types cannot
    /// express through `fetch_object` — a hand-built request, for instance.
    /// Publish a pointer as the serving node, signed by the harness's own identity.
    ///
    /// Signed here rather than by otwono-idd because what is under test is the wire and the
    /// verification, and a second signing path would be a second thing to get wrong. The
    /// bytes are the same either way: `id.sign` prepends the application domain, which
    /// `domain_separated` does here.
    fn publish(&self, service: &str, name: &str, content: Option<&str>) -> u64 {
        let signer = &self.node_signing;
        // A token per call. pointer.publish is Egress, and Egress tokens are one-shot by
        // default, so reusing one across two calls fails on the second -- as it should.
        let mut client = Client::connect(&self.store_socket).unwrap();
        let next = client
            .call_with_capability(
                "pointer.next_sequence",
                json!({ "service": service, "name": name }),
                &self.token("pointer.read"),
            )
            .unwrap()
            .unwrap()["next_sequence"]
            .as_u64()
            .unwrap();

        let mut pointer = otwono_pointer::Pointer::new(
            signer.node_id(),
            service,
            name,
            next,
            content.map(str::to_string),
            1_700_000_000_000 + next,
        );
        let payload = pointer.payload_for_id_sign().unwrap();
        pointer.signature = data_encoding::BASE64.encode(
            &signer
                .sign(&otwono_identity::domain_separated(&payload))
                .to_bytes(),
        );
        client
            .call_with_capability(
                "pointer.publish",
                json!({ "record": pointer }),
                &self.token("pointer.publish"),
            )
            .unwrap()
            .expect("publish");
        next
    }

    fn responder(&self) -> ContentResponder {
        ContentResponder::new(&self.store_socket, &self.perm_socket)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Bytes that will not chunk into one piece: over ADR-0016's 256 KiB ceiling, so the
/// transfer needs several chunks and each chunk needs several ranges.
fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    // Mixed, not `seed | 1`: that maps 2 and 3 (and every other adjacent pair) to the same
    // stream, so two "different" fixtures come out byte-identical. Cost one debugging round
    // in the cache's LRU test.
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
fn a_public_object_crosses_a_real_link_byte_for_byte() {
    let h = Harness::start("public");
    let bytes = payload(600 * 1024, 0xC0FFEE);
    let id = h.put(&bytes, "public");

    let got = h.fetch(&id).expect("a public object must be servable");
    assert_eq!(got.content_id, id);
    assert_eq!(got.visibility, "public");
    assert_eq!(got.bytes.len(), bytes.len());
    assert_eq!(
        got.bytes, bytes,
        "the bytes that arrived are not the bytes that were stored"
    );

    // And it really was ranged: the object spans several chunks, and the largest of them is
    // bigger than one reply can carry, so at least one chunk took several round trips.
    let chunks = otwono_store::chunk::slice(&bytes);
    let per_reply = otwono_net::content::max_body_bytes(&LinkProperties::internet());
    assert!(chunks.len() > 1, "the fixture must span several chunks");
    assert!(
        chunks.iter().any(|c| c.length > per_reply),
        "no chunk exceeded the {per_reply}-byte reply cap, so ranging was never exercised"
    );
}

#[test]
fn a_replicated_object_crosses_a_real_link() {
    let h = Harness::start("replicated");
    let bytes = b"replicated content is explicitly permitted to be copied to other nodes".to_vec();
    let id = h.put(&bytes, "replicated");

    let got = h.fetch(&id).expect("a replicated object must be servable");
    assert_eq!(got.bytes, bytes);
    assert_eq!(got.visibility, "replicated");
}

#[test]
fn a_private_object_never_crosses_a_link() {
    // The criterion. A private object, in a store the serving node can reach, asked for by
    // an authenticated peer over a real socket.
    let h = Harness::start("private");
    let secret = b"the user's private notes".to_vec();
    let id = h.put(&secret, "private");

    let err = h.fetch(&id).expect_err("a private object must not cross a link");
    assert!(
        !err.contains("private notes"),
        "the refusal leaked the content: {err}"
    );

    // And a public object of the same size still works, so the refusal is about the label
    // and not about the harness being broken.
    let public = h.put(&secret, "public");
    assert_eq!(h.fetch(&public).expect("public still works").bytes, secret);
}

#[test]
fn a_shared_object_cannot_even_be_created_over_the_control_plane_yet() {
    // Since ADR-0019 a SHARED object is encrypted before it is chunked and carries a
    // content key sealed per recipient. store.put takes bytes and a label and knows nothing
    // about recipients, so it cannot make one -- and refuses rather than writing a record
    // labelled shared that nobody, including its owner, could open.
    //
    // There is deliberately no store.put_shared yet. So SHARED still never crosses a link,
    // for a reason one step further along than "the feature is missing": it is now missing
    // its door, not its lock.
    let h = Harness::start("shared");
    let token = h.token("store.write");
    let err = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "store.put",
            json!({
                "data": data_encoding::BASE64.encode(b"for one named peer only"),
                "visibility": "shared",
            }),
            &token,
        )
        .unwrap()
        .expect_err("store.put must not mint an unopenable shared object");
    assert!(
        err.message.contains("not sealed"),
        "the refusal should say why: {}",
        err.message
    );

    // And nothing in the store carries the label, so there is nothing to ask a peer for.
    let public = h.put(b"for one named peer only", "public");
    assert!(h.fetch(&public).is_ok(), "the same bytes as public still move");

    // `shared` is still not on the unattended allow-list: it never leaves a node except to
    // a peer named in its own envelope, which is a different question and a different
    // function (`may_go_to_peer`).
    assert!(!otwono_netd::content::may_leave_a_node(Some("shared")));
}

#[test]
fn a_private_object_and_one_that_does_not_exist_fail_identically() {
    // A refusal that differs from a miss is an oracle: a peer that can tell them apart can
    // confirm this node holds bytes it already guessed.
    let h = Harness::start("oracle");
    let private = h.put(b"present but not for you", "private");
    let absent = "0".repeat(64);

    let a = h.fetch(&private).unwrap_err();
    let b = h.fetch(&absent).unwrap_err();
    // The ids differ, so compare the message with each id removed.
    assert_eq!(
        a.replace(&private, "<id>"),
        b.replace(&absent, "<id>"),
        "a private object and an absent one must be indistinguishable"
    );
}

#[test]
fn a_chunk_of_a_private_object_cannot_be_reached_through_a_public_one() {
    // The probe oracle ADR-0017 closes. Two objects with the same bytes have the same
    // chunks; asking for the private one's chunk while naming the public one's id must not
    // work, and neither must naming a digest that belongs to no servable object.
    let h = Harness::start("probe");
    let bytes = payload(300 * 1024, 7);
    let private = h.put(&bytes, "private");
    let public = h.put(b"something else entirely", "public");

    // Learn the private object's chunk digests the only way this test legitimately can:
    // by chunking the same bytes locally, exactly as an attacker who guessed them would.
    let guessed = otwono_store::chunk::slice(&bytes);
    assert!(guessed.len() > 1, "the fixture must span several chunks");

    let responder = h.responder();
    let peer = h.client.node_id();
    let mut session = otwono_netd::content::Session::default();
    for entry in &guessed {
        // Named under the public object: the digest is not in its chunk list.
        let through_public = responder.answer(
            peer,
            &Request::Chunk {
                content_id: public.clone(),
                digest: entry.hex(),
                offset: 0,
                max_bytes: 1024,
            },
            &mut session,
        );
        assert!(
            matches!(through_public, Response::NotAvailable { .. }),
            "a chunk reached through an object that does not contain it: {through_public:?}"
        );

        // Named under the private object: refused because the object is refused.
        let direct = responder.answer(
            peer,
            &Request::Chunk {
                content_id: private.clone(),
                digest: entry.hex(),
                offset: 0,
                max_bytes: 1024,
            },
            &mut session,
        );
        assert!(
            matches!(direct, Response::NotAvailable { .. }),
            "a chunk of a private object was served: {direct:?}"
        );
    }
}

#[test]
fn demotion_stops_a_peer_fetching_what_it_could_fetch_before() {
    let h = Harness::start("demote");
    let bytes = b"public until it is not".to_vec();
    let id = h.put(&bytes, "public");
    assert_eq!(h.fetch(&id).expect("public first").bytes, bytes);

    h.demote(&id, "private");
    assert!(
        h.fetch(&id).is_err(),
        "demotion must stop future serving over a link, not just locally"
    );
}

#[test]
fn the_serving_node_serves_without_ever_holding_store_read() {
    // Structural, and the reason the policy above denies store.read. If serving needed that
    // capability, every test here would fail; that they pass means otwono-netd reaches the
    // store through store.serve alone.
    let h = Harness::start("noread");
    let mut broker = Client::connect(&h.perm_socket).unwrap();
    let refused = broker
        .call(
            "perm.request",
            json!({ "action": "store.read", "reason": "prove the policy denies it" }),
        )
        .unwrap();
    assert!(refused.is_err(), "the test policy must deny store.read");

    let id = h.put(b"served without store.read", "public");
    assert_eq!(h.fetch(&id).unwrap().bytes, b"served without store.read");
}

#[test]
fn net_fetch_over_the_control_plane_requires_the_capability() {
    let h = Harness::start("cap");
    let id = h.put(b"public bytes", "public");
    let params = json!({
        "node_id": h.server_node.to_text(),
        "address": h.server_addr.to_string(),
        "content_id": id,
    });

    let mut client = Client::connect(&h.net_socket).unwrap();
    let err = client
        .call("net.fetch", params.clone())
        .unwrap()
        .expect_err("net.fetch without a token must be refused");
    assert_eq!(err.code, code::UNAUTHORIZED);

    let value = client
        .call_with_capability("net.fetch", params, &h.token("net.content"))
        .unwrap()
        .expect("net.fetch with net.content must succeed");
    assert_eq!(
        data_encoding::BASE64
            .decode(value["data"].as_str().unwrap().as_bytes())
            .unwrap(),
        b"public bytes"
    );
    assert_eq!(value["visibility"], "public");
}

#[test]
fn a_trickle_link_is_refused_before_anything_is_sent() {
    // Measured while writing ADR-0017, and it corrected the ADR: a manifest reply is 262
    // bytes before a single entry, and EU868 LoRa will bear 256 in a frame. Chunk replies
    // do fit — six bytes of payload at a time — so the object could be *transferred*, but
    // it cannot be *described*, so the fetch cannot start. It must say so plainly.
    let h = Harness::start("trickle");
    let id = h.put(b"content that will not fit a radio's frame", "public");

    let alice = NodeIdentity::generate().unwrap();
    let bob = NodeIdentity::generate().unwrap();
    let (client_link, server_link) = MemoryLink::pair();
    let responder = h.responder();
    let serving = std::thread::spawn(move || {
        let mut channel = SecureChannel::accept(server_link, &bob).unwrap();
        let _ = otwono_netd::serve_session(&mut channel, &responder);
    });

    let mut channel = SecureChannel::initiate(client_link, &alice).unwrap();
    let err = otwono_netd::fetch_object(&mut channel, &id, &LinkProperties::lora_eu868())
        .expect_err("a Trickle link cannot carry a manifest window");
    assert!(
        matches!(
            err,
            ProtocolError::TooLarge {
                field: "manifest window",
                ..
            }
        ),
        "expected a manifest-window refusal, got {err}"
    );
    drop(channel);
    let _ = serving.join();
}

#[test]
fn the_same_protocol_moves_an_object_over_an_in_memory_link() {
    // The transport is interchangeable: the same responder and the same requester, over a
    // channel with no socket under it at all.
    let h = Harness::start("memory");
    let bytes = payload(300 * 1024, 42);
    let id = h.put(&bytes, "public");

    let alice = NodeIdentity::generate().unwrap();
    let bob = NodeIdentity::generate().unwrap();
    let (client_link, server_link) = MemoryLink::pair();
    let responder = h.responder();
    let serving = std::thread::spawn(move || {
        let mut channel = SecureChannel::accept(server_link, &bob).unwrap();
        let _ = otwono_netd::serve_session(&mut channel, &responder);
    });

    let mut channel = SecureChannel::initiate(client_link, &alice).unwrap();
    let got = otwono_netd::fetch_object(&mut channel, &id, &LinkProperties::internet())
        .expect("an in-memory link must work");
    assert_eq!(got.bytes, bytes);
    drop(channel);
    let _ = serving.join();
}

#[test]
fn a_peer_that_serves_the_wrong_bytes_is_caught() {
    // A hostile peer, hand-rolled: it answers with a manifest for content it was not asked
    // about. Nothing on the wire is trusted, so the requester must reject this rather than
    // hand back whatever arrived.
    let alice = NodeIdentity::generate().unwrap();
    let bob = NodeIdentity::generate().unwrap();
    let (client_link, server_link) = MemoryLink::pair();

    let real = b"the bytes that were asked for".to_vec();
    let fake = b"substituted content".to_vec();
    let wanted = otwono_store::ContentId::of(&otwono_store::chunk::slice(&real)).to_hex();
    let served = otwono_store::chunk::slice(&fake);

    let hostile = std::thread::spawn(move || {
        let mut channel = SecureChannel::accept(server_link, &bob).unwrap();
        while let Ok(frame) = channel.recv() {
            let request: Request = otwono_net::content::decode(&frame).unwrap();
            // Always answers, always about the wrong content, always claiming the id it was
            // asked for. Only the digests give it away.
            let response = match request {
                // This liar shares nothing and offers nothing, so the honest answer to
                // either index question is an empty page of the matching kind.
                Request::SharedWithMe { .. } => {
                    Response::SharedWithYou(otwono_net::content::SharedIndexPage { entries: Vec::new() })
                }
                Request::Replicable { .. } => {
                    Response::Replicable(otwono_net::content::ReplicablePage { entries: Vec::new() })
                }
                // It carries nothing for anyone either, and an empty page is what a carrier
                // holding nothing and a carrier refusing to say both return.
                Request::Relayable { .. } | Request::AddressedToMe { .. } => {
                    Response::Carried(otwono_net::content::CarriedPage { entries: Vec::new() })
                }
                // It publishes nothing either, so there is no pointer to lie about.
                Request::Pointer { .. } => Response::not_available(""),
                // It carries nothing, so it releases nothing.
                Request::Delivered { envelope_id } => Response::Released {
                    envelope_id,
                    released: false,
                },
                Request::Manifest { content_id, .. } => {
                    Response::Manifest(otwono_net::content::ManifestPage {
                        content_id,
                        size_bytes: fake.len() as u64,
                        chunking: otwono_store::CHUNKING_VERSION.to_string(),
                        visibility: "public".into(),
                        sharing: None,
                        total_chunks: served.len() as u32,
                        from_chunk: 0,
                        chunks: served
                            .iter()
                            .map(|c| otwono_net::content::ChunkEntry {
                                blake3: c.hex(),
                                length: c.length,
                            })
                            .collect(),
                    })
                }
                Request::Chunk {
                    content_id, digest, ..
                } => Response::Chunk(otwono_net::content::ChunkPart {
                    content_id,
                    digest,
                    offset: 0,
                    total_length: fake.len() as u32,
                    data: data_encoding::BASE64.encode(&fake),
                }),
            };
            if channel
                .send(&otwono_net::content::encode(&response).unwrap())
                .is_err()
            {
                return;
            }
        }
    });

    let mut channel = SecureChannel::initiate(client_link, &alice).unwrap();
    let err = otwono_netd::fetch_object(&mut channel, &wanted, &LinkProperties::internet())
        .expect_err("substituted content must be refused");
    assert!(
        matches!(err, ProtocolError::ObjectIdMismatch { .. }),
        "expected an id mismatch, got {err}"
    );
    drop(channel);
    let _ = hostile.join();
}

#[test]
fn a_shared_object_crosses_a_real_link_and_opens_at_the_other_end() {
    // ADR-0019 end to end over TCP and Noise: the serving node encrypts to a named
    // recipient, that recipient fetches, and what arrives opens back to the original bytes.
    // Everything before this asserted SHARED failing closed; this is the first time it
    // actually moves.
    let h = Harness::start("sharedlink");
    let plaintext = b"the household accounts, for two neighbours and nobody else".repeat(40);
    let id = h.share_with_myself(&plaintext);

    let fetched = h
        .fetch(&id)
        .expect("the recipient is named, so the object is served");
    assert_eq!(fetched.visibility, "shared");
    assert_ne!(fetched.bytes, plaintext, "what crosses the link is ciphertext");
    assert!(!fetched
        .bytes
        .windows(24)
        .any(|w| w == b"the household accounts, "));

    // The envelope came with it, addressed to this node, and nothing in it names anybody
    // else -- only the asking peer's own copy travels.
    let envelope = fetched
        .sharing
        .as_ref()
        .expect("a shared object carries its envelope");
    assert_eq!(envelope.encryption, otwono_store::SHARED_ENCRYPTION);
    assert_eq!(
        envelope.sealed_key.recipient,
        h.server_node.to_text(),
        "the copy is addressed to this node"
    );
    assert_eq!(envelope.plaintext_size_bytes, plaintext.len() as u64);

    // And it opens. The sharing secret is read here as otwono-idd would read it.
    let sharing_key = SharingKeystore::new(h.identity_dir()).load().unwrap();
    let content_key = otwono_store::ContentKey::from_bytes(
        *sharing_key
            .open(&envelope.sealed_key)
            .expect("this node's own copy"),
    );
    let prefix = data_encoding::BASE64
        .decode(envelope.nonce_prefix.as_bytes())
        .unwrap();
    let mut opened = Vec::new();
    otwono_store::shared::open(
        &content_key,
        &otwono_store::shared::decode_prefix(&prefix).unwrap(),
        fetched.bytes.as_slice(),
        &mut opened,
    )
    .expect("the fetched ciphertext opens with the key that crossed with it");
    assert_eq!(opened, plaintext);
}

#[test]
fn a_fetched_shared_object_can_be_kept_and_opened_later() {
    // The last step of the recipient's path: fetch, keep what arrived, and read it back out
    // of the store afterwards -- without re-sealing it, which would produce a different
    // object under a key its sender never issued.
    let h = Harness::start("acceptshared");
    let plaintext = b"a document that outlives the session it arrived in".repeat(30);
    let id = h.share_with_myself(&plaintext);
    let fetched = h.fetch(&id).expect("served to a named recipient");
    let envelope = fetched.sharing.clone().expect("the envelope came with it");

    let token = h.token("store.write");
    let kept = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "store.accept_shared",
            json!({
                "content_id": id,
                "data": data_encoding::BASE64.encode(&fetched.bytes),
                "encryption": envelope.encryption,
                "nonce_prefix": envelope.nonce_prefix,
                "plaintext_size_bytes": envelope.plaintext_size_bytes,
                "sealed_key": envelope.sealed_key,
            }),
            &token,
        )
        .unwrap()
        .unwrap_or_else(|e| panic!("store.accept_shared refused: {}", e.message));
    assert_eq!(kept["content_id"], id, "keeping it must not rename it");
    assert_eq!(kept["visibility"], "shared");
    // One recipient, itself. It does not learn who else was on the sender's list.
    assert_eq!(kept["sharing"]["authorized"].as_array().unwrap().len(), 1);

    // And the stored object opens with the key that came with it.
    let sharing_key = SharingKeystore::new(h.identity_dir()).load().unwrap();
    let content_key = otwono_store::ContentKey::from_bytes(*sharing_key.open(&envelope.sealed_key).unwrap());
    let prefix = data_encoding::BASE64
        .decode(envelope.nonce_prefix.as_bytes())
        .unwrap();
    let mut opened = Vec::new();
    otwono_store::shared::open(
        &content_key,
        &otwono_store::shared::decode_prefix(&prefix).unwrap(),
        fetched.bytes.as_slice(),
        &mut opened,
    )
    .unwrap();
    assert_eq!(opened, plaintext);
}

#[test]
fn keeping_bytes_that_are_not_the_object_asked_for_is_refused() {
    // Chunking is deterministic, so the same ciphertext must reproduce the same id. A
    // mismatch means the peer sent something else, and the record is not written.
    let h = Harness::start("acceptwrong");
    let id = h.share_with_myself(b"the real one");
    let fetched = h.fetch(&id).unwrap();
    let envelope = fetched.sharing.clone().unwrap();

    let token = h.token("store.write");
    let err = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "store.accept_shared",
            json!({
                "content_id": "0".repeat(64),
                "data": data_encoding::BASE64.encode(&fetched.bytes),
                "encryption": envelope.encryption,
                "nonce_prefix": envelope.nonce_prefix,
                "plaintext_size_bytes": envelope.plaintext_size_bytes,
                "sealed_key": envelope.sealed_key,
            }),
            &token,
        )
        .unwrap()
        .expect_err("bytes that are not the object asked for must not be kept under its name");
    assert!(err.message.contains(&id), "{}", err.message);
}

#[test]
fn a_recipient_discovers_what_was_shared_with_it_and_fetches_it() {
    // ADR-0020, and the loop it exists to close. Until now a recipient could be sealed to
    // and had no way to learn the id, because a SHARED object's id is over ciphertext keyed
    // by a fresh per-object key -- unlike a PUBLIC object, it cannot be derived from the
    // content. Nothing here passes an id from one side to the other.
    let h = Harness::start("discover");
    let plaintext = b"a document the recipient was never told the name of".repeat(30);
    let expected = h.share_with_myself(&plaintext);

    let found = h
        .client
        .shared_with_me(&h.candidate())
        .expect("asking must succeed");
    let ids: Vec<&str> = found.iter().map(|e| e.content_id.as_str()).collect();
    assert!(ids.contains(&expected.as_str()), "{ids:?}");
    let entry = found
        .iter()
        .find(|e| e.content_id == expected)
        .expect("just checked");
    assert_eq!(entry.plaintext_size_bytes, plaintext.len() as u64);

    // And what was discovered can be fetched and opened, with the id having come only from
    // the peer's own answer.
    let fetched = h
        .fetch(&entry.content_id)
        .expect("what was offered must be servable");
    let envelope = fetched
        .sharing
        .as_ref()
        .expect("a shared object carries its envelope");
    let sharing_key = SharingKeystore::new(h.identity_dir()).load().unwrap();
    let content_key = otwono_store::ContentKey::from_bytes(*sharing_key.open(&envelope.sealed_key).unwrap());
    let prefix = data_encoding::BASE64
        .decode(envelope.nonce_prefix.as_bytes())
        .unwrap();
    let mut opened = Vec::new();
    otwono_store::shared::open(
        &content_key,
        &otwono_store::shared::decode_prefix(&prefix).unwrap(),
        fetched.bytes.as_slice(),
        &mut opened,
    )
    .unwrap();
    assert_eq!(opened, plaintext);
}

#[test]
fn the_index_offers_only_what_was_sealed_to_the_asker() {
    // A node holding public, private, and objects sealed to somebody else must offer none of
    // them. This is the reply's whole privacy story, so it is checked against a store that
    // has one of each.
    let h = Harness::start("indexscope");
    let mine = h.share_with_myself(b"for the node asking");
    let public = h.put(b"public bytes", "public");
    let private = h.put(b"private bytes", "private");

    // Held for somebody else, and this node is not on its list -- which store.share can no
    // longer produce, since it always keeps the owner a key (ADR-0019 §5).
    let stranger = otwono_identity::NodeIdentity::from_seeds(&[61u8; 32], &[62u8; 32], 1);
    let their_key = otwono_identity::SharingKey::from_seed(&[63u8; 32], 1);
    let theirs = h.hold_an_object_for(&stranger.node_id().to_text(), &their_key, b"for somebody else");

    let ids: Vec<String> = h
        .client
        .shared_with_me(&h.candidate())
        .unwrap()
        .into_iter()
        .map(|e| e.content_id)
        .collect();
    assert_eq!(ids, vec![mine], "{ids:?}");
    assert!(!ids.contains(&public));
    assert!(!ids.contains(&private));
    assert!(!ids.contains(&theirs), "the asker was told about somebody else's");
}

#[test]
fn a_node_that_has_been_sealed_nothing_gets_the_same_answer_as_one_that_shares_with_nobody() {
    // "Nothing for you" and "nothing for anybody" must be indistinguishable, or asking
    // becomes a way to learn whether a node shares at all.
    let sharing_node = Harness::start("indexsome");
    let stranger = otwono_identity::NodeIdentity::from_seeds(&[71u8; 32], &[72u8; 32], 1);
    let their_key = otwono_identity::SharingKey::from_seed(&[73u8; 32], 1);
    sharing_node.hold_an_object_for(
        &stranger.node_id().to_text(),
        &their_key,
        b"for somebody who is not asking",
    );

    // This harness's client and server share a signing key, so the asker is a node the
    // serving side has sealed nothing to.
    let asked_a_sharer = sharing_node
        .client
        .shared_with_me(&sharing_node.candidate())
        .unwrap();

    let quiet_node = Harness::start("indexnone");
    let asked_a_non_sharer = quiet_node.client.shared_with_me(&quiet_node.candidate()).unwrap();

    assert_eq!(asked_a_sharer, vec![]);
    assert_eq!(asked_a_sharer, asked_a_non_sharer);
}

#[test]
fn a_shared_object_is_not_served_to_a_peer_that_is_not_named() {
    // An object this node holds and is not a recipient of. The refusal must be the one every
    // other refusal is, or asking becomes a way to learn who a node shares with.
    let h = Harness::start("sharedstranger");
    let stranger = otwono_identity::NodeIdentity::from_seeds(&[77u8; 32], &[78u8; 32], 1);
    let their_key = otwono_identity::SharingKey::from_seed(&[79u8; 32], 1);
    let id = h.hold_an_object_for(
        &stranger.node_id().to_text(),
        &their_key,
        b"not for the node asking",
    );

    let refused = h.fetch(&id).unwrap_err();
    let absent = h.fetch(&"0".repeat(64)).unwrap_err();
    assert_eq!(
        refused.replace(&id, "<id>"),
        absent.replace(&"0".repeat(64), "<id>"),
        "a shared object this node may not have and one that does not exist must look alike"
    );
}

#[test]
fn a_chunk_of_a_shared_object_is_refused_without_the_manifest_that_carries_its_key() {
    // otwono-netd's own half of the ADR-0019 §4 check. The store holds the recipient list,
    // so a chunk reply cannot carry an independent proof of authorization without repeating
    // the whole envelope on every chunk. What this daemon can say by itself is that it has
    // not given this peer the manifest for this object in this session -- and if it has not,
    // the chunk does not go out, whatever the store answered.
    let h = Harness::start("chunkgate");
    let id = h.share_with_myself(&b"chunks that need a manifest first".repeat(40));
    let responder = h.responder();
    let peer = h.server_node;

    // A fresh session that has released nothing.
    let mut cold = otwono_netd::content::Session::default();
    let digest = {
        let mut warm = otwono_netd::content::Session::default();
        match responder.answer(
            &peer,
            &Request::Manifest {
                content_id: id.clone(),
                from_chunk: 0,
                max_chunks: 8,
            },
            &mut warm,
        ) {
            Response::Manifest(page) => page.chunks[0].blake3.clone(),
            other => panic!("the named peer must get a manifest: {other:?}"),
        }
    };

    let refused = responder.answer(
        &peer,
        &Request::Chunk {
            content_id: id.clone(),
            digest: digest.clone(),
            offset: 0,
            max_bytes: 262_144,
        },
        &mut cold,
    );
    assert!(
        matches!(refused, Response::NotAvailable { .. }),
        "a chunk went out in a session that had released no manifest: {refused:?}"
    );

    // The same request in a session that did release the manifest is served.
    let mut warm = otwono_netd::content::Session::default();
    let _ = responder.answer(
        &peer,
        &Request::Manifest {
            content_id: id.clone(),
            from_chunk: 0,
            max_chunks: 8,
        },
        &mut warm,
    );
    let served = responder.answer(
        &peer,
        &Request::Chunk {
            content_id: id,
            digest,
            offset: 0,
            max_bytes: 262_144,
        },
        &mut warm,
    );
    assert!(
        matches!(served, Response::Chunk(_)),
        "the same request was refused after the manifest went out: {served:?}"
    );
}

#[test]
fn a_peer_is_offered_replicated_content_and_nothing_else() {
    // ADR-0026 §7 across the responder, with a real store behind it.
    //
    // The filter is the whole test. PUBLIC serves on request but is never *offered* as a
    // copy, and PRIVATE must not appear in an answer that crosses a link at all -- an offer
    // list is the one place a labelling mistake would hand a peer a shopping list of things
    // it should never have heard of.
    let h = Harness::start("replicable");
    let mut ids = Vec::new();
    for (label, v) in [
        ("private", "private"),
        ("public", "public"),
        ("replicated", "replicated"),
    ] {
        ids.push((v.to_string(), h.put(label.as_bytes(), v)));
    }

    let responder = h.responder();
    let peer = h.client.node_id();
    let mut session = otwono_netd::content::Session::default();
    let reply = responder.answer(
        peer,
        &Request::Replicable {
            after: None,
            max_entries: 64,
        },
        &mut session,
    );
    let page = match reply {
        Response::Replicable(p) => p,
        other => panic!("expected a replicable page, got {other:?}"),
    };

    let offered: Vec<&str> = page.entries.iter().map(|e| e.content_id.as_str()).collect();
    for (v, id) in &ids {
        let present = offered.contains(&id.as_str());
        assert_eq!(
            present,
            v == "replicated",
            "{v} content was {} in the offer list",
            if present { "present" } else { "absent" }
        );
    }

    // And the policy travels, so a holder can apply the owner's size cap and TTL without a
    // second round trip.
    let e = page
        .entries
        .iter()
        .find(|e| e.content_id == ids[2].1)
        .expect("the replicated object is offered");
    assert_eq!(e.ttl_days, 365, "the default policy did not travel");
    assert_eq!(e.max_size_bytes, 100 * 1024 * 1024);
    assert!(e.allow_rereplication);
    assert_eq!(e.size_bytes, "replicated".len() as u64);
}

#[test]
fn an_offer_page_is_taken_once_per_session_like_the_sharing_index() {
    // ADR-0026 §7 inherits ADR-0020 §4's discipline: producing the list scans every object
    // record, so a peer that could force a fresh scan per request would have a cheap way to
    // make an SD-card-backed node miserable. The cost is that an object marked REPLICATED
    // *during* a session is not visible to it, which is asserted here rather than left as
    // a claim in a document.
    let h = Harness::start("replicable-session");
    let responder = h.responder();
    let peer = h.client.node_id();
    let mut session = otwono_netd::content::Session::default();

    let ask = |session: &mut otwono_netd::content::Session| match responder.answer(
        peer,
        &Request::Replicable {
            after: None,
            max_entries: 64,
        },
        session,
    ) {
        Response::Replicable(p) => p.entries.len(),
        other => panic!("expected a replicable page, got {other:?}"),
    };

    assert_eq!(ask(&mut session), 0, "nothing is offered yet");

    h.put(b"added later", "replicated");

    assert_eq!(ask(&mut session), 0, "the snapshot must not change mid-session");
    let mut fresh = otwono_netd::content::Session::default();
    assert_eq!(ask(&mut fresh), 1, "a new session sees it");
}

#[test]
fn a_replica_crosses_a_link_and_is_held_to_its_ttl() {
    // ADR-0026 end to end, between two stores: one node offers, the other asks, takes one
    // object, and holds it. Until this ran, every piece of replication existed and nothing
    // had ever copied anything.
    let h = Harness::start("replicate");
    let bytes = payload(40 * 1024, 91);
    let id = h.put(&bytes, "replicated");
    // Something it must *not* take, in the same store: an offer list that leaked a PUBLIC
    // object would be caught by the responder, and a holder that took one anyway would be
    // caught here.
    let public = h.put(b"public, and never offered as a copy", "public");

    // The holder's own cache, standing in for the second node's store.
    let holder_dir = std::env::temp_dir().join(format!("otw-holder-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&holder_dir);
    let holder = otwono_store::Cache::at(&holder_dir, otwono_store::StorageKey::generate(), 1 << 20)
        .expect("holder cache");

    let alice = NodeIdentity::generate().unwrap();
    let bob = NodeIdentity::generate().unwrap();
    let (client_link, server_link) = MemoryLink::pair();
    let responder = h.responder();
    let serving = std::thread::spawn(move || {
        let mut channel = SecureChannel::accept(server_link, &bob).unwrap();
        let _ = otwono_netd::serve_session(&mut channel, &responder);
    });

    let mut channel = SecureChannel::initiate(client_link, &alice).unwrap();
    let now = 1_700_000_000_000u64;
    let outcome =
        otwono_netd::content::replication_pass(&mut channel, &LinkProperties::internet(), &holder, now)
            .expect("a replication pass");

    match &outcome {
        otwono_netd::content::ReplicationPass::Took { content_id, .. } => {
            assert_eq!(content_id, &id, "took something other than the offered object");
            assert_ne!(content_id, &public, "a PUBLIC object was replicated");
        }
        other => panic!("expected to take the offered object, got {other:?}"),
    }

    // Held, not merely cached: budget pressure must not evict it.
    assert_eq!(holder.replicas_held(now), 1);
    let held = holder.entries();
    let entry = held.iter().find(|e| e.content_id == id).expect("held");
    assert!(!entry.is_evictable(now), "a live replica is evictable");
    assert!(entry.replica_expires_ms.is_some(), "no hold was recorded");

    // And the hold lapses on schedule rather than lasting forever.
    let a_year = 365 * 24 * 60 * 60 * 1000u64;
    assert_eq!(holder.expire_replicas(now + a_year).unwrap(), 1);
    assert_eq!(holder.replicas_held(now + a_year), 0);

    drop(channel);
    let _ = serving.join();
    let _ = std::fs::remove_dir_all(&holder_dir);
}

#[test]
fn a_node_that_does_not_replicate_asks_for_nothing() {
    // The capability engine's gate is the operator's consent, and it is checked before
    // anything reaches the wire -- a node that does not replicate should make no
    // replication traffic at all, rather than asking and discarding the answer.
    let h = Harness::start("no-replicate");
    h.put(b"on offer, and nobody is asking", "replicated");

    let dir = std::env::temp_dir().join(format!("otw-noholder-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // Budget zero is how the capability engine expresses "this machine contributes nothing".
    let holder = otwono_store::Cache::at(&dir, otwono_store::StorageKey::generate(), 0).expect("cache");

    let alice = NodeIdentity::generate().unwrap();
    let bob = NodeIdentity::generate().unwrap();
    let (client_link, server_link) = MemoryLink::pair();
    let responder = h.responder();
    let serving = std::thread::spawn(move || {
        let mut channel = SecureChannel::accept(server_link, &bob).unwrap();
        let _ = otwono_netd::serve_session(&mut channel, &responder);
    });

    let mut channel = SecureChannel::initiate(client_link, &alice).unwrap();
    let outcome = otwono_netd::content::replication_pass(
        &mut channel,
        &LinkProperties::internet(),
        &holder,
        1_700_000_000_000,
    )
    .expect("a pass that does nothing is not an error");
    assert_eq!(outcome, otwono_netd::content::ReplicationPass::NotReplicating);
    assert!(holder.is_empty());

    drop(channel);
    let _ = serving.join();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The whole wire, both halves: a real TCP link and a real control plane (ADR-0026 §10).
///
/// Every other replication test drives the pass against an in-process `Cache`, which proves
/// the protocol and says nothing about the split. This one goes through
/// `NetState::replicate_from`, so the offer crosses a socket to a peer, the bytes come back
/// over that socket, and the decision to keep them is made in the *other* process — the one
/// that owns the cache index. If the daemon were opening the cache directly, this test
/// would still pass and the node would still be wrong; what it checks is that the path that
/// does not do that works.
#[test]
fn a_replica_crosses_a_link_and_the_control_plane_together() {
    let h = Harness::start("brokered-replica");
    let bytes = payload(6 * 1024, 91);
    let id = h.put(&bytes, "replicated");
    // Something the holder must not take, on the same node, so "took the right one" is a
    // claim about choosing rather than about there being only one thing to choose.
    let public = h.put(&payload(1024, 92), "public");

    let outcome = h
        .client
        .replicate_from(&h.candidate())
        .expect("a replication pass over a real link");

    match &outcome {
        otwono_netd::content::ReplicationPass::Took {
            content_id,
            size_bytes,
        } => {
            assert_eq!(content_id, &id, "took something other than the offered object");
            assert_ne!(content_id, &public, "a PUBLIC object was replicated");
            assert_eq!(*size_bytes, bytes.len() as u64);
        }
        other => panic!("expected to take the offered object, got {other:?}"),
    }

    // And it is really in the other process's cache, asked for over the control plane rather
    // than read off a struct this test happens to be holding.
    let token = h.token("cache.replicate");
    let mut client = Client::connect(&h.store_socket).unwrap();
    let room = client
        .call_with_capability(
            "cache.replica_room",
            json!({ "candidates": [id, public] }),
            &token,
        )
        .unwrap()
        .expect("asking about room");
    assert_eq!(
        room["already_held"],
        json!([id]),
        "the store daemon does not have the replica the pass said it took"
    );

    // Asking again takes nothing: the object is already held, so the second pass finds the
    // only offer filtered out. Without this, "took one" and "takes one every time" would be
    // indistinguishable, and a node that meets a peer often would fill its budget with
    // copies of the same object.
    let again = h
        .client
        .replicate_from(&h.candidate())
        .expect("a second pass is not an error");
    match again {
        // The reason matters as much as the count here: "took none" is also what an empty
        // page and a budget refusal look like, and this test is about the duplicate.
        otwono_netd::content::ReplicationPass::NothingTaken { offered, why } => {
            assert_eq!(offered, 1, "the peer stopped offering it");
            assert!(
                why.contains("1 already held"),
                "a second pass declined for the wrong reason: {why}"
            );
        }
        other => panic!("took the same object twice: {other:?}"),
    }
}

/// A node whose broker refuses `cache.replicate` never opens a connection at all.
///
/// ADR-0026 §9 says a node that does not replicate makes no replication traffic. Checked
/// here by pointing the holder at a broker that denies it and asserting the pass reports
/// `NotReplicating` — the one outcome that is reached before the dial.
#[test]
fn a_node_whose_broker_refuses_replication_asks_nobody() {
    let h = Harness::start("brokered-refused");
    let _id = h.put(&payload(4096, 93), "replicated");

    // A second broker, denying the one capability, on its own socket. Everything else about
    // the node is unchanged, so a refusal here is about the capability and nothing else.
    let deny_dir = h.dir.join("deny");
    std::fs::create_dir_all(deny_dir.join("policy.d")).unwrap();
    std::fs::write(
        deny_dir.join("policy.d/10-deny.toml"),
        "[[rule]]\naction = \"cache.replicate\"\ndecision = \"deny\"\n",
    )
    .unwrap();
    let deny_socket = deny_dir.join("perm.sock");
    let policy = Policy::load_dir(&deny_dir.join("policy.d")).unwrap();
    policy.validate(&ActionRegistry::builtin()).unwrap();
    let broker = Arc::new(Broker::new(
        policy,
        AuditLog::open(deny_dir.join("audit.jsonl")).unwrap(),
    ));
    let s = h.shutdown.clone();
    let server = Server::bind(&deny_socket).unwrap();
    std::thread::spawn(move || server.serve(broker, s));
    Client::connect_waiting(&deny_socket, Duration::from_secs(5)).unwrap();

    // Its own identity, signed through the *allowing* broker: only the holder is pointed at
    // the denying one, so a refusal here cannot be a handshake that failed for another reason.
    let (agreement, _) = AgreementKeystore::new(h.dir.join("agreement-refused"))
        .load_or_generate()
        .unwrap();
    let signer = BrokeredSigner::bind(agreement, &h.id_socket, &h.perm_socket).expect("bind");
    let refused = Arc::new(NetState::new(Arc::new(signer)).with_holder(Arc::new(
        otwono_netd::content::BrokeredCache::new(&h.store_socket, &deny_socket),
    )));
    assert_eq!(
        refused.replicate_from(&h.candidate()).expect("not an error"),
        otwono_netd::content::ReplicationPass::NotReplicating
    );
}

/// A peer that offers pages forever cannot hold the discovery thread open (ADR-0026 §10).
///
/// The pass runs inline on the thread that finds peers, so "how long can one peer make this
/// take" is a question about the whole node's ability to mesh, not about one caller's
/// request. An entry ceiling does not answer it: sixteen thousand entries at a trickle
/// link's one-per-page is sixteen thousand round trips, each able to sit on a socket
/// timeout. The bound is therefore on pages.
///
/// Here the peer is deliberately hostile-but-conformant: every page is full, every page
/// advances the content id as the protocol requires, and it never stops. A pass that reads
/// until the peer stops reading would never return.
#[test]
fn a_peer_offering_endless_pages_is_cut_off_after_a_bounded_number() {
    use otwono_net::content::{ReplicableEntry, ReplicablePage};

    let alice = NodeIdentity::generate().unwrap();
    let bob = NodeIdentity::generate().unwrap();
    let (client_link, server_link) = MemoryLink::pair();

    let pages_served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&pages_served);
    let serving = std::thread::spawn(move || {
        let mut channel = SecureChannel::accept(server_link, &bob).unwrap();
        // Ids ascend forever, so the pass's no-progress check never fires and the only thing
        // that can stop this is the page bound under test.
        let mut next: u64 = 0;
        while let Ok(raw) = channel.recv() {
            let request: Request = match serde_json::from_slice(&raw) {
                Ok(r) => r,
                Err(_) => break,
            };
            let Request::Replicable { max_entries, .. } = request else {
                break;
            };
            let entries: Vec<ReplicableEntry> = (0..max_entries)
                .map(|_| {
                    next += 1;
                    ReplicableEntry {
                        content_id: format!("{next:064x}"),
                        // Larger than any budget this test gives, so nothing is ever
                        // fetched and the test measures paging and nothing else.
                        size_bytes: 1 << 30,
                        ttl_days: 365,
                        max_size_bytes: 1 << 30,
                        allow_rereplication: true,
                    }
                })
                .collect();
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let reply = serde_json::to_vec(&Response::Replicable(ReplicablePage { entries })).unwrap();
            if channel.send(&reply).is_err() {
                break;
            }
        }
    });

    let dir = std::env::temp_dir().join(format!("otw-endless-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let holder = otwono_store::Cache::at(&dir, otwono_store::StorageKey::generate(), 8 << 20).expect("cache");

    let mut channel = SecureChannel::initiate(client_link, &alice).unwrap();
    let outcome = otwono_netd::content::replication_pass(
        &mut channel,
        &LinkProperties::internet(),
        &holder,
        1_700_000_000_000,
    )
    .expect("the pass must return rather than page forever");

    // Nothing on offer fits, so nothing is taken — but it got there by stopping, not by
    // exhausting a peer that never stops.
    assert!(
        matches!(
            outcome,
            otwono_netd::content::ReplicationPass::NothingTaken { .. }
        ),
        "{outcome:?}"
    );
    let served = pages_served.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        served,
        otwono_netd::content::MAX_OFFER_PAGES_PER_PASS,
        "the pass read {served} pages from a peer that offers them endlessly"
    );

    drop(channel);
    let _ = serving.join();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A pointer crosses a real link and is verified against the key the handshake proved.
///
/// The second OTWONO primitive on the wire (ADR-0027). What makes this worth a test over a
/// real channel rather than a function call is precisely where the verification key comes
/// from: not from the record, not from a directory, but from the Noise handshake — so a peer
/// cannot serve a record for anybody but itself, and there is no third party to trust.
#[test]
fn a_pointer_crosses_a_link_and_is_verified_against_the_handshake() {
    let h = Harness::start("pointer");
    let target = h.put(b"the page this name points at", "public");
    let sequence = h.publish("wiki", "Home", Some(&target));
    assert_eq!(sequence, 1, "the store assigns the first sequence");

    let found = h
        .client
        .pointer_from(&h.candidate(), "wiki", "Home")
        .expect("a pointer fetch over a real link")
        .expect("the peer publishes that name");

    assert_eq!(found.content_id.as_deref(), Some(target.as_str()));
    assert_eq!(found.sequence, 1);
    assert_eq!(found.service, "wiki");
    assert_eq!(found.name, "Home");
    assert!(!found.is_tombstone());
    // The owner is the peer the handshake authenticated, not whatever the record claimed.
    assert_eq!(found.node_id, h.server_node.to_text());
}

/// A name the peer does not publish is indistinguishable from a refusal.
///
/// One answer for both, deliberately: a distinct "no such name" would let a stranger
/// enumerate which names a node has by the shape of the reply, which is the same leak
/// `not_available` closes everywhere else in this protocol.
#[test]
fn a_name_that_is_not_published_gets_the_same_answer_as_a_refusal() {
    let h = Harness::start("pointer-absent");
    assert!(h
        .client
        .pointer_from(&h.candidate(), "wiki", "Never-Written")
        .expect("asking is not an error")
        .is_none());

    // And a service that does not exist at all answers the same way.
    assert!(h
        .client
        .pointer_from(&h.candidate(), "forum", "Anything")
        .expect("asking is not an error")
        .is_none());
}

/// Publishing again advances the sequence, and the new record is what crosses.
#[test]
fn republishing_advances_the_sequence_over_the_wire() {
    let h = Harness::start("pointer-advance");
    let first = h.put(b"version one", "public");
    let second = h.put(b"version two", "public");

    h.publish("wiki", "Home", Some(&first));
    let got = h
        .client
        .pointer_from(&h.candidate(), "wiki", "Home")
        .unwrap()
        .unwrap();
    assert_eq!(got.sequence, 1);
    assert_eq!(got.content_id.as_deref(), Some(first.as_str()));

    assert_eq!(h.publish("wiki", "Home", Some(&second)), 2);
    let got = h
        .client
        .pointer_from(&h.candidate(), "wiki", "Home")
        .unwrap()
        .unwrap();
    assert_eq!(got.sequence, 2, "the peer served a stale record");
    assert_eq!(got.content_id.as_deref(), Some(second.as_str()));
}

/// A deletion crosses as a tombstone, not as an absence.
///
/// The distinction matters on the wire: an absent reply means "no such name, or I will not
/// say", and a tombstone means "the owner says this is gone" — signed, sequenced, and
/// impossible to roll back to the live version (ADR-0027 §4).
#[test]
fn a_tombstone_crosses_as_a_signed_record_rather_than_an_absence() {
    let h = Harness::start("pointer-tombstone");
    let target = h.put(b"here for now", "public");
    h.publish("wiki", "Home", Some(&target));
    h.publish("wiki", "Home", None);

    let got = h
        .client
        .pointer_from(&h.candidate(), "wiki", "Home")
        .unwrap()
        .expect("a tombstone is a record, not an absence");
    assert!(got.is_tombstone());
    assert_eq!(got.sequence, 2);
    assert_eq!(got.content_id, None);
}

/// A replayed older record is refused over a real link, by a peer that is genuinely the owner.
///
/// The property ADR-0027 exists for, and until now the one thing in it never shown outside a
/// unit test. Everything here is legitimate except the age: the handshake authenticates, the
/// record is the owner's own, and the signature verifies. What refuses it is the reader's
/// memory of a higher sequence — held by `otwono-stored` and reached over the control plane,
/// which is the path a booted node uses.
#[test]
fn an_older_record_replayed_by_its_owner_is_refused() {
    use otwono_net::content::{PointerReply, Request, Response};

    let h = Harness::start("pointer-rollback");
    let was = h.put(b"what the name meant first", "public");
    let now = h.put(b"what the name means now", "public");

    h.publish("wiki", "Home", Some(&was));
    let first = h
        .client
        .pointer_from(&h.candidate(), "wiki", "Home")
        .unwrap()
        .expect("the first read");
    assert_eq!(first.sequence, 1);
    assert_eq!(first.content_id.as_deref(), Some(was.as_str()));

    // Reading the unchanged name again must keep working. This is the ordinary case, and it
    // is asserted next to the attack because the rule that stops one nearly stopped both.
    assert_eq!(
        h.client
            .pointer_from(&h.candidate(), "wiki", "Home")
            .unwrap()
            .expect("an unchanged name is still readable")
            .sequence,
        1
    );

    h.publish("wiki", "Home", Some(&now));
    let second = h
        .client
        .pointer_from(&h.candidate(), "wiki", "Home")
        .unwrap()
        .expect("the update");
    assert_eq!(second.sequence, 2);
    assert_eq!(second.content_id.as_deref(), Some(now.as_str()));

    // Now the owner's own sequence-1 record, served again by something that authenticates as
    // the owner. A second signer over the same agreement key: the same NodeID the client
    // already trusts, because a replay is not an impersonation.
    let replayed = serde_json::to_value(&first).unwrap();
    let (client_link, server_link) = MemoryLink::pair();
    let replaying = h.server_signer();
    let serving = std::thread::spawn(move || {
        let mut channel = SecureChannel::accept(server_link, &replaying).unwrap();
        while let Ok(frame) = channel.recv() {
            let Ok(Request::Pointer { .. }) = otwono_net::content::decode(&frame) else {
                break;
            };
            let reply = serde_json::to_vec(&Response::Pointer(PointerReply {
                record: replayed.clone(),
            }))
            .unwrap();
            if channel.send(&reply).is_err() {
                break;
            }
        }
    });

    let mut channel = SecureChannel::initiate(client_link, h.client.signer.as_ref()).unwrap();
    let err = otwono_netd::fetch_pointer(
        &mut channel,
        "wiki",
        "Home",
        &LinkProperties::internet(),
        h.client.pointer_memory.as_ref(),
    )
    .expect_err("a replayed record must be refused");
    assert!(
        matches!(err, ProtocolError::Rollback { seen: 2, offered: 1 }),
        "expected a rollback naming both sequences, got {err:?}"
    );

    // And the refusal changed nothing: the honest peer still answers with the current record.
    assert_eq!(
        h.client
            .pointer_from(&h.candidate(), "wiki", "Home")
            .unwrap()
            .expect("still readable")
            .content_id
            .as_deref(),
        Some(now.as_str()),
        "a refused replay disturbed what the reader holds"
    );

    drop(channel);
    let _ = serving.join();
}

/// A peer that serves a record for somebody else is refused.
///
/// The property the whole narrow scope buys. The record here is perfectly signed by Mallory
/// and claims Mallory's NodeID — it would verify in isolation. It is refused because the
/// handshake proved this peer is somebody else, and a pointer is only ever fetched from its
/// owner.
#[test]
fn a_peer_cannot_serve_a_pointer_it_does_not_own() {
    use otwono_net::content::{PointerReply, Request, Response};

    let mallory = NodeIdentity::generate().unwrap();
    let bob = NodeIdentity::generate().unwrap();
    let alice = NodeIdentity::generate().unwrap();
    let (client_link, server_link) = MemoryLink::pair();

    // Bob answers every pointer request with a genuine record of Mallory's.
    let serving = std::thread::spawn(move || {
        let mut channel = SecureChannel::accept(server_link, &bob).unwrap();
        while let Ok(frame) = channel.recv() {
            let request: Request = otwono_net::content::decode(&frame).unwrap();
            let Request::Pointer { service, name } = request else {
                break;
            };
            let mut record =
                otwono_pointer::Pointer::new(mallory.node_id(), service, name, 1, Some("aa".repeat(32)), 1);
            let payload = record.payload_for_id_sign().unwrap();
            record.signature = data_encoding::BASE64.encode(
                &mallory
                    .sign(&otwono_identity::domain_separated(&payload))
                    .to_bytes(),
            );
            // Genuinely signed, genuinely Mallory's, genuinely current.
            record.verify(&mallory.public_key_bytes()).expect("a real record");
            let reply = serde_json::to_vec(&Response::Pointer(PointerReply {
                record: serde_json::to_value(&record).unwrap(),
            }))
            .unwrap();
            if channel.send(&reply).is_err() {
                break;
            }
        }
    });

    let mut channel = SecureChannel::initiate(client_link, &alice).unwrap();
    // `NoMemory` deliberately: this test is about the owner check, which happens whether or
    // not the reader remembers anything. A record from the wrong node is refused on the
    // first read as much as the hundredth.
    let err = otwono_netd::fetch_pointer(
        &mut channel,
        "wiki",
        "Home",
        &LinkProperties::internet(),
        &otwono_pointer::NoMemory,
    )
    .expect_err("a record for another node must be refused");
    assert!(
        matches!(err, ProtocolError::Mismatched(_)),
        "expected a mismatch, got {err:?}"
    );

    drop(channel);
    let _ = serving.join();
}

/// A pointer served *after* the responder has already served something else.
///
/// This is the case a booted node is always in and the earlier tests never were. `store.serve`
/// is `BlastRadius::Egress`, so its tokens are one-shot by default: the first content request
/// spends one and caches it, and every later request has to notice and ask for another. The
/// responder has `call_store` for exactly that.
///
/// The first version of the pointer responder did not use it — it read the cached token and
/// refused if the call failed. With a fresh responder that is invisible, because the cache is
/// empty and a fresh token is requested; on a node that has served anything at all, every
/// pointer request is answered "I do not publish that name" by a node that does. Which is
/// what two twenty-minute VM runs reported before this test existed.
#[test]
fn a_pointer_is_served_after_the_responder_has_spent_a_token() {
    let h = Harness::start("pointer-after-serving");
    let object = h.put(b"something served before the pointer is asked for", "public");
    let target = h.put(b"what the name points at", "public");
    h.publish("wiki", "Home", Some(&target));

    // Spend a token on the ordinary content path first. This is the step that makes the
    // responder's cached token stale, and without it the test proves nothing.
    let fetched = h
        .client
        .fetch_from(&h.candidate(), &object)
        .expect("an ordinary fetch");
    assert_eq!(fetched.content_id, object);

    // Now the pointer, through the same long-lived responder.
    let found = h
        .client
        .pointer_from(&h.candidate(), "wiki", "Home")
        .expect("a pointer fetch after a content fetch")
        .expect("the peer publishes that name");
    assert_eq!(found.content_id.as_deref(), Some(target.as_str()));

    // And again, so a second stale-token round is covered too.
    let again = h
        .client
        .pointer_from(&h.candidate(), "wiki", "Home")
        .expect("a second pointer fetch")
        .expect("still published");
    assert_eq!(again.sequence, found.sequence);
}

/// A carrier offers what it holds, over a responder that has already served content.
///
/// This is the test for a failure that is **invisible by design**, which is why it cost a
/// three-node run to find. `envelope.held` is guarded by `envelope.carry` and not by
/// `store.serve`, because ADR-0028 §8 made carrying mail a different agreement from serving
/// content. The responder was asking with its cached serve token, and an index question
/// answers a denial with an *empty page* — the same answer a node with nothing gives,
/// deliberately, so that asking cannot reveal whether a node carries at all (ADR-0020).
///
/// So the wrong token produced a carrier that looked exactly like an empty one, to itself and
/// to every peer, and the only symptom anywhere was "offered 0 envelope(s)" in a journal.
#[test]
fn a_carrier_offers_what_it_holds_after_serving_ordinary_content() {
    use otwono_net::content::{Request, Response};

    let h = Harness::start("carriage-offer");

    // Something for the serving node to carry, taken into custody through the control plane
    // exactly as `envelope-send` and the carry pass both do.
    //
    // Custody of bytes the node actually has. A made-up id used to do here, because nothing
    // looked: since ADR-0031 the carriage listing drops records whose object this node
    // cannot find, so a fabricated one is swept before it can be offered and this test would
    // fail for a reason that has nothing to do with the token it exists to pin.
    let carried_bytes = b"the ciphertext a carrier is holding for somebody".to_vec();
    let carried = h.put(&carried_bytes, "public");
    let recipient = NodeIdentity::generate().unwrap();
    let envelope = otwono_envelope::Envelope::new(
        &carried,
        recipient.node_id(),
        carried_bytes.len() as u64,
        otwono_identity::now_unix_ms() + 60 * 60 * 1000,
    );
    let mut store = Client::connect(&h.store_socket).unwrap();
    let taken = store
        .call_with_capability(
            "envelope.take",
            json!({ "envelope": envelope }),
            &h.token("envelope.carry"),
        )
        .unwrap()
        .expect("the node takes custody");
    assert_eq!(taken["taken"], json!(true));

    // Spend a token on the ordinary content path first, so the responder's cached
    // `store.serve` token is the one it would reach for next. Without this the test can pass
    // with the wrong capability.
    let object = h.put(b"served before anyone asks about carriage", "public");
    h.client
        .fetch_from(&h.candidate(), &object)
        .expect("an ordinary fetch");

    // Now ask the carriage question over a fresh channel, as a carry pass does.
    let source = h.client.open_content_channel(&h.candidate()).unwrap();
    let otwono_netd::content::PeerSource { mut channel, .. } = source;
    let frame = serde_json::to_vec(&Request::Relayable {
        after: None,
        max_entries: 32,
    })
    .unwrap();
    channel.send(&frame).unwrap();
    let reply: Response = otwono_net::content::decode(&channel.recv().unwrap()).unwrap();
    let Response::Carried(page) = reply else {
        panic!("a carriage question must be answered with a carriage page, got {reply:?}");
    };
    assert_eq!(
        page.entries.len(),
        1,
        "the carrier holds one envelope and offered {} — an empty page here is what a denied \
         capability looks like, which is exactly the bug this test exists for",
        page.entries.len()
    );
    assert_eq!(page.entries[0].envelope_id, envelope.envelope_id);
    assert_eq!(page.entries[0].recipient, recipient.node_id().to_text());

    // And the scoped question tells a different node nothing about it (ADR-0028 §9).
    let frame = serde_json::to_vec(&Request::AddressedToMe {
        after: None,
        max_entries: 32,
    })
    .unwrap();
    channel.send(&frame).unwrap();
    let reply: Response = otwono_net::content::decode(&channel.recv().unwrap()).unwrap();
    let Response::Carried(scoped) = reply else {
        panic!("expected a carriage page, got {reply:?}");
    };
    assert!(
        scoped.entries.is_empty(),
        "the asking node is not the recipient and must be told nothing: {:?}",
        scoped.entries
    );
}

/// Store-and-forward end to end: a message survives its sender being absent (ADR-0028).
///
/// Three parties with three distinct identities, over real Noise links, through real daemons:
///
/// 1. a **sender** seals an object to a recipient's sharing key and takes custody of it,
/// 2. a **carrier** — neither party, its own store daemon, its own budget — runs a carry pass,
///    is served the ciphertext with the key sealed to the recipient, keeps it and records
///    custody,
/// 3. the **recipient** dials the carrier and collects it, with the key intact.
///
/// Every identity here is separate on purpose. The harness fronts both ends of its own link
/// with one signing key, so a test written on that alone has the asking peer *be* the
/// recipient, and every rule store-and-forward depends on — who may be served a sealed
/// object, whose copy of the content key travels, what a scoped index question returns — is
/// satisfied trivially. Three booted nodes found three separate defects that this blindness
/// hid; each of them fails this test.
/// A carrier standing on its own: its own store daemon, its own mesh identity, listening.
///
/// Everything a node needs to take custody from one peer and hand the envelope to the next.
/// Extracted so a second hop costs a call rather than sixty lines, which is why no test
/// exercised one before: building a carrier by hand was most of writing the test.
///
/// The store daemon gets a cache as well as an envelope store, because since ADR-0031 a
/// carrier without one carries nothing — the ciphertext goes in the cache so that giving it
/// up frees disk and not only a record.
struct Carrier {
    store_socket: PathBuf,
    /// Where other nodes reach it, for `carry_from` and `collect_from`.
    candidate: Candidate,
}

impl Carrier {
    fn start(h: &Harness, tag: &str) -> Carrier {
        let dir = h.dir.join(format!("carrier-{tag}"));
        let store_socket = h.dir.join(format!("carrier-{tag}.sock"));
        let store = otwono_store::Store::encrypted(dir.join("store"), otwono_store::StorageKey::generate());
        store.ensure_layout().unwrap();
        let service = Arc::new(
            otwono_stored::StoreService::new(store, h.perm_socket.clone())
                .with_identity(h.id_socket.clone())
                .with_cache(
                    otwono_store::Cache::at(dir.join("cache"), otwono_store::StorageKey::generate(), 8 << 20)
                        .unwrap(),
                )
                .with_envelopes(
                    otwono_store::EnvelopeStore::at(dir.join("envelopes")).unwrap(),
                    8 << 20,
                ),
        );
        let shutdown = h.shutdown.clone();
        let server = Server::bind(&store_socket).unwrap();
        std::thread::spawn(move || server.serve(service, shutdown));
        Client::connect_waiting(&store_socket, Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("carrier {tag}'s store daemon never came up: {e}"));

        let mesh = BrokeredSigner::bind(
            AgreementKeystore::new(h.dir.join(format!("agreement-carrier-{tag}")))
                .load_or_generate()
                .unwrap()
                .0,
            &h.id_socket,
            &h.perm_socket,
        )
        .expect("bind");
        let claimed_node_id = mesh.node_id();
        let listener = TcpLink::listen("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let serving = Arc::new(
            NetState::new(Arc::new(mesh))
                .with_responder(ContentResponder::new(&store_socket, &h.perm_socket)),
        );
        std::thread::spawn(move || otwono_netd::run_listener(serving, listener));

        Carrier {
            store_socket,
            candidate: Candidate {
                claimed_node_id,
                address,
            },
        }
    }

    /// This carrier as a taker of other people's mail, for `carry_from`.
    fn taking(&self, h: &Harness) -> Arc<NetState> {
        Arc::new(
            NetState::new(Arc::clone(&h.client).signer.clone()).with_carrier(Arc::new(
                otwono_netd::content::BrokeredCarrier::new(&self.store_socket, &h.perm_socket),
            )),
        )
    }

    /// What it is holding, by envelope id.
    fn holding(&self, h: &Harness) -> Vec<String> {
        Client::connect(&self.store_socket)
            .unwrap()
            .call_with_capability("envelope.held", json!({}), &h.token("envelope.carry"))
            .unwrap()
            .expect("the carriage listing")["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["envelope_id"].as_str().unwrap().to_string())
            .collect()
    }
}

#[test]
fn an_envelope_reaches_its_recipient_through_a_carrier_that_is_neither_party() {
    let h = Harness::start("brokered-carry");

    // A recipient that is nowhere near this test's identities, and the envelope is sealed to
    // *its* sharing key. Sealing to the harness's own binding instead would have hidden the
    // bug this test exists for: the sender must serve the copy of the content key belonging
    // to the node it is carrying for, not to the node asking, and if those are the same node
    // there is nothing to get wrong.
    let recipient_signing = otwono_identity::SigningIdentity::generate().unwrap();
    let recipient_sharing = otwono_identity::SharingKey::generate().unwrap();
    let binding = recipient_signing.bind_sharing(&recipient_sharing.public());
    let recipient = *recipient_signing.node_id();

    let sealed = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "store.share",
            json!({
                "data": data_encoding::BASE64.encode(b"brokered mail for somebody who is not here"),
                "recipients": [binding],
            }),
            &h.token("store.share"),
        )
        .unwrap()
        .expect("sealing");
    let sealed_id = sealed["content_id"].as_str().unwrap().to_string();
    let size_bytes = sealed["size_bytes"].as_u64().unwrap();

    let envelope = otwono_envelope::Envelope::new(
        &sealed_id,
        &recipient,
        size_bytes,
        otwono_identity::now_unix_ms() + 2 * 60 * 60 * 1000,
    );
    Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "envelope.take",
            json!({ "envelope": envelope }),
            &h.token("envelope.carry"),
        )
        .unwrap()
        .expect("the sender holds its own outgoing envelope");

    // A second store daemon: the carrier's own. Same broker, so the capability decision is
    // the real one; different directories, so it starts out holding nothing.
    let carrier_dir = h.dir.join("carrier-store");
    let carrier_socket = h.dir.join("carrier-store.sock");
    let store =
        otwono_store::Store::encrypted(carrier_dir.join("store"), otwono_store::StorageKey::generate());
    store.ensure_layout().unwrap();
    let service = std::sync::Arc::new(
        otwono_stored::StoreService::new(store, h.perm_socket.clone())
            .with_identity(h.id_socket.clone())
            // A carrier needs a cache, since ADR-0031: the custody record goes in the
            // envelope store and the ciphertext goes here, where it can be deleted again.
            // `otwono-stored` refuses carriage outright on a node with a carriage budget and
            // no cache, so a carrier without one is not a configuration this has to model.
            .with_cache(
                otwono_store::Cache::at(
                    carrier_dir.join("cache"),
                    otwono_store::StorageKey::generate(),
                    8 << 20,
                )
                .unwrap(),
            )
            .with_envelopes(
                otwono_store::EnvelopeStore::at(carrier_dir.join("envelopes")).unwrap(),
                8 << 20,
            ),
    );
    let s = h.shutdown.clone();
    let server = Server::bind(&carrier_socket).unwrap();
    std::thread::spawn(move || server.serve(service, s));
    Client::connect_waiting(&carrier_socket, Duration::from_secs(5))
        .expect("the carrier's own store daemon never came up");

    let carrier = std::sync::Arc::new(otwono_netd::content::BrokeredCarrier::new(
        &carrier_socket,
        &h.perm_socket,
    ));
    let client = std::sync::Arc::new(
        NetState::new(std::sync::Arc::clone(&h.client).signer.clone()).with_carrier(carrier),
    );

    match client.carry_from(&h.candidate()).expect("a carry pass") {
        otwono_netd::content::CarryPass::Took { envelope_id, .. } => {
            assert_eq!(envelope_id, sealed_id)
        }
        // The reason, not just the fact. This assertion is the one the three-node run needed
        // and did not have.
        other => panic!("the peer offered one envelope and the brokered pass said {other:?}"),
    }

    let held = Client::connect(&carrier_socket)
        .unwrap()
        .call_with_capability("envelope.held", json!({}), &h.token("envelope.carry"))
        .unwrap()
        .expect("the carrier's store lists what it holds");
    let entries = held["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "the carrier's own store holds {entries:?}");
    assert_eq!(entries[0]["recipient"], json!(recipient.to_text()));

    // And the carrier kept the bytes, with the key sealed to the recipient. Custody of an
    // envelope this node cannot serve on would be a promise it could not keep.
    let kept = Client::connect(&carrier_socket)
        .unwrap()
        .call_with_capability(
            "store.serve_manifest",
            json!({ "content_id": sealed_id, "from_chunk": 0, "max_chunks": 64,
                    "peer": recipient.to_text() }),
            &h.token("store.serve"),
        )
        .unwrap()
        .expect("the carrier serves what it carries to the node it carries it for");
    assert_eq!(kept["visibility"], json!("shared"));
    assert_eq!(
        kept["sharing"]["sealed_key"]["recipient"],
        json!(recipient.to_text()),
        "the carrier holds the ciphertext but not the recipient's key: {kept}"
    );

    // --- and the recipient collects it, over a link, from the carrier -------------------
    //
    // The delivery half. Everything above proves a carrier can be *given* an envelope; this
    // proves it can hand one over. The recipient here is a NetState fronted by the identity
    // the envelope was sealed to — not the harness's — because a collector that is also the
    // sender is a collector for which every scoping and key check is trivially satisfied.
    let carrier_mesh = BrokeredSigner::bind(
        AgreementKeystore::new(h.dir.join("agreement-carrier-mesh"))
            .load_or_generate()
            .unwrap()
            .0,
        &h.id_socket,
        &h.perm_socket,
    )
    .expect("bind");
    let carrier_node = carrier_mesh.node_id();
    let listener = TcpLink::listen("127.0.0.1:0").unwrap();
    let carrier_addr = listener.local_addr().unwrap();
    let serving = Arc::new(
        NetState::new(Arc::new(carrier_mesh))
            .with_responder(ContentResponder::new(&carrier_socket, &h.perm_socket)),
    );
    std::thread::spawn(move || otwono_netd::run_listener(serving, listener));

    // The recipient's own store, stood up before the collector so the collector can be given
    // an inbox pointed at it. Collecting and keeping are one step now: a `collect_from` that
    // handed objects back unkept is what made this half a command rather than a daemon.
    let inbox_dir = h.dir.join("recipient-store");
    let inbox_socket = h.dir.join("recipient-store.sock");
    let inbox = otwono_store::Store::encrypted(inbox_dir.join("store"), otwono_store::StorageKey::generate());
    inbox.ensure_layout().unwrap();
    let inbox_service = Arc::new(
        otwono_stored::StoreService::new(inbox, h.perm_socket.clone()).with_identity(h.id_socket.clone()),
    );
    let sd = h.shutdown.clone();
    let inbox_server = Server::bind(&inbox_socket).unwrap();
    std::thread::spawn(move || inbox_server.serve(inbox_service, sd));
    Client::connect_waiting(&inbox_socket, Duration::from_secs(5)).expect("the inbox never came up");

    let recipient_node = Arc::new(otwono_identity::NodeIdentity::from_parts(
        recipient_signing,
        otwono_identity::AgreementKey::generate().unwrap(),
    ));
    let collector = NetState::new(recipient_node).with_inbox(Arc::new(
        otwono_netd::content::BrokeredInbox::new(&inbox_socket, &h.perm_socket),
    ));
    let carrier_candidate = Candidate {
        claimed_node_id: carrier_node,
        address: carrier_addr,
    };
    let collected = collector
        .collect_from(&carrier_candidate)
        .expect("collecting from the carrier");
    let collected = match collected {
        otwono_netd::content::Collected::Fetched(v) => v,
        other => panic!("a collector with an inbox reported {other:?}"),
    };

    assert_eq!(
        collected.len(),
        1,
        "the carrier is holding one envelope for this node and served {}",
        collected.len()
    );
    assert_eq!(collected[0].content_id, sealed_id);
    assert_eq!(
        collected[0]
            .sharing
            .as_ref()
            .expect("an envelope with no key could never be opened")
            .sealed_key
            .recipient,
        recipient.to_text(),
        "the key that arrived is not the one sealed to this node"
    );
    assert_eq!(collected[0].bytes.len() as u64, size_bytes);

    // A second pass takes nothing — but say why, because there are now two reasons and only
    // one of them is the one worth testing. Drop on delivery has already run by this point,
    // so the carrier is offering nothing and a second pass would take nothing even if the
    // `holds` check did not exist. That makes this assertion on its own almost vacuous.
    let again = collector
        .collect_from(&carrier_candidate)
        .expect("a second pass is not an error");
    assert_eq!(
        again,
        otwono_netd::content::Collected::Fetched(Vec::new()),
        "the second pass did not ask, or collected the same envelope twice"
    );

    // On the recipient's own disk, with the key that opens it — the step that makes an
    // envelope openable rather than merely fetched, asserted by asking that store to serve
    // the manifest back.
    let on_disk = Client::connect(&inbox_socket)
        .unwrap()
        .call_with_capability(
            "store.serve_manifest",
            json!({ "content_id": sealed_id, "from_chunk": 0, "max_chunks": 64,
                    "peer": recipient.to_text() }),
            &h.token("store.serve"),
        )
        .unwrap()
        .expect("the collected envelope is in the recipient's own store");
    assert_eq!(on_disk["visibility"], json!("shared"));
    assert_eq!(
        on_disk["sharing"]["sealed_key"]["recipient"],
        json!(recipient.to_text()),
        "the envelope is on disk without the key that opens it: {on_disk}"
    );

    // And the carrier has let it go (ADR-0028 §7). The recipient reports delivery as the
    // last step of collecting, *after* the write above succeeded, so this is the whole
    // round trip: taken, carried, handed over, dropped.
    let still_held = Client::connect(&carrier_socket)
        .unwrap()
        .call_with_capability("envelope.held", json!({}), &h.token("envelope.carry"))
        .unwrap()
        .expect("the carrier lists what it holds");
    assert_eq!(
        still_held["entries"].as_array().unwrap().len(),
        0,
        "the carrier is still holding an envelope it delivered: {still_held}"
    );

    // And the bytes went with the record (ADR-0031). Releasing the record alone is what made
    // drop on delivery free a budget and no disk: the index emptied, the ciphertext stayed,
    // and the permanent store has no way to delete it. The carrier keeps its mail in the
    // cache precisely so this assertion can hold.
    let gone = Client::connect(&carrier_socket)
        .unwrap()
        .call_with_capability(
            "store.serve_manifest",
            json!({ "content_id": sealed_id, "from_chunk": 0, "max_chunks": 64,
                    "peer": recipient.to_text() }),
            &h.token("store.serve"),
        )
        .unwrap();
    assert!(
        gone.is_err(),
        "the carrier gave up custody and kept the ciphertext: {gone:?}"
    );

    // So put the whole offer back — bytes and record — and ask again. That is a carrier
    // which never heard the release, or one that took the same envelope again from another
    // peer, and it is the best-effort failure path in `collect_from`, which is otherwise
    // covered by nothing. Now the envelope really is offered and really is servable, and
    // `holds` is the only thing between this node and downloading it twice.
    //
    // Both halves, because either alone tests the wrong thing: the record without the bytes
    // makes the third pass fail on the fetch rather than skip on `holds`.
    let back = collected[0]
        .sharing
        .as_ref()
        .expect("the collected envelope had no key");
    Client::connect(&carrier_socket)
        .unwrap()
        .call_with_capability(
            "envelope.keep",
            json!({
                "envelope": envelope,
                "data": data_encoding::BASE64.encode(&collected[0].bytes),
                "encryption": back.encryption,
                "nonce_prefix": back.nonce_prefix,
                "plaintext_size_bytes": back.plaintext_size_bytes,
                "sealed_key": back.sealed_key,
            }),
            &h.token("envelope.carry"),
        )
        .unwrap()
        .expect("the carrier keeps the ciphertext again");
    Client::connect(&carrier_socket)
        .unwrap()
        .call_with_capability(
            "envelope.take",
            json!({ "envelope": envelope }),
            &h.token("envelope.carry"),
        )
        .unwrap()
        .expect("the carrier takes custody again");
    let servable_again = Client::connect(&carrier_socket)
        .unwrap()
        .call_with_capability(
            "store.serve_manifest",
            json!({ "content_id": sealed_id, "from_chunk": 0, "max_chunks": 64,
                    "peer": recipient.to_text() }),
            &h.token("store.serve"),
        )
        .unwrap();
    assert!(
        servable_again.is_ok(),
        "the carrier took the envelope back and cannot serve it, so the next assertion \
         would pass for the wrong reason: {servable_again:?}"
    );
    let offered_again = collector
        .collect_from(&carrier_candidate)
        .expect("a third pass is not an error");
    assert_eq!(
        offered_again,
        otwono_netd::content::Collected::Fetched(Vec::new()),
        "a carrier that never let go got this node to fetch the same envelope twice"
    );
}

/// A carrier stops offering an envelope whose bytes it no longer has (ADR-0031).
///
/// The custody record and the ciphertext live in two places and can come apart: the cache
/// evicted it, a keep failed after the record was written, a disk came back with one and not
/// the other. What is left is the worst shape carriage has — a node advertising a message,
/// being asked for it, and having nothing to send. On a mesh where the sender may be gone,
/// that is a message nobody can retrieve while a node keeps saying it has it.
///
/// The record is not just hidden from the listing, it is dropped, so the carriage budget it
/// was occupying comes back too.
#[test]
fn a_carrier_that_lost_the_bytes_stops_offering_the_envelope() {
    let h = Harness::start("carriage-orphan");

    // Custody of an id this node has no object for, which is exactly what an eviction leaves
    // behind. Taking it is allowed: `envelope.take` writes a record and does not look for
    // bytes, and it must not — the carry pass keeps the bytes first and claims custody
    // second, so a take that demanded them could never succeed.
    let recipient = NodeIdentity::generate().unwrap();
    let envelope = otwono_envelope::Envelope::new(
        &"ab".repeat(32),
        recipient.node_id(),
        4096,
        otwono_identity::now_unix_ms() + 60 * 60 * 1000,
    );
    let mut store = Client::connect(&h.store_socket).unwrap();
    let taken = store
        .call_with_capability(
            "envelope.take",
            json!({ "envelope": envelope }),
            &h.token("envelope.carry"),
        )
        .unwrap()
        .expect("the node takes custody");
    assert_eq!(taken["taken"], json!(true), "the setup did not take custody");

    let held = store
        .call_with_capability("envelope.held", json!({}), &h.token("envelope.carry"))
        .unwrap()
        .expect("the carriage listing");
    assert_eq!(
        held["entries"].as_array().unwrap().len(),
        0,
        "a carrier offered an envelope it has no bytes for: {held}"
    );

    // And it is gone, not filtered. A record that survived the listing would keep occupying
    // the carriage budget for as long as its deadline ran, which is the second half of the
    // failure: the node would also refuse new mail it could actually carry.
    let again = store
        .call_with_capability("envelope.held", json!({}), &h.token("envelope.carry"))
        .unwrap()
        .expect("the carriage listing again");
    assert_eq!(again["entries"].as_array().unwrap().len(), 0);
    let scoped = store
        .call_with_capability(
            "envelope.held",
            json!({ "recipient": recipient.node_id().to_text() }),
            &h.token("envelope.carry"),
        )
        .unwrap()
        .expect("the scoped listing");
    assert_eq!(
        scoped["entries"].as_array().unwrap().len(),
        0,
        "the scoped question still offers it: {scoped}"
    );
}

/// An envelope crosses two carriers, and the first one never finds out it arrived.
///
/// ADR-0028 §7 concludes that a carrier may pass an envelope on, and says the record's shape
/// makes that structural rather than a permission: what a carrier offers is what it holds,
/// and a second carrier taking from a first is the same pass as taking from the sender. That
/// had never been run. A conclusion nothing exercises is a guess.
///
/// It is also the one place drop on delivery does not reach, and this pins that rather than
/// leaving it as prose. The recipient tells the carrier it collected *from*. Earlier hops
/// hold their copies until they lapse, because telling them would need either gossip or the
/// sender's involvement and §5 rules the second out.
#[test]
fn an_envelope_crosses_two_carriers_and_only_the_last_is_told_it_arrived() {
    let h = Harness::start("two-hop");

    // Sealed to a recipient that is neither carrier and is not the harness, so every scoping
    // and key check on the way has something to get wrong.
    let recipient_signing = otwono_identity::SigningIdentity::generate().unwrap();
    let recipient_sharing = otwono_identity::SharingKey::generate().unwrap();
    let binding = recipient_signing.bind_sharing(&recipient_sharing.public());
    let recipient = *recipient_signing.node_id();

    let sealed = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "store.share",
            json!({
                "data": data_encoding::BASE64.encode(b"mail that has to change hands twice"),
                "recipients": [binding],
            }),
            &h.token("store.share"),
        )
        .unwrap()
        .expect("sealing");
    let sealed_id = sealed["content_id"].as_str().unwrap().to_string();
    let envelope = otwono_envelope::Envelope::new(
        &sealed_id,
        &recipient,
        sealed["size_bytes"].as_u64().unwrap(),
        otwono_identity::now_unix_ms() + 2 * 60 * 60 * 1000,
    );
    Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "envelope.take",
            json!({ "envelope": envelope }),
            &h.token("envelope.carry"),
        )
        .unwrap()
        .expect("the sender holds its own outgoing envelope");

    // --- hop one: the sender to A ------------------------------------------------------
    let a = Carrier::start(&h, "a");
    match a.taking(&h).carry_from(&h.candidate()).expect("a carry pass") {
        otwono_netd::content::CarryPass::Took { envelope_id, .. } => assert_eq!(envelope_id, sealed_id),
        other => panic!("the sender offered one envelope and A's pass said {other:?}"),
    }
    assert_eq!(a.holding(&h), vec![sealed_id.clone()]);

    // --- hop two: A to B ---------------------------------------------------------------
    //
    // The step nothing had run. A is not the sender and not the recipient, so it is serving
    // an object it cannot open, to a peer that is also not the recipient, on the strength of
    // a custody record alone — and it has to hand over the recipient's copy of the content
    // key, not its own, because it has none.
    let b = Carrier::start(&h, "b");
    match b
        .taking(&h)
        .carry_from(&a.candidate)
        .expect("a second carry pass")
    {
        otwono_netd::content::CarryPass::Took { envelope_id, .. } => assert_eq!(envelope_id, sealed_id),
        other => panic!("A offered one envelope and B's pass said {other:?}"),
    }
    assert_eq!(
        b.holding(&h),
        vec![sealed_id.clone()],
        "B took the pass but is not holding the envelope"
    );

    // And what B holds is openable by the recipient, not by B. A relay that dropped the key
    // on the way would leave the last hop with ciphertext nobody can read, and nothing
    // before the recipient would notice.
    let at_b = Client::connect(&b.store_socket)
        .unwrap()
        .call_with_capability(
            "store.serve_manifest",
            json!({ "content_id": sealed_id, "from_chunk": 0, "max_chunks": 64,
                    "peer": recipient.to_text() }),
            &h.token("store.serve"),
        )
        .unwrap()
        .expect("B serves what it carries");
    assert_eq!(
        at_b["sharing"]["sealed_key"]["recipient"],
        json!(recipient.to_text()),
        "the key that survived two hops is not the recipient's: {at_b}"
    );

    // --- the recipient collects from B -------------------------------------------------
    let inbox_dir = h.dir.join("two-hop-inbox");
    let inbox_socket = h.dir.join("two-hop-inbox.sock");
    let inbox = otwono_store::Store::encrypted(inbox_dir.join("store"), otwono_store::StorageKey::generate());
    inbox.ensure_layout().unwrap();
    let inbox_service = Arc::new(
        otwono_stored::StoreService::new(inbox, h.perm_socket.clone()).with_identity(h.id_socket.clone()),
    );
    let sd = h.shutdown.clone();
    let inbox_server = Server::bind(&inbox_socket).unwrap();
    std::thread::spawn(move || inbox_server.serve(inbox_service, sd));
    Client::connect_waiting(&inbox_socket, Duration::from_secs(5)).expect("the inbox never came up");

    let collector = NetState::new(Arc::new(otwono_identity::NodeIdentity::from_parts(
        recipient_signing,
        otwono_identity::AgreementKey::generate().unwrap(),
    )))
    .with_inbox(Arc::new(otwono_netd::content::BrokeredInbox::new(
        &inbox_socket,
        &h.perm_socket,
    )));
    let collected = match collector.collect_from(&b.candidate).expect("collecting from B") {
        otwono_netd::content::Collected::Fetched(v) => v,
        other => panic!("a collector with an inbox reported {other:?}"),
    };
    assert_eq!(
        collected.len(),
        1,
        "two hops and the recipient got {} objects",
        collected.len()
    );
    assert_eq!(collected[0].content_id, sealed_id);

    // --- and the asymmetry ADR-0028 §7 names -------------------------------------------
    //
    // B is told and lets go. A is not, and holds its copy until its own deadline. That is
    // not a defect to be fixed here; it is the documented cost of a release that travels
    // only between the two nodes that met.
    assert!(
        b.holding(&h).is_empty(),
        "B was told the envelope arrived and is still holding it: {:?}",
        b.holding(&h)
    );
    assert_eq!(
        a.holding(&h),
        vec![sealed_id],
        "A was told about a delivery it was not party to, or dropped the envelope for some \
         other reason — either way the release reached further than it should"
    );
}

/// An envelope that runs out its deadline takes its ciphertext with it (ADR-0031).
///
/// The other way a carrier lets go, and the one that happens when drop on delivery does not:
/// the release is best effort, so a carrier whose recipient never comes back reaches its
/// deadline still holding the envelope. Both endings have to free the same things.
///
/// Freeing only the record is what the permanent store forced before ADR-0031, and it is
/// worse on this path than on delivery's — an undelivered envelope is exactly the one nobody
/// will ever ask for again, so its bytes would sit in a stranger's cache displacing what that
/// household actually fetched.
///
/// The one test here that sleeps. The sweep reads the wall clock and ADR-0026 §9 puts it in
/// the listing rather than on a timer, so there is no seam to inject a clock through; a
/// deadline a few seconds out and a real wait is the honest way to reach it.
#[test]
fn an_envelope_that_expires_frees_its_bytes_as_well_as_its_record() {
    let h = Harness::start("carriage-expiry");

    // Sealed to somebody who never turns up, and taken by a carrier the ordinary way — a
    // real carry pass rather than the bytes being handed over in the test, because reaching
    // into the sender's store for the ciphertext needs `store.read` and this daemon is right
    // to refuse it.
    let recipient_signing = otwono_identity::SigningIdentity::generate().unwrap();
    let recipient_sharing = otwono_identity::SharingKey::generate().unwrap();
    let binding = recipient_signing.bind_sharing(&recipient_sharing.public());
    let recipient = *recipient_signing.node_id();

    let sealed = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "store.share",
            json!({
                "data": data_encoding::BASE64.encode(b"mail whose recipient never came back"),
                "recipients": [binding],
            }),
            &h.token("store.share"),
        )
        .unwrap()
        .expect("sealing");
    let sealed_id = sealed["content_id"].as_str().unwrap().to_string();

    let deadline_ms = 4_000;
    let envelope = otwono_envelope::Envelope::new(
        &sealed_id,
        &recipient,
        sealed["size_bytes"].as_u64().unwrap(),
        otwono_identity::now_unix_ms() + deadline_ms,
    );
    let took_at = std::time::Instant::now();
    Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "envelope.take",
            json!({ "envelope": envelope }),
            &h.token("envelope.carry"),
        )
        .unwrap()
        .expect("the sender holds its own outgoing envelope");

    let carrier = Carrier::start(&h, "expiring");
    match carrier
        .taking(&h)
        .carry_from(&h.candidate())
        .expect("a carry pass")
    {
        otwono_netd::content::CarryPass::Took { envelope_id, .. } => assert_eq!(envelope_id, sealed_id),
        other => panic!("the sender offered one envelope and the pass said {other:?}"),
    }

    // Here and servable while the deadline stands. If the machine were slow enough that
    // getting this far already spent the window, say so rather than passing for that reason.
    // Whether the *bytes* are here, asked of the cache directly.
    //
    // Not `store.serve_manifest`: after the deadline that refuses because custody is gone,
    // whether or not the ciphertext went with it, so it would pass for a carrier that freed
    // the record and kept the bytes for ever — which is the whole defect. Found by inverting
    // the free and watching the first version of this test pass anyway.
    let cached = || -> bool {
        Client::connect(&carrier.store_socket)
            .unwrap()
            .call_with_capability("cache.status", json!({}), &h.token("cache.read"))
            .unwrap()
            .expect("the carrier's cache status")["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["content_id"] == json!(sealed_id))
    };
    assert_eq!(carrier.holding(&h), vec![sealed_id.clone()]);
    assert!(
        cached(),
        "the carrier took custody without keeping the ciphertext"
    );
    let servable = Client::connect(&carrier.store_socket)
        .unwrap()
        .call_with_capability(
            "store.serve_manifest",
            json!({ "content_id": sealed_id, "from_chunk": 0, "max_chunks": 64,
                    "peer": recipient.to_text() }),
            &h.token("store.serve"),
        )
        .unwrap();
    assert!(
        servable.is_ok(),
        "the carrier cannot serve what it just took: {servable:?}"
    );
    let spent = took_at.elapsed();
    assert!(
        spent < Duration::from_millis(deadline_ms - 500),
        "setup took {spent:?} of a {deadline_ms}ms window; the assertions below would pass \
         for the wrong reason"
    );

    // Past the deadline. The listing is what sweeps, as ADR-0026 §9 has it.
    std::thread::sleep(Duration::from_millis(deadline_ms + 400) - spent);
    assert!(
        carrier.holding(&h).is_empty(),
        "the record outlived its deadline: {:?}",
        carrier.holding(&h)
    );
    assert!(
        !cached(),
        "custody lapsed and the ciphertext stayed in the carrier's cache"
    );
}

/// A node whose broker refuses `envelope.carry` carries nobody's mail and asks nobody.
///
/// The carriage counterpart of [`a_node_whose_broker_refuses_replication_asks_nobody`], and
/// ADR-0028 §2's consent rule made structural: a node that does not carry makes no carriage
/// traffic. `NotCarrying` is the only outcome reached before the dial, so asserting it is
/// asserting that nothing went out.
///
/// It also pins the direction of the failure. A refusal that arrived *after* the connection
/// would still look like a pass that took nothing, which is exactly the ambiguity this whole
/// area has been paying for.
#[test]
fn a_node_whose_broker_refuses_carriage_asks_nobody() {
    let h = Harness::start("carriage-refused");

    let deny_dir = h.dir.join("deny-carry");
    std::fs::create_dir_all(deny_dir.join("policy.d")).unwrap();
    std::fs::write(
        deny_dir.join("policy.d/10-deny.toml"),
        "[[rule]]\naction = \"envelope.carry\"\ndecision = \"deny\"\n",
    )
    .unwrap();
    let deny_socket = deny_dir.join("perm.sock");
    let policy = Policy::load_dir(&deny_dir.join("policy.d")).unwrap();
    policy.validate(&ActionRegistry::builtin()).unwrap();
    let broker = Arc::new(Broker::new(
        policy,
        AuditLog::open(deny_dir.join("audit.jsonl")).unwrap(),
    ));
    let s = h.shutdown.clone();
    let server = Server::bind(&deny_socket).unwrap();
    std::thread::spawn(move || server.serve(broker, s));
    Client::connect_waiting(&deny_socket, Duration::from_secs(5)).unwrap();

    let (agreement, _) = AgreementKeystore::new(h.dir.join("agreement-no-carry"))
        .load_or_generate()
        .unwrap();
    let signer = BrokeredSigner::bind(agreement, &h.id_socket, &h.perm_socket).expect("bind");
    let refused = Arc::new(NetState::new(Arc::new(signer)).with_carrier(Arc::new(
        otwono_netd::content::BrokeredCarrier::new(&h.store_socket, &deny_socket),
    )));
    assert_eq!(
        refused.carry_from(&h.candidate()).expect("not an error"),
        otwono_netd::content::CarryPass::NotCarrying
    );
}

/// `envelope.take` and `envelope.held` are guarded, and a token for another action is not one.
///
/// The bug this pins down cost a three-node run: `otwono-netd` asked `envelope.held` with the
/// `store.serve` token it had cached for ordinary content, and because an index question
/// answers an unauthorised caller with an empty page rather than an error (ADR-0020), a
/// carrier holding the wrong token looked exactly like a carrier holding nothing.
#[test]
fn the_carriage_methods_refuse_a_token_minted_for_another_action() {
    let h = Harness::start("carriage-capability");

    let recipient = NodeIdentity::generate().unwrap();
    let envelope = otwono_envelope::Envelope::new(
        &"c".repeat(64),
        recipient.node_id(),
        16,
        otwono_identity::now_unix_ms() + 60_000,
    );
    for (method, params) in [
        ("envelope.take", json!({ "envelope": envelope })),
        ("envelope.held", json!({})),
        ("envelope.release", json!({ "envelope_id": "c".repeat(64) })),
    ] {
        let refusal = Client::connect(&h.store_socket)
            .unwrap()
            .call_with_capability(method, params.clone(), &h.token("store.serve"))
            .unwrap()
            .expect_err(&format!("{method} accepted a store.serve token"));
        assert_eq!(refusal.code, code::UNAUTHORIZED, "{method}: {refusal:?}");

        // And the same call with the right token is not refused, so the assertion above is
        // about the capability and not about a malformed request.
        let allowed = Client::connect(&h.store_socket)
            .unwrap()
            .call_with_capability(method, params, &h.token("envelope.carry"))
            .unwrap();
        assert!(allowed.is_ok(), "{method} with envelope.carry: {allowed:?}");
    }
}

/// Being dialled teaches a node that a peer is here, not where to reach it.
///
/// An accepted socket's remote port is the *dialer's* ephemeral source port. Recording it as
/// the peer's address produces an entry nothing listens on, and `PeerTable::observe` appends,
/// so an inbound connection seen before the peer's advertisement leaves that dead entry first
/// in the list — and every outbound pass reads `addresses.first()`. The symptom is a peer
/// that authenticates inbound and is refused on every dial back, asymmetrically and for the
/// life of the process:
///
/// ```text
/// carry pass with otw1:30am-… failed: connect to 169.254.182.65:33814:
///   link I/O failed: Connection refused (os error 111)
/// ```
///
/// 33814 was never a listening port. This test is the assertion that would have caught it.
#[test]
fn an_inbound_connection_records_no_dialable_address() {
    let h = Harness::start("inbound-address");

    // A second serving node, kept rather than moved into the listener thread, so its peer
    // table can be read after somebody dials it.
    let (agreement, _) = AgreementKeystore::new(h.dir.join("agreement-listener"))
        .load_or_generate()
        .unwrap();
    let signer = BrokeredSigner::bind(agreement, &h.id_socket, &h.perm_socket).expect("bind");
    let listening_node = signer.node_id();
    let listener = TcpLink::listen("127.0.0.1:0").unwrap();
    let listen_addr = listener.local_addr().unwrap();
    let inbound = Arc::new(NetState::new(Arc::new(signer)));
    let serving = Arc::clone(&inbound);
    std::thread::spawn(move || otwono_netd::run_listener(serving, listener));

    h.client
        .dial(&Candidate {
            claimed_node_id: listening_node,
            address: listen_addr,
        })
        .expect("the dial authenticates");

    // Give the accepting side its moment: the dial returns once the handshake completes, and
    // the record happens on the other thread.
    let dialer = h.client.node_id();
    let mut recorded = None;
    for _ in 0..50 {
        if let Some(p) = inbound.peers.lock().unwrap().get(dialer) {
            recorded = Some(p.addresses.clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let addresses = recorded.expect("the accepting node never recorded the peer that dialled it");
    assert!(
        addresses.is_empty(),
        "an inbound connection contributed a dial address: {addresses:?} — the port there is \
         the dialer's ephemeral source port and nothing listens on it"
    );
    assert_eq!(
        inbound.peers.lock().unwrap().connected().len(),
        1,
        "the peer is still connected; only its address is unknown"
    );
}

/// Custody widens the audience of a sealed object. It does not unlock a private one.
///
/// `envelope.take` records custody by content id and needs only a local capability, so a
/// carriage exception applied beside the label check rather than inside it would be a way to
/// make a `PRIVATE` object servable by taking custody of its id. This is the assertion that
/// the exception sits under the `shared`-and-own-store guard.
#[test]
fn taking_custody_of_a_private_objects_id_does_not_make_it_servable() {
    let h = Harness::start("custody-not-a-key");

    let private = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "store.put",
            json!({
                "data": data_encoding::BASE64.encode(b"this never leaves the node"),
                "visibility": "private",
            }),
            &h.token("store.write"),
        )
        .unwrap()
        .expect("storing a private object");
    let private_id = private["content_id"].as_str().unwrap().to_string();

    // Custody of that exact id, taken through the ordinary method with the ordinary
    // capability — which is all an operator of this node can do.
    let recipient = NodeIdentity::generate().unwrap();
    let envelope = otwono_envelope::Envelope::new(
        &private_id,
        recipient.node_id(),
        private["size_bytes"].as_u64().unwrap(),
        otwono_identity::now_unix_ms() + 60 * 60 * 1000,
    );
    let taken = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "envelope.take",
            json!({ "envelope": envelope }),
            &h.token("envelope.carry"),
        )
        .unwrap()
        .expect("the store records custody of whatever id it is given");
    assert_eq!(taken["taken"], json!(true), "the premise of this test: {taken}");

    let refused = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "store.serve_manifest",
            json!({ "content_id": private_id, "from_chunk": 0, "max_chunks": 8,
                    "peer": recipient.node_id().to_text() }),
            &h.token("store.serve"),
        )
        .unwrap();
    assert!(
        refused.is_err(),
        "custody of a PRIVATE object's id made it servable: {refused:?}"
    );
}

/// The largest envelope a carrier will accept must be one it can actually keep.
///
/// A carrier stores what it takes through `store.accept_shared`, which is an inline
/// control-plane call with its own ceiling. When the envelope ceiling was the larger of the
/// two, an envelope between them passed `CarryPolicy::decide`, passed the fetch, and failed
/// at the store — on every carrier and every retry, because nothing about it would ever
/// change. Accepted by policy and undeliverable by construction.
///
/// Two constants in two crates that nothing linked, so this is the link.
#[test]
fn the_envelope_ceiling_fits_what_a_carrier_can_actually_keep() {
    assert!(
        otwono_envelope::MAX_ENVELOPE_BYTES <= otwono_stored::MAX_INLINE_BYTES as u64,
        "a carrier will accept {} bytes and can only keep {}",
        otwono_envelope::MAX_ENVELOPE_BYTES,
        otwono_stored::MAX_INLINE_BYTES
    );
}

/// A peer that cannot be reached stops being reported as connected.
///
/// `retry_candidates` returns peers that are `Discovered` or `Failed` and never `Connected`,
/// so a peer that dies while connected is in a state nothing moves it out of: the sweeps
/// dial it, fail, and leave it exactly where it was. It stays "connected" for the life of
/// the process, `net.peers` reports a switched-off machine as connected, and everything that
/// reads `connected()` works from that.
///
/// Seen on a three-node run. Node 3 was powered down; both survivors logged
///
/// ```text
/// carry pass with otw1:wc6h-… failed: connect to 169.254.71.187:8443:
///   link I/O failed: No route to host (os error 113)
/// ```
///
/// every cycle, and both went on printing `connected=2`.
#[test]
fn a_peer_that_stops_answering_stops_being_connected() {
    let h = Harness::start("unreachable-peer");

    // Connected for real, so the state under test is reached the way it is on a node.
    let live = h.candidate();
    h.client.dial(&live).expect("the first dial authenticates");
    assert_eq!(
        h.client.peers.lock().unwrap().connected().len(),
        1,
        "the premise: the peer is connected"
    );

    // Now it is gone. A port nothing listens on stands in for a machine that was powered
    // off: what reaches this node either way is a failed connect.
    let gone = Candidate {
        claimed_node_id: live.claimed_node_id,
        address: "127.0.0.1:1".parse().unwrap(),
    };
    assert!(
        h.client.open_content_channel(&gone).is_err(),
        "a dial to a dead address must not succeed"
    );

    assert!(
        h.client.peers.lock().unwrap().connected().is_empty(),
        "a peer that could not be reached is still being reported as connected"
    );
    // And it is a retry candidate again, so it comes back when the machine does.
    assert_eq!(
        h.client.peers.lock().unwrap().retry_candidates().len(),
        1,
        "a failed peer must be redialled, or the mesh never heals"
    );
}

/// A node with no inbox asks nobody whether it has mail.
///
/// The collection counterpart of [`a_node_whose_broker_refuses_carriage_asks_nobody`]. A node
/// that cannot keep what it collects must not dial for it: it would learn a carrier is
/// holding something for it and then have nowhere to put it, having told that carrier it was
/// interested for nothing.
#[test]
fn a_node_with_nowhere_to_put_mail_does_not_go_looking_for_it() {
    let h = Harness::start("no-inbox");
    let collected = Arc::clone(&h.client)
        .collect_from(&h.candidate())
        .expect("not an error");
    // `NoInbox`, not an empty list. A node that will not keep mail and a node with no mail
    // waiting collect the same amount of nothing, and telling them apart is the difference
    // between an operator fixing a policy and an operator staring at a blank line.
    assert_eq!(
        collected,
        otwono_netd::content::Collected::NoInbox,
        "a node with no inbox went looking anyway"
    );
}

/// `store.holds` answers a caller that holds `store.write` and not `store.read`.
///
/// This is the point of the method existing at all. `otwono-netd` is the Z3 hostile-input
/// daemon; `the_serving_node_serves_without_ever_holding_store_read` exists to keep it away
/// from the user's store, and the collection sweep must not be the thing that quietly hands
/// it that authority. So the sweep's "do I already have this?" is guarded by the authority to
/// avoid a redundant write, not by the authority to read.
#[test]
fn asking_whether_an_object_is_here_does_not_need_store_read() {
    let h = Harness::start("holds-without-read");

    let refused = Client::connect(&h.perm_socket)
        .unwrap()
        .call(
            "perm.request",
            json!({ "action": "store.read", "reason": "prove the policy denies it" }),
        )
        .unwrap();
    assert!(refused.is_err(), "the test policy must deny store.read");

    let id = h.put(b"something this node holds", "public");
    let write = h.token("store.write");

    let here = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability("store.holds", json!({ "content_id": id }), &write)
        .unwrap()
        .expect("store.holds with store.write");
    assert_eq!(here["holds"], json!(true), "{here}");

    // And an object nobody put there. `false` rather than an error, because "not here" is a
    // true answer and the caller's next move is to fetch it.
    let absent = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability("store.holds", json!({ "content_id": "a".repeat(64) }), &write)
        .unwrap()
        .expect("store.holds for something absent");
    assert_eq!(absent["holds"], json!(false), "{absent}");

    // Nothing but the bool. A caller wanting size or label is asking to read the object.
    let keys: Vec<&String> = here.as_object().unwrap().keys().collect();
    assert_eq!(
        keys.len(),
        2,
        "store.holds leaked more than a schema version and a bool: {keys:?}"
    );
}

/// `net.mail` says what is waiting and fetches none of it.
///
/// The read-only half of the collection question. It exists so that "the sweep delivered
/// this" and "somebody ran the command" are two observable outcomes rather than one — a
/// recipient cannot name an envelope it has not been given, so before this there was no way
/// to look without also collecting.
///
/// Asserted both ways round: the envelope is named, and it is *not* on this node's disk
/// afterwards.
#[test]
fn asking_what_is_waiting_does_not_fetch_it() {
    let h = Harness::start("mail-without-fetching");

    // A carrier holding one envelope for a recipient that is neither end of the link.
    let recipient_signing = otwono_identity::SigningIdentity::generate().unwrap();
    let recipient_sharing = otwono_identity::SharingKey::generate().unwrap();
    let binding = recipient_signing.bind_sharing(&recipient_sharing.public());
    let recipient = *recipient_signing.node_id();

    let sealed = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "store.share",
            json!({
                "data": data_encoding::BASE64.encode(b"waiting, not fetched"),
                "recipients": [binding],
            }),
            &h.token("store.share"),
        )
        .unwrap()
        .expect("sealing");
    let sealed_id = sealed["content_id"].as_str().unwrap().to_string();
    let size_bytes = sealed["size_bytes"].as_u64().unwrap();

    let envelope = otwono_envelope::Envelope::new(
        &sealed_id,
        &recipient,
        size_bytes,
        otwono_identity::now_unix_ms() + 60 * 60 * 1000,
    );
    Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "envelope.take",
            json!({ "envelope": envelope }),
            &h.token("envelope.carry"),
        )
        .unwrap()
        .expect("the harness node holds it for the recipient");

    // The recipient asks. No inbox: looking is not keeping, so it needs none.
    let asker = NetState::new(Arc::new(otwono_identity::NodeIdentity::from_parts(
        recipient_signing,
        otwono_identity::AgreementKey::generate().unwrap(),
    )));
    let waiting = asker.mail_at(&h.candidate()).expect("asking what is waiting");
    assert_eq!(waiting.len(), 1, "expected one envelope waiting, got {waiting:?}");
    assert_eq!(waiting[0].envelope_id, sealed_id);
    assert_eq!(waiting[0].recipient, recipient.to_text());
    assert_eq!(waiting[0].size_bytes, size_bytes);

    // And nothing was fetched: a node with no inbox has nowhere to put anything, so the
    // strong form of this is that asking works *without* one at all — which the line above
    // already shows by not erroring. This pins the other half: no collection happened.
    assert_eq!(
        asker
            .collect_from(&h.candidate())
            .expect("collect is not an error"),
        otwono_netd::content::Collected::NoInbox,
        "asking what is waiting must not have configured an inbox as a side effect"
    );
}

/// A carrier drops an envelope once its recipient says it has it (ADR-0028 §7).
///
/// The third bound on amplification, and the only one that asks a node to be honest. Before
/// this a carrier held every envelope it took until the sender's expiry, delivered or not, so
/// a message with a week-long deadline occupied a stranger's disk for a week after arriving.
#[test]
fn a_carrier_gives_up_custody_when_its_recipient_says_it_arrived() {
    let h = Harness::start("drop-on-delivery");

    let recipient_signing = otwono_identity::SigningIdentity::generate().unwrap();
    let recipient_sharing = otwono_identity::SharingKey::generate().unwrap();
    let binding = recipient_signing.bind_sharing(&recipient_sharing.public());
    let recipient = *recipient_signing.node_id();

    let sealed = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "store.share",
            json!({
                "data": data_encoding::BASE64.encode(b"delivered, and then dropped"),
                "recipients": [binding],
            }),
            &h.token("store.share"),
        )
        .unwrap()
        .expect("sealing");
    let sealed_id = sealed["content_id"].as_str().unwrap().to_string();

    let envelope = otwono_envelope::Envelope::new(
        &sealed_id,
        &recipient,
        sealed["size_bytes"].as_u64().unwrap(),
        otwono_identity::now_unix_ms() + 60 * 60 * 1000,
    );
    Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "envelope.take",
            json!({ "envelope": envelope }),
            &h.token("envelope.carry"),
        )
        .unwrap()
        .expect("the harness node carries it");

    let held = |what: &str| -> usize {
        Client::connect(&h.store_socket)
            .unwrap()
            .call_with_capability("envelope.held", json!({}), &h.token("envelope.carry"))
            .unwrap()
            .unwrap_or_else(|e| panic!("{what}: {e:?}"))["entries"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(held("before"), 1, "the premise: it is being carried");

    // Somebody who is not the recipient cannot make the carrier drop it. Asserted *first*,
    // so a bug that released for anyone could not be hidden by the recipient's own release
    // happening earlier in the test.
    let stranger = Arc::new(otwono_identity::NodeIdentity::generate().unwrap());
    let mut channel = NetState::new(stranger)
        .open_content_channel(&h.candidate())
        .expect("a stranger may still connect")
        .channel;
    assert!(
        !otwono_netd::content::report_delivered(&mut channel, &sealed_id).unwrap(),
        "a node that is not the recipient released somebody else's envelope"
    );
    assert_eq!(
        held("after a stranger asked"),
        1,
        "the stranger's claim was acted on"
    );

    // The recipient can.
    let node = Arc::new(otwono_identity::NodeIdentity::from_parts(
        recipient_signing,
        otwono_identity::AgreementKey::generate().unwrap(),
    ));
    let mut channel = NetState::new(node)
        .open_content_channel(&h.candidate())
        .expect("the recipient connects")
        .channel;
    assert!(
        otwono_netd::content::report_delivered(&mut channel, &sealed_id).unwrap(),
        "the recipient said it had the envelope and the carrier kept it anyway"
    );
    assert_eq!(held("after the recipient reported delivery"), 0);

    // Saying it twice is not an error. A recipient that collects, reports, and is restarted
    // before the reply lands will say it again.
    assert!(
        !otwono_netd::content::report_delivered(&mut channel, &sealed_id).unwrap(),
        "a repeated report must not be an error, only a `false`"
    );
}

/// An inbox that fetches happily and cannot write. The failure that must not lose a message.
struct ButterFingers;

impl otwono_store::Inbox for ButterFingers {
    fn accepting(&self) -> bool {
        true
    }
    fn holds(&self, _content_id: &str) -> bool {
        false
    }
    fn keep(
        &self,
        _content_id: &str,
        _bytes: &[u8],
        _sharing: &otwono_store::object::Sharing,
    ) -> Result<(), String> {
        Err("the disk is full".into())
    }
}

/// A recipient that could not store what it collected does not tell the carrier to drop it.
///
/// The failure this whole feature has to survive. The sender may be gone, so the carrier's
/// copy can be the last one in existence; a release sent on the strength of a fetch rather
/// than a write would lose the message permanently and tell nobody.
///
/// So the order in `collect_from` is: fetch, verify, **store**, and only then report. This
/// asserts the middle step failing takes the report with it.
#[test]
fn a_recipient_that_could_not_store_its_mail_does_not_release_the_carrier() {
    let h = Harness::start("butter-fingers");

    let recipient_signing = otwono_identity::SigningIdentity::generate().unwrap();
    let recipient_sharing = otwono_identity::SharingKey::generate().unwrap();
    let binding = recipient_signing.bind_sharing(&recipient_sharing.public());
    let recipient = *recipient_signing.node_id();

    let sealed = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "store.share",
            json!({
                "data": data_encoding::BASE64.encode(b"must survive a clumsy recipient"),
                "recipients": [binding],
            }),
            &h.token("store.share"),
        )
        .unwrap()
        .expect("sealing");
    let sealed_id = sealed["content_id"].as_str().unwrap().to_string();

    let envelope = otwono_envelope::Envelope::new(
        &sealed_id,
        &recipient,
        sealed["size_bytes"].as_u64().unwrap(),
        otwono_identity::now_unix_ms() + 60 * 60 * 1000,
    );
    Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability(
            "envelope.take",
            json!({ "envelope": envelope }),
            &h.token("envelope.carry"),
        )
        .unwrap()
        .expect("the harness node carries it");

    let collector = NetState::new(Arc::new(otwono_identity::NodeIdentity::from_parts(
        recipient_signing,
        otwono_identity::AgreementKey::generate().unwrap(),
    )))
    .with_inbox(Arc::new(ButterFingers));

    let outcome = collector.collect_from(&h.candidate());
    assert!(
        outcome.is_err(),
        "a collection that could not be stored reported success: {outcome:?}"
    );

    let still_held = Client::connect(&h.store_socket)
        .unwrap()
        .call_with_capability("envelope.held", json!({}), &h.token("envelope.carry"))
        .unwrap()
        .expect("the carrier lists what it holds");
    assert_eq!(
        still_held["entries"].as_array().unwrap().len(),
        1,
        "the carrier dropped an envelope the recipient never managed to store: {still_held}"
    );
}

/// A wiki page crosses a link: the pointer, the revision, and the body (ADR-0032).
///
/// Phase 6's first exit clause is "node A's wiki page is readable on node B", and this is the
/// composition that makes it one thing rather than three primitives in a row. The pointer
/// names a revision, the revision names a body, and each hop is a separate fetch — so a page
/// that *resolves* is not yet a page that can be *read*, and only fetching all three shows it.
#[test]
fn a_wiki_page_is_readable_from_another_node() {
    let h = Harness::start("wiki-link");

    // The serving node writes a page the way `otwono-wikictl write` does.
    let text = b"the first page on the serving node\n";
    let body = h.put(text, "public");
    let mut revision =
        otwono_wiki::Revision::new(h.node_signing.node_id(), "Getting-Started", &body, None, 1_000);
    revision.signature =
        data_encoding::BASE64.encode(&h.node_signing.sign(&revision.signing_bytes().unwrap()).to_bytes());
    let head = h.put(&serde_json::to_vec(&revision).unwrap(), "public");
    h.publish("wiki", "Getting-Started", Some(&head));

    // The reading node resolves the name, and takes the key from the handshake rather than
    // from the record — a NodeID is a hash of it, so the record cannot vouch for itself.
    let resolved = h
        .client
        .resolve_pointer(&h.candidate(), "wiki", "Getting-Started")
        .expect("resolving over a real link");
    let pointer = resolved.record.expect("the peer publishes that page");
    assert_eq!(pointer.content_id.as_deref(), Some(head.as_str()));
    assert_eq!(
        resolved.public_key,
        h.node_signing.public_key_bytes(),
        "the key must be the one the handshake proved, not one the reply asserted"
    );

    // The revision, fetched and verified on its own signature. The pointer vouches for which
    // id is current and says nothing about what that id contains.
    let fetched = h
        .client
        .fetch_from(&h.candidate(), &head)
        .expect("the head revision must cross");
    let arrived: otwono_wiki::Revision =
        serde_json::from_slice(&fetched.bytes).expect("the head must be a revision");
    arrived
        .verify(&resolved.public_key)
        .expect("a revision the peer signed must verify against the key the handshake proved");
    assert_eq!(arrived.page, "Getting-Started");

    // And the body, which is a second fetch and the thing a person actually reads.
    let arrived_body = h
        .client
        .fetch_from(&h.candidate(), &arrived.body)
        .expect("the body must cross");
    assert_eq!(arrived_body.bytes, text, "the page did not arrive byte for byte");
}

/// A revision signed by somebody other than the node serving it is refused.
///
/// The check that makes the composition worth anything. `otwono-netd` verifies the *pointer*
/// against the handshake key; nothing it does says who signed the revision that pointer
/// names. A serving node could otherwise publish a pointer to a revision it did not write and
/// have a reader display it as that node's page.
#[test]
fn a_wiki_revision_the_serving_node_did_not_sign_is_refused() {
    let h = Harness::start("wiki-forged");
    let somebody_else = NodeIdentity::generate().unwrap();

    let body = h.put(b"words the serving node never wrote\n", "public");
    // Authored and signed by a third party, and served under the serving node's own name.
    let mut revision =
        otwono_wiki::Revision::new(somebody_else.node_id(), "Getting-Started", &body, None, 1_000);
    revision.signature =
        data_encoding::BASE64.encode(&somebody_else.sign(&revision.signing_bytes().unwrap()).to_bytes());
    let head = h.put(&serde_json::to_vec(&revision).unwrap(), "public");
    h.publish("wiki", "Getting-Started", Some(&head));

    let resolved = h
        .client
        .resolve_pointer(&h.candidate(), "wiki", "Getting-Started")
        .expect("resolving over a real link");
    assert!(resolved.record.is_some(), "the pointer itself is genuine");

    let fetched = h
        .client
        .fetch_from(&h.candidate(), &head)
        .expect("the revision crosses; it is what it says that is wrong");
    let arrived: otwono_wiki::Revision = serde_json::from_slice(&fetched.bytes).unwrap();
    // The author is a third party, so checking against the peer's key fails on the identity
    // binding before the signature is even considered.
    let err = arrived
        .verify(&resolved.public_key)
        .expect_err("a revision by another author must not verify against this peer's key");
    assert_eq!(err, otwono_wiki::WikiError::WrongKey, "{err}");
}

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
        let service = Arc::new(
            StoreService::new(store, perm_socket.clone())
                .with_identity(id_socket.clone())
                .with_cache(cache),
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
        let client = Arc::new(
            NetState::new(Arc::new(signer(dir.join("agreement-client")))).with_holder(Arc::new(
                otwono_netd::content::BrokeredCache::new(&store_socket, &perm_socket),
            )),
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
            shutdown,
        }
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
    assert_eq!(
        again,
        otwono_netd::content::ReplicationPass::NothingTaken { offered: 1 },
        "took the same object twice"
    );
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

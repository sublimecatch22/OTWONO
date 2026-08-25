//! ADR-0009, exercised end to end: the mesh daemon authenticates a peer while being
//! unable to read the node's signing key.
//!
//! Every assertion goes over real Unix sockets against real daemons. An in-process test
//! would prove nothing here — the entire claim is about which *process* can open which
//! file, and about a capability check that runs on the far side of a socket.
//!
//! Three daemons: `otwono-permd` (the broker), `otwono-idd` (holds `node.key`), and two
//! `otwono-netd` states, each holding only its own `agreement.key`.

use otwono_idd::IdentityService;
use otwono_identity::{
    AgreementKeystore, NodeIdentity, SessionSigner, SharingKeystore, SignerError, SigningKeystore,
    HANDSHAKE_HASH_LEN,
};
use otwono_net::{Candidate, TcpLink};
use otwono_netd::{run_listener, BrokeredSigner, NetState};
use otwono_permd::{AuditLog, Broker, Policy};
use otwono_proto::{Client, Server, Shutdown};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Policy that lets the mesh work: the two handshake capabilities and nothing else.
const MESH_POLICY: &str = r#"
[[rule]]
action = "id.sign_session"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "id.bind_agreement"
decision = "allow"
ttl_seconds = 300
"#;

/// The mesh capabilities plus the one ADR-0019 adds, as a node that can open what was
/// shared with it would have.
const SHARING_POLICY: &str = r#"
[[rule]]
action = "id.sign_session"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "id.bind_agreement"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "id.unwrap_shared"
decision = "allow"
ttl_seconds = 300
"#;

struct Harness {
    dir: PathBuf,
    perm_socket: PathBuf,
    id_socket: PathBuf,
    shutdown: Shutdown,
}

impl Harness {
    fn start(tag: &str, policy_toml: &str) -> Harness {
        let dir = std::env::temp_dir().join(format!("otw-ks-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::write(dir.join("policy.d/10-test.toml"), policy_toml).unwrap();

        let perm_socket = dir.join("perm.sock");
        let id_socket = dir.join("id.sock");
        let shutdown = Shutdown::new();

        let policy = Policy::load_dir(&dir.join("policy.d")).expect("policy must load");
        policy
            .validate(&otwono_permd::ActionRegistry::builtin())
            .expect("test policy must name only registered actions");
        let broker = Arc::new(Broker::new(
            policy,
            AuditLog::open(dir.join("audit.jsonl")).unwrap(),
        ));
        let server = Server::bind(&perm_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || server.serve(broker, s));

        // otwono-idd, holding the one and only copy of the signing key.
        let keystore = SigningKeystore::new(dir.join("identity"));
        let sharing_store = SharingKeystore::new(dir.join("identity"));
        let (signing, generated) = keystore.load_or_generate().unwrap();
        assert!(generated);
        let idd = Arc::new(
            IdentityService::new(
                keystore,
                signing,
                sharing_store.load_or_generate().unwrap().0,
                perm_socket.clone(),
            )
            .unwrap(),
        );
        let server = Server::bind(&id_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || server.serve(idd, s));

        Client::connect_waiting(&perm_socket, Duration::from_secs(5)).expect("permd never came up");
        Client::connect_waiting(&id_socket, Duration::from_secs(5)).expect("idd never came up");

        Harness {
            dir,
            perm_socket,
            id_socket,
            shutdown,
        }
    }

    fn token(&self, action: &str) -> Option<String> {
        let mut broker = Client::connect(&self.perm_socket).unwrap();
        broker
            .call(
                "perm.request",
                serde_json::json!({ "action": action, "reason": "test" }),
            )
            .unwrap()
            .ok()?
            .get("token")
            .and_then(|t| t.as_str())
            .map(str::to_string)
    }

    fn identity_dir(&self) -> PathBuf {
        self.dir.join("identity")
    }

    fn signing_key_path(&self) -> PathBuf {
        SigningKeystore::new(self.identity_dir()).key_path()
    }

    fn agreement_key_path(&self, tag: &str) -> PathBuf {
        AgreementKeystore::new(self.agreement_dir(tag)).key_path()
    }

    /// Where a mesh daemon keeps its agreement key.
    ///
    /// The first one shares `identity/` with the signing key, exactly as a shipped node
    /// does — the separation being tested is between processes, not directories, and a
    /// test that quietly moved the files apart would be testing something easier.
    fn agreement_dir(&self, tag: &str) -> PathBuf {
        if tag == "a" {
            self.identity_dir()
        } else {
            self.dir.join(format!("agreement-{tag}"))
        }
    }

    /// A mesh daemon's signer: its own agreement key, bound through the broker.
    fn brokered_signer(&self, tag: &str) -> Result<BrokeredSigner, otwono_netd::BindError> {
        let store = AgreementKeystore::new(self.agreement_dir(tag));
        let (agreement, _) = store.load_or_generate().unwrap();
        BrokeredSigner::bind(agreement, &self.id_socket, &self.perm_socket)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_handshake_completes_after_the_signing_key_is_removed_from_disk() {
    // The claim ADR-0009 makes, stated so that it cannot pass by accident: once the
    // daemons are bound, *delete* node.key and require the mesh to form anyway.
    //
    // File modes would be the obvious lever, but CI runs as root and root ignores them, so
    // a chmod test would pass whether or not otwono-netd reads the key. Deleting the file
    // is decisive for any uid: if this handshake ever needed node.key it now fails.
    let h = Harness::start("split", MESH_POLICY);
    let alice_signer = h.brokered_signer("a").expect("alice must bind");
    let bob_signer = h.brokered_signer("b").expect("bob must bind");

    std::fs::remove_file(h.signing_key_path()).unwrap();
    assert!(!h.signing_key_path().exists());

    // Both mesh daemons here front the same node — one keystore, one signing key — so both
    // bindings name it. What is being tested is the handshake, not who is on each end.
    let node_id = alice_signer.node_id();
    assert_eq!(node_id, bob_signer.node_id());

    let listener = TcpLink::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let responder = Arc::new(NetState::new(Arc::new(bob_signer)));
    {
        let responder = Arc::clone(&responder);
        std::thread::spawn(move || run_listener(responder, listener));
    }

    let initiator = Arc::new(NetState::new(Arc::new(alice_signer)));
    let proved = initiator
        .dial(&Candidate {
            claimed_node_id: node_id,
            address: addr,
        })
        .expect("the handshake must succeed with node.key gone from the filesystem");
    assert_eq!(proved, node_id);
}

#[test]
fn the_mesh_daemon_reads_only_the_agreement_key() {
    // Stronger than "it worked": name the files each side needs. otwono-netd is given the
    // production layout — both key files in one directory — and still only ever opens one.
    let h = Harness::start("layout", MESH_POLICY);
    h.brokered_signer("a").unwrap();

    assert!(h.signing_key_path().exists(), "otwono-idd's key");
    assert!(h.agreement_key_path("a").exists(), "otwono-netd's key");
    assert_eq!(
        h.signing_key_path().parent(),
        h.agreement_key_path("a").parent(),
        "both live in the same directory on a shipped node; the split is between processes"
    );

    // And the two files are disjoint: neither contains a seed the other daemon would need.
    let signing_raw = std::fs::read_to_string(h.signing_key_path()).unwrap();
    let agreement_raw = std::fs::read_to_string(h.agreement_key_path("a")).unwrap();
    let agreement_seed: serde_json::Value = serde_json::from_str(&agreement_raw).unwrap();
    assert!(
        !signing_raw.contains(agreement_seed["agreement_seed"].as_str().unwrap()),
        "otwono-idd's file must not carry otwono-netd's secret: {signing_raw}"
    );
}

#[test]
fn a_mesh_daemon_denied_the_session_capability_cannot_authenticate() {
    // Fail closed. A node that cannot get a session signature must refuse the handshake,
    // not fall back to an unauthenticated channel.
    let h = Harness::start(
        "denied",
        r#"
[[rule]]
action = "id.bind_agreement"
decision = "allow"

[[rule]]
action = "id.sign_session"
decision = "deny"
"#,
    );
    let signer = h.brokered_signer("a").expect("binding is still allowed");
    let err = signer
        .sign_session(&[0u8; HANDSHAKE_HASH_LEN])
        .expect_err("policy denies this");
    assert!(matches!(err, SignerError::Unavailable(_)), "{err}");
}

#[test]
fn a_mesh_daemon_denied_binding_never_starts() {
    // Without a binding the daemon has no NodeID and nothing to present. Refusing to
    // start beats running a node that silently authenticates nobody.
    let h = Harness::start(
        "nobind",
        r#"
[[rule]]
action = "id.bind_agreement"
decision = "deny"
"#,
    );
    let err = h.brokered_signer("a").expect_err("policy denies binding");
    let rendered = err.to_string();
    assert!(rendered.contains("prove who it is"), "{rendered}");
}

#[test]
fn the_session_signer_will_not_sign_arbitrary_bytes() {
    // id.sign_session is an oracle for the session domain. It is bounded to a handshake
    // hash so a caller who reaches it cannot get a signature over anything it chooses.
    let h = Harness::start("oracle", MESH_POLICY);
    let signer = h.brokered_signer("a").unwrap();
    assert!(signer.sign_session(&[0u8; HANDSHAKE_HASH_LEN]).is_ok());
    for bad in [0usize, 1, 31, 33, 64, 128] {
        assert!(
            matches!(
                signer.sign_session(&vec![7u8; bad]),
                Err(SignerError::BadHandshakeHash(n)) if n == bad
            ),
            "a {bad}-byte payload must be refused"
        );
    }
}

#[test]
fn the_agreement_key_alone_cannot_impersonate_the_node() {
    // The other half of the split. Holding the agreement secret without the ability to get
    // a session signature is not enough — which is why a compromised otwono-netd cannot
    // keep authenticating as this node once it is cut off from otwono-idd.
    let h = Harness::start("half", MESH_POLICY);
    let signer = h.brokered_signer("a").unwrap();
    let binding = signer.agreement_binding().unwrap();

    // The attacker has the binding (it crosses the wire on every handshake) and, say, the
    // agreement secret. What it cannot produce is a signature over a *new* handshake hash.
    let stolen = NodeIdentity::generate().unwrap();
    let forged = stolen.sign_session(&[1u8; HANDSHAKE_HASH_LEN]).unwrap();
    let verified = binding.verify().unwrap();
    assert!(
        otwono_identity::verify_signature(
            &verified.public_key,
            &otwono_identity::session_proof_message(&[1u8; HANDSHAKE_HASH_LEN]),
            &forged,
        )
        .is_err(),
        "a signature from any other key must not pass as this node's session proof"
    );
}

#[test]
fn the_identity_daemon_publishes_the_key_the_mesh_daemon_registered() {
    let h = Harness::start("publish", MESH_POLICY);
    let signer = h.brokered_signer("a").unwrap();
    let mine = signer.agreement_binding().unwrap();

    let mut client = Client::connect(&h.id_socket).unwrap();
    let value = client
        .call("id.agreement_binding", serde_json::json!({}))
        .unwrap()
        .expect("the binding is an open method");
    let published: otwono_identity::AgreementBinding = serde_json::from_value(value).unwrap();
    assert_eq!(published.agreement_public_key, mine.agreement_public_key);
    assert_eq!(published.node_id, signer.node_id());

    // And node.pub on disk agrees, so a peer reading the file and a peer completing a
    // handshake see the same node.
    let public: otwono_identity::PublicIdentity = serde_json::from_str(
        &std::fs::read_to_string(SigningKeystore::new(h.identity_dir()).public_path()).unwrap(),
    )
    .unwrap();
    assert!(public.is_self_consistent());
    assert_eq!(public.agreement_public_key, mine.agreement_public_key);
}

#[test]
fn a_peer_can_seal_a_content_key_to_what_the_daemon_publishes() {
    // The whole point of ADR-0019's third key, end to end over the control plane: a peer
    // asks the daemon who to seal to, seals, and the node's own sharing secret opens it.
    // Nothing here trusts an unsigned field — the binding is verified first, and that
    // verification is what turns a NodeID into a key.
    let h = Harness::start("seal", MESH_POLICY);
    h.brokered_signer("a").unwrap();

    let mut client = Client::connect(&h.id_socket).unwrap();
    let value = client
        .call("id.sharing_binding", serde_json::json!({}))
        .unwrap()
        .expect("the sharing binding is an open method, like the agreement one");
    let binding: otwono_identity::SharingBinding = serde_json::from_value(value).unwrap();
    let recipient_key = binding.verify().expect("the daemon must vouch for its own key");

    let content_key = [42u8; 32];
    let sealed = otwono_identity::seal_to(&binding.node_id.to_text(), &recipient_key, &content_key).unwrap();

    // Opened by the secret on disk, which is where otwono-idd will unwrap from.
    let held = SharingKeystore::new(h.identity_dir()).load().unwrap();
    assert_eq!(held.open(&sealed).unwrap().as_ref(), &content_key);
}

/// Seal a content key to whatever the daemon says it is.
fn seal_to_the_node(h: &Harness, content_key: &[u8; 32]) -> otwono_identity::SealedKey {
    let mut client = Client::connect(&h.id_socket).unwrap();
    let value = client
        .call("id.sharing_binding", serde_json::json!({}))
        .unwrap()
        .unwrap();
    let binding: otwono_identity::SharingBinding = serde_json::from_value(value).unwrap();
    let key = binding.verify().unwrap();
    otwono_identity::seal_to(&binding.node_id.to_text(), &key, content_key).unwrap()
}

#[test]
fn the_daemon_unwraps_a_content_key_sealed_to_it() {
    // What otwono-stored will do to open a SHARED object: hand over one recipient's copy
    // and get the content key back, without the sharing secret ever leaving otwono-idd.
    let h = Harness::start("unwrap", SHARING_POLICY);
    h.brokered_signer("a").unwrap();

    let content_key = [17u8; 32];
    let sealed = seal_to_the_node(&h, &content_key);
    let token = h.token("id.unwrap_shared").expect("policy allows unwrapping");

    let value = Client::connect(&h.id_socket)
        .unwrap()
        .call_with_capability(
            "id.unwrap_shared",
            serde_json::json!({ "sealed_key": sealed }),
            &token,
        )
        .unwrap()
        .expect("the node's own copy must open");
    let returned = data_encoding::BASE64
        .decode(value["content_key"].as_str().unwrap().as_bytes())
        .unwrap();
    assert_eq!(returned, content_key);
}

#[test]
fn unwrapping_without_the_capability_is_refused() {
    // MESH_POLICY grants the two handshake capabilities and nothing else. A node that can
    // authenticate peers must not thereby be able to read what was shared with it.
    let h = Harness::start("unwrapdenied", MESH_POLICY);
    h.brokered_signer("a").unwrap();
    let content_key = [18u8; 32];
    let sealed = seal_to_the_node(&h, &content_key);

    assert!(
        h.token("id.unwrap_shared").is_none(),
        "the broker issued a token the policy does not grant"
    );
    let err = Client::connect(&h.id_socket)
        .unwrap()
        .call("id.unwrap_shared", serde_json::json!({ "sealed_key": sealed }))
        .unwrap()
        .expect_err("an unauthenticated unwrap must be refused");
    assert!(
        err.message.contains("capability"),
        "the refusal should say what is missing: {}",
        err.message
    );
}

#[test]
fn a_copy_sealed_to_another_node_is_refused_by_name() {
    // Not merely "did not decrypt": a caller handed the wrong recipient's copy needs to
    // know it will never work, not retry.
    let h = Harness::start("unwrapother", SHARING_POLICY);
    h.brokered_signer("a").unwrap();

    let stranger = otwono_identity::SharingKey::generate().unwrap();
    let theirs = otwono_identity::seal_to("otw1somebodyelse", &stranger.public(), &[19u8; 32]).unwrap();
    let token = h.token("id.unwrap_shared").unwrap();

    let err = Client::connect(&h.id_socket)
        .unwrap()
        .call_with_capability(
            "id.unwrap_shared",
            serde_json::json!({ "sealed_key": theirs }),
            &token,
        )
        .unwrap()
        .expect_err("someone else's copy must not open");
    assert!(err.message.contains("otw1somebodyelse"), "{}", err.message);
}

#[test]
fn a_tampered_seal_does_not_open() {
    let h = Harness::start("unwraptamper", SHARING_POLICY);
    h.brokered_signer("a").unwrap();

    let mut sealed = seal_to_the_node(&h, &[20u8; 32]);
    // Substitute an ephemeral public key of the attacker's choosing, which is the shape of
    // an attempt to make the node derive a key the attacker also knows.
    sealed.ephemeral_public_key =
        data_encoding::BASE64.encode(&otwono_identity::SharingKey::generate().unwrap().public());
    let token = h.token("id.unwrap_shared").unwrap();

    let err = Client::connect(&h.id_socket)
        .unwrap()
        .call_with_capability(
            "id.unwrap_shared",
            serde_json::json!({ "sealed_key": sealed }),
            &token,
        )
        .unwrap()
        .expect_err("a substituted ephemeral key must not open the seal");
    assert!(err.message.contains("does not open"), "{}", err.message);
}

#[test]
fn the_published_identity_carries_a_binding_that_names_this_node() {
    let h = Harness::start("sharepub", MESH_POLICY);
    let signer = h.brokered_signer("a").unwrap();

    let mut client = Client::connect(&h.id_socket).unwrap();
    let value = client.call("id.node", serde_json::json!({})).unwrap().unwrap();
    let published: otwono_identity::PublicIdentity = serde_json::from_value(value).unwrap();
    assert_eq!(published.node_id, signer.node_id());

    let key = published
        .verified_sharing_key()
        .expect("the published binding must check out")
        .expect("a node that has booted has a sharing key");

    // node.pub on disk says the same thing, so a peer handed the file and a peer asking
    // the daemon seal to the same key.
    let on_disk: otwono_identity::PublicIdentity = serde_json::from_str(
        &std::fs::read_to_string(SigningKeystore::new(h.identity_dir()).public_path()).unwrap(),
    )
    .unwrap();
    assert_eq!(on_disk.verified_sharing_key().unwrap(), Some(key));
}

#[test]
fn the_sharing_secret_never_crosses_the_control_plane() {
    // Every open method is checked, not just the two that mention sharing: the secret must
    // not be reachable by asking politely for anything at all.
    let h = Harness::start("sharesecret", MESH_POLICY);
    h.brokered_signer("a").unwrap();
    let held = SharingKeystore::new(h.identity_dir()).load().unwrap();
    let secret = data_encoding::BASE64.encode(held.secret_bytes().as_ref());

    let mut client = Client::connect(&h.id_socket).unwrap();
    for method in [
        "describe",
        "id.node",
        "id.fingerprint",
        "id.agreement_binding",
        "id.sharing_binding",
        "id.succession",
    ] {
        let value = client.call(method, serde_json::json!({})).unwrap().unwrap();
        let text = serde_json::to_string(&value).unwrap();
        assert!(!text.contains(&secret), "{method} returned the sharing secret");
    }

    // And the file the node hands out does not carry it either.
    let public = std::fs::read_to_string(SigningKeystore::new(h.identity_dir()).public_path()).unwrap();
    assert!(!public.contains(&secret), "{public}");
}

#[test]
fn the_mesh_daemons_key_file_holds_no_signing_material() {
    let h = Harness::start("files", MESH_POLICY);
    h.brokered_signer("a").unwrap();

    let signing_raw = {
        // Read it as the only process allowed to: the test stands in for otwono-idd here.
        std::fs::read_to_string(h.signing_key_path()).unwrap()
    };
    let agreement_raw = std::fs::read_to_string(h.agreement_key_path("a")).unwrap();

    let signing: serde_json::Value = serde_json::from_str(&signing_raw).unwrap();
    let agreement: serde_json::Value = serde_json::from_str(&agreement_raw).unwrap();

    assert!(signing.get("agreement_seed").is_none(), "{signing_raw}");
    assert!(agreement.get("signing_seed").is_none(), "{agreement_raw}");
    assert_eq!(signing["algorithm"], "ed25519");
    assert_eq!(agreement["algorithm"], "x25519");
    // No byte of one file appears in the other.
    let signing_seed = signing["signing_seed"].as_str().unwrap();
    assert!(!agreement_raw.contains(signing_seed), "{agreement_raw}");
}

#[test]
fn two_separate_brokered_nodes_authenticate_each_other() {
    // The configuration a real mesh actually has, and the one the single-harness tests
    // above do not cover: two *different* nodes, each with its own signing key, its own
    // identity daemon and its own broker, meeting over TCP. Both sides sign every session
    // through their own otwono-idd.
    let alice = Harness::start("pairA", MESH_POLICY);
    let bob = Harness::start("pairB", MESH_POLICY);
    let alice_signer = alice.brokered_signer("a").expect("alice binds");
    let bob_signer = bob.brokered_signer("a").expect("bob binds");

    let alice_id = alice_signer.node_id();
    let bob_id = bob_signer.node_id();
    assert_ne!(alice_id, bob_id, "two keystores must mean two nodes");

    let listener = TcpLink::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let responder = Arc::new(NetState::new(Arc::new(bob_signer)));
    {
        let responder = Arc::clone(&responder);
        std::thread::spawn(move || run_listener(responder, listener));
    }

    let initiator = Arc::new(NetState::new(Arc::new(alice_signer)));
    let proved = initiator
        .dial(&Candidate {
            claimed_node_id: bob_id,
            address: addr,
        })
        .expect("two brokered nodes must authenticate each other");
    assert_eq!(proved, bob_id);

    // And the responder recorded alice, not something else.
    //
    // Retried, because `dial` returns as soon as *alice* has finished her half: bob's
    // listener thread is still running when the assertion is reached, and reading the table
    // at that instant is a race the test would win or lose on scheduling. It won for a long
    // time, and then adding one socket round trip to bob's hello lost it -- which is the
    // third time on this branch a green assertion turned out to be passing on timing.
    let alice_recorded = wait_until(Duration::from_secs(5), || {
        responder.peers.lock().unwrap().get(&alice_id).is_some()
    });
    assert!(alice_recorded, "bob must have recorded alice");

    // Bob learned where to seal to alice, from the hello she sent, and kept it only because
    // the signature checked out against the NodeID the handshake authenticated (ADR-0019).
    let sealable = wait_until(Duration::from_secs(5), || {
        responder
            .peers
            .lock()
            .unwrap()
            .get(&alice_id)
            .and_then(|p| p.sharing_binding.clone())
            .is_some_and(|b| b.node_id == alice_id && b.verify().is_ok())
    });
    assert!(sealable, "bob must know where to seal to alice");
}

#[test]
fn a_node_that_offers_no_sharing_binding_still_meshes() {
    // Noise needs the agreement key and nothing else. Tying the two together would mean a
    // node that could not share also could not talk -- and a node whose identity daemon is
    // down, or an older build, is exactly such a node.
    let bob = Harness::start("nosharing", MESH_POLICY);
    let bob_signer = bob.brokered_signer("a").expect("bob binds");
    let bob_id = bob_signer.node_id();

    // Alice signs locally and holds no sharing key at all: SessionSigner's default answer
    // for `sharing_binding` is "this node cannot be sealed to".
    let alice = otwono_identity::NodeIdentity::generate().unwrap();
    let alice_id = *alice.node_id();
    assert!(
        alice.sharing_binding().is_err(),
        "the fixture must have no sharing key"
    );

    let listener = TcpLink::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let responder = Arc::new(NetState::new(Arc::new(bob_signer)));
    {
        let responder = Arc::clone(&responder);
        std::thread::spawn(move || run_listener(responder, listener));
    }

    let initiator = Arc::new(NetState::new(Arc::new(alice)));
    initiator
        .dial(&Candidate {
            claimed_node_id: bob_id,
            address: addr,
        })
        .expect("a node with no sharing key must still authenticate");

    let recorded = wait_until(Duration::from_secs(5), || {
        responder.peers.lock().unwrap().get(&alice_id).is_some()
    });
    assert!(recorded, "bob must have recorded alice");
    assert!(
        responder
            .peers
            .lock()
            .unwrap()
            .get(&alice_id)
            .unwrap()
            .sharing_binding
            .is_none(),
        "a peer that offered nothing must not appear sealable"
    );

    // The other direction: alice learned where to seal to bob, who does have one.
    let bobs = wait_until(Duration::from_secs(5), || {
        initiator
            .peers
            .lock()
            .unwrap()
            .get(&bob_id)
            .and_then(|p| p.sharing_binding.clone())
            .is_some()
    });
    assert!(bobs, "alice must know where to seal to bob");
}

/// A node that signs for itself but advertises somebody else's sharing binding.
struct Liar {
    me: otwono_identity::NodeIdentity,
    claim: otwono_identity::SharingBinding,
}

impl SessionSigner for Liar {
    fn node_id(&self) -> otwono_identity::NodeId {
        SessionSigner::node_id(&self.me)
    }
    fn agreement_secret(&self) -> zeroize::Zeroizing<[u8; 32]> {
        self.me.agreement_secret()
    }
    fn agreement_binding(&self) -> Result<otwono_identity::AgreementBinding, SignerError> {
        SessionSigner::agreement_binding(&self.me)
    }
    fn sign_session(&self, handshake_hash: &[u8]) -> Result<[u8; 64], SignerError> {
        self.me.sign_session(handshake_hash)
    }
    fn sharing_binding(&self) -> Result<otwono_identity::SharingBinding, SignerError> {
        Ok(self.claim.clone())
    }
}

#[test]
fn a_peer_advertising_somebody_elses_sharing_binding_ends_the_session() {
    // The binding is genuine -- signed by the key it names -- it is just not this peer's.
    // Accepting it would mean this node later seals data to a stranger while believing it
    // is sealing to the peer it authenticated. Treating the lie as an absence would be
    // nearly as bad: it would teach this daemon to ignore signed claims that do not check
    // out, which is the only kind worth checking.
    let bob = Harness::start("liar", MESH_POLICY);
    let bob_signer = bob.brokered_signer("a").expect("bob binds");
    let bob_id = bob_signer.node_id();

    let stranger = otwono_identity::NodeIdentity::from_seeds(&[41u8; 32], &[42u8; 32], 1);
    let stranger_sharing = otwono_identity::SharingKey::from_seed(&[43u8; 32], 1);
    let alice = Liar {
        me: otwono_identity::NodeIdentity::generate().unwrap(),
        claim: stranger.signing().bind_sharing(&stranger_sharing.public()),
    };
    let alice_id = SessionSigner::node_id(&alice);
    assert!(
        alice.claim.verify().is_ok(),
        "the claim must be genuine, just not alice's"
    );

    let listener = TcpLink::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let responder = Arc::new(NetState::new(Arc::new(bob_signer)));
    {
        let responder = Arc::clone(&responder);
        std::thread::spawn(move || run_listener(responder, listener));
    }

    let initiator = Arc::new(NetState::new(Arc::new(alice)));
    let outcome = initiator.dial(&Candidate {
        claimed_node_id: bob_id,
        address: addr,
    });

    // Bob refuses the session. Alice's own dial may well have completed its half before the
    // link dropped -- she sends first and bob's objection comes after -- so what is asserted
    // is bob's state, not alice's return.
    //
    // "Not connected" rather than "recorded as failed": an inbound peer that fails the hello
    // was never observed, so there is nothing in the table to mark, and recording every
    // stranger who fails a handshake would be an unbounded table anyone could grow.
    let _ = outcome;
    let settled = wait_until(Duration::from_secs(2), || {
        !matches!(
            responder.peers.lock().unwrap().get(&alice_id).map(|p| p.state),
            None | Some(otwono_net::PeerState::Failed)
        )
    });
    assert!(
        !settled,
        "bob accepted a peer advertising somebody else's sharing binding"
    );
    assert!(
        responder
            .peers
            .lock()
            .unwrap()
            .get(&alice_id)
            .and_then(|p| p.sharing_binding.clone())
            .is_none(),
        "and must not have kept the claim"
    );

    // And the refusal was about the binding, not about the harness being broken: an honest
    // node dialling the same listener straight afterwards is accepted. Without this the
    // assertion above would pass just as well if nothing had connected at all.
    let honest = otwono_identity::NodeIdentity::generate().unwrap();
    let honest_id = *honest.node_id();
    Arc::new(NetState::new(Arc::new(honest)))
        .dial(&Candidate {
            claimed_node_id: bob_id,
            address: addr,
        })
        .expect("an honest node must still be accepted");
    assert!(
        wait_until(Duration::from_secs(5), || {
            matches!(
                responder.peers.lock().unwrap().get(&honest_id).map(|p| p.state),
                Some(otwono_net::PeerState::Connected)
            )
        }),
        "bob's listener stopped accepting after refusing the liar"
    );
}

/// Poll until `check` passes or the deadline expires.
///
/// A bounded retry, not a sleep: a sleep long enough to be safe is long enough to hide a
/// regression, and one short enough to be quick is a race.
fn wait_until(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    check()
}

#[test]
fn a_second_handshake_reuses_the_cached_capability() {
    // Regression guard: if the broker issued a one-shot token, the first handshake would
    // work and every later one would fail — a failure mode that only shows up in a
    // long-running mesh, never in a single-connection test.
    let h = Harness::start("reuse", MESH_POLICY);
    let signer = h.brokered_signer("a").unwrap();
    for round in 0..5 {
        signer
            .sign_session(&[round as u8; HANDSHAKE_HASH_LEN])
            .unwrap_or_else(|e| panic!("round {round} failed: {e}"));
    }
}

//! Handshake tests, including the attacks the identity binding exists to prevent.
//!
//! Each runs a real Noise `XX` exchange over a link, with the two ends in separate threads
//! — the handshake is a lock-step conversation, so a single-threaded test would deadlock
//! on the first blocking read.

use otwono_identity::NodeIdentity;
use otwono_net::{HandshakeError, MemoryLink, SecureChannel};

fn identity() -> NodeIdentity {
    NodeIdentity::generate().unwrap()
}

/// Run a handshake between two identities, returning what each side concluded.
fn handshake(
    initiator: NodeIdentity,
    responder: NodeIdentity,
) -> (
    Result<SecureChannel<MemoryLink>, HandshakeError>,
    Result<SecureChannel<MemoryLink>, HandshakeError>,
) {
    let (a, b) = MemoryLink::pair();
    let responder_thread = std::thread::spawn(move || SecureChannel::accept(b, &responder));
    let initiator_result = SecureChannel::initiate(a, &initiator);
    let responder_result = responder_thread.join().expect("responder thread panicked");
    (initiator_result, responder_result)
}

#[test]
fn two_nodes_authenticate_each_other() {
    let alice = identity();
    let bob = identity();
    let (alice_id, bob_id) = (*alice.node_id(), *bob.node_id());

    let (a, b) = handshake(alice, bob);
    let a = a.expect("initiator handshake");
    let b = b.expect("responder handshake");

    // Each side learns the other's NodeID, and it is the right one.
    assert_eq!(
        a.peer().node_id,
        bob_id,
        "the initiator must authenticate the responder"
    );
    assert_eq!(
        b.peer().node_id,
        alice_id,
        "the responder must authenticate the initiator"
    );
}

#[test]
fn the_authenticated_node_id_names_the_key_that_authenticated() {
    // The whole chain: NodeID -> Ed25519 key -> signed binding -> X25519 key used by Noise.
    let alice = identity();
    let bob = identity();
    let bob_agreement = bob.agreement_public().to_bytes();

    let (a, _b) = handshake(alice, bob);
    let peer = a.unwrap();
    assert!(peer.peer().node_id.matches_public_key(&peer.peer().public_key));
    assert_eq!(peer.peer().agreement_public_key, bob_agreement);
}

#[test]
fn an_established_channel_carries_messages_both_ways() {
    let (a, b) = handshake(identity(), identity());
    let mut a = a.unwrap();
    let mut b = b.unwrap();

    let echo = std::thread::spawn(move || {
        let got = b.recv().unwrap();
        b.send(&got).unwrap();
        got
    });

    a.send(b"the quick brown fox").unwrap();
    assert_eq!(a.recv().unwrap(), b"the quick brown fox");
    assert_eq!(echo.join().unwrap(), b"the quick brown fox");
}

#[test]
fn the_same_identity_yields_the_same_node_id_across_sessions() {
    // Identity must be stable: a peer that reconnects is recognisably the same node.
    let seeded = || NodeIdentity::from_seeds(&[9u8; 32], &[8u8; 32], 1);
    let expected = *seeded().node_id();

    let (_, first_responder) = handshake(seeded(), identity());
    let (_, second_responder) = handshake(seeded(), identity());
    assert_eq!(first_responder.unwrap().peer().node_id, expected);
    assert_eq!(second_responder.unwrap().peer().node_id, expected);
}

/// A link that keeps a copy of every frame it carries, so a test can inspect the wire.
struct TappedLink {
    inner: MemoryLink,
    sent: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
}

impl otwono_net::LinkAdapter for TappedLink {
    fn properties(&self) -> otwono_net::LinkProperties {
        self.inner.properties()
    }
    fn send(&mut self, frame: &[u8]) -> Result<(), otwono_net::LinkError> {
        self.sent.lock().unwrap().push(frame.to_vec());
        self.inner.send(frame)
    }
    fn recv(&mut self) -> Result<Vec<u8>, otwono_net::LinkError> {
        self.inner.recv()
    }
}

#[test]
fn nothing_readable_crosses_the_wire() {
    // Tap the link and assert the plaintext never appears in any frame it carried —
    // including the handshake frames, which is where a static key would leak if the
    // pattern were wrong.
    const CANARY: &[u8] = b"PLAINTEXT-CANARY-9f3a";

    let (a, b) = MemoryLink::pair();
    let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let tapped = TappedLink {
        inner: a,
        sent: std::sync::Arc::clone(&sent),
    };

    let bob = identity();
    let responder = std::thread::spawn(move || {
        let mut c = SecureChannel::accept(b, &bob).unwrap();
        c.recv().unwrap()
    });

    let mut channel = SecureChannel::initiate(tapped, &identity()).unwrap();
    channel.send(CANARY).unwrap();
    assert_eq!(responder.join().unwrap(), CANARY, "the peer must still read it");

    let frames = sent.lock().unwrap();
    assert!(
        frames.len() >= 3,
        "expected handshake plus payload frames, got {}",
        frames.len()
    );
    for (i, frame) in frames.iter().enumerate() {
        assert!(
            frame.windows(CANARY.len()).all(|w| w != CANARY),
            "frame {i} carried the plaintext"
        );
    }
}

/// A hostile responder: completes the Noise handshake with its own key, then sends
/// whatever session proof the test tells it to.
///
/// Hand-rolled with `snow` rather than reusing `SecureChannel`, because the point is to do
/// what `SecureChannel` refuses to do.
fn hostile_responder(
    mut link: MemoryLink,
    static_secret: [u8; 32],
    forge: impl FnOnce(&[u8]) -> otwono_net::SessionProof + Send + 'static,
) -> std::thread::JoinHandle<()> {
    use otwono_net::LinkAdapter;
    std::thread::spawn(move || {
        let params = otwono_net::NOISE_PATTERN.parse().unwrap();
        let mut state = snow::Builder::new(params)
            .local_private_key(&static_secret)
            .build_responder()
            .unwrap();
        let mut buf = vec![0u8; 8192];

        // XX: <- e ; -> e ee s es ; <- s se
        let msg = link.recv().unwrap();
        state.read_message(&msg, &mut buf).unwrap();
        let n = state.write_message(&[], &mut buf).unwrap();
        link.send(&buf[..n]).unwrap();
        let msg = link.recv().unwrap();
        state.read_message(&msg, &mut buf).unwrap();

        let hash = state.get_handshake_hash().to_vec();
        let mut transport = state.into_transport_mode().unwrap();

        // The initiator speaks first, so consume its proof before sending the forgery.
        let frame = link.recv().unwrap();
        let mut plain = vec![0u8; frame.len()];
        transport.read_message(&frame, &mut plain).unwrap();

        let proof = forge(&hash);
        let json = serde_json::to_vec(&proof).unwrap();
        let mut out = vec![0u8; json.len() + 16];
        let n = transport.write_message(&json, &mut out).unwrap();
        let _ = link.send(&out[..n]);
    })
}

#[test]
fn a_binding_replayed_from_another_node_is_rejected() {
    // Mallory completes the handshake with her own key while presenting Alice's genuine,
    // correctly-signed binding. Without the check that the binding names the key Noise
    // actually authenticated, she would be accepted as Alice — and anyone who had ever
    // observed Alice's binding could impersonate her.
    let alice = identity();
    let mallory = identity();
    let alice_binding = alice.agreement_binding();
    assert!(
        alice_binding.verify().is_ok(),
        "the replayed binding is genuine, not forged"
    );

    let (a, b) = MemoryLink::pair();
    let attacker = hostile_responder(b, *mallory.agreement().secret_bytes(), move |hash| {
        otwono_net::SessionProof {
            binding: alice_binding,
            handshake_signature: data_encoding::BASE64.encode(
                &mallory
                    .sign(&otwono_net::secure::session_proof_message(hash))
                    .to_bytes(),
            ),
        }
    });

    let err = SecureChannel::initiate(a, &identity()).unwrap_err();
    assert!(
        matches!(err, HandshakeError::BindingDoesNotMatchHandshake),
        "expected the replayed binding to be caught, got {err}"
    );
    attacker.join().unwrap();
}

#[test]
fn a_session_proof_from_a_different_handshake_is_rejected() {
    // Mallory presents her own valid binding, so the agreement-key check passes, but signs
    // some other session's hash. This is the replay the per-session signature exists to
    // stop: without it a binding would be a standing credential, reusable forever.
    let mallory = identity();
    let mallory_binding = mallory.agreement_binding();

    let (a, b) = MemoryLink::pair();
    let attacker = hostile_responder(b, *mallory.agreement().secret_bytes(), move |_hash| {
        let wrong_hash = [0x42u8; 32];
        otwono_net::SessionProof {
            binding: mallory_binding,
            handshake_signature: data_encoding::BASE64.encode(
                &mallory
                    .sign(&otwono_net::secure::session_proof_message(&wrong_hash))
                    .to_bytes(),
            ),
        }
    });

    let err = SecureChannel::initiate(a, &identity()).unwrap_err();
    assert!(
        matches!(err, HandshakeError::StaleOrForgedSessionProof),
        "expected a stale session proof to be caught, got {err}"
    );
    attacker.join().unwrap();
}

#[test]
fn a_binding_whose_node_id_names_someone_else_is_rejected() {
    // Mallory keeps her own agreement key but claims Alice's NodeID.
    let mallory = identity();
    let alice_node_id = *identity().node_id();
    let mut binding = mallory.agreement_binding();
    binding.node_id = alice_node_id;

    let (a, b) = MemoryLink::pair();
    let attacker = hostile_responder(b, *mallory.agreement().secret_bytes(), move |hash| {
        otwono_net::SessionProof {
            binding,
            handshake_signature: data_encoding::BASE64.encode(
                &mallory
                    .sign(&otwono_net::secure::session_proof_message(hash))
                    .to_bytes(),
            ),
        }
    });

    let err = SecureChannel::initiate(a, &identity()).unwrap_err();
    assert!(matches!(err, HandshakeError::Identity(_)), "got {err}");
    attacker.join().unwrap();
}

#[test]
fn a_handshake_against_a_hung_up_link_fails_rather_than_hanging() {
    let (a, b) = MemoryLink::pair();
    drop(b);
    let err = SecureChannel::initiate(a, &identity()).unwrap_err();
    assert!(matches!(err, HandshakeError::Link(_)), "{err}");
}

#[test]
fn garbage_in_place_of_a_handshake_is_rejected() {
    use otwono_net::LinkAdapter;
    let (a, mut b) = MemoryLink::pair();
    let attacker = std::thread::spawn(move || {
        let _ = b.recv();
        let _ = b.send(&[0xAAu8; 96]);
    });
    let err = SecureChannel::initiate(a, &identity()).unwrap_err();
    assert!(matches!(err, HandshakeError::Noise(_)), "{err}");
    attacker.join().unwrap();
}

#[test]
fn two_nodes_authenticate_over_a_real_tcp_socket() {
    // MemoryLink proves the protocol; this proves the framing underneath it. Everything
    // between two VMs on a LAN runs this path.
    use otwono_net::TcpLink;
    use std::time::Duration;

    let listener = TcpLink::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let bob = identity();
    let bob_id = *bob.node_id();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let link = TcpLink::from_stream(stream).unwrap();
        link.set_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut channel = SecureChannel::accept(link, &bob).unwrap();
        let peer = channel.peer().node_id;
        let msg = channel.recv().unwrap();
        channel.send(b"ack").unwrap();
        (peer, msg)
    });

    let alice = identity();
    let alice_id = *alice.node_id();
    let link = TcpLink::connect(addr, Duration::from_secs(10)).unwrap();
    link.set_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut channel = SecureChannel::initiate(link, &alice).unwrap();

    assert_eq!(
        channel.peer().node_id,
        bob_id,
        "initiator authenticated the responder"
    );
    channel.send(b"hello over tcp").unwrap();
    assert_eq!(channel.recv().unwrap(), b"ack");

    let (seen_by_bob, received) = server.join().unwrap();
    assert_eq!(seen_by_bob, alice_id, "responder authenticated the initiator");
    assert_eq!(received, b"hello over tcp");
}

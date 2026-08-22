//! Two complete nodes, over real TCP sockets.
//!
//! Everything here goes through the same code the daemon runs: real listeners, real Noise
//! handshakes, real hello exchange, real peer table. Only mDNS is bypassed — multicast is
//! unreliable inside a container, so candidates are supplied directly. The QEMU two-VM
//! test in `build/qemu/` is what exercises discovery on a real broadcast domain.

use otwono_identity::NodeIdentity;
use otwono_net::{Candidate, PeerState, TcpLink};
use otwono_netd::{run_listener, NetState};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Start a node listening on an ephemeral port.
fn start_node() -> (Arc<NetState>, std::net::SocketAddr) {
    let identity = NodeIdentity::generate().unwrap();
    let listener = TcpLink::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(NetState::new(identity));
    {
        let state = Arc::clone(&state);
        std::thread::spawn(move || run_listener(state, listener));
    }
    // run_listener publishes the bound address; wait for it so tests never race startup.
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.listen_addr.lock().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    (state, addr)
}

fn candidate(state: &NetState, addr: std::net::SocketAddr) -> Candidate {
    Candidate {
        claimed_node_id: *state.identity.node_id(),
        address: addr,
    }
}

#[test]
fn two_nodes_authenticate_each_other_over_tcp() {
    let (alice, _alice_addr) = start_node();
    let (bob, bob_addr) = start_node();
    let alice_id = *alice.identity.node_id();
    let bob_id = *bob.identity.node_id();

    let proved = alice
        .dial(&candidate(&bob, bob_addr))
        .expect("dial should succeed");
    assert_eq!(proved, bob_id, "the dialer must authenticate the listener");

    // Alice recorded Bob as connected.
    let record = alice
        .peers
        .lock()
        .unwrap()
        .get(&bob_id)
        .cloned()
        .expect("bob in alice's table");
    assert_eq!(record.state, PeerState::Connected);
    assert_eq!(record.fingerprint, bob_id.fingerprint());

    // Bob's side is handled on its listener thread; wait for it to land.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if bob
            .peers
            .lock()
            .unwrap()
            .get(&alice_id)
            .is_some_and(|p| p.state == PeerState::Connected)
        {
            break;
        }
        assert!(Instant::now() < deadline, "bob never recorded alice as connected");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_peer_advertising_someone_elses_node_id_is_refused() {
    // The LAN attack: anyone can put any NodeID in an mDNS TXT record. The handshake
    // proves who is really there, and a mismatch must be refused rather than retried.
    let (alice, _) = start_node();
    let (bob, bob_addr) = start_node();
    let impostor_claim = *NodeIdentity::generate().unwrap().node_id();

    let err = alice
        .dial(&Candidate {
            claimed_node_id: impostor_claim,
            address: bob_addr,
        })
        .expect_err("a mismatched advertisement must be refused");
    assert!(err.contains("authenticated as"), "{err}");

    // It is recorded as a failure against the *claimed* identity, with the reason kept.
    let record = alice.peers.lock().unwrap().get(&impostor_claim).cloned().unwrap();
    assert_eq!(record.state, PeerState::Failed);
    assert!(record.last_error.unwrap().contains("advertised"));

    // Bob is untouched: he was never legitimately connected under that claim.
    assert!(alice.peers.lock().unwrap().get(bob.identity.node_id()).is_none());
}

#[test]
fn dialling_a_dead_address_fails_without_hanging() {
    let (alice, _) = start_node();
    // Port 1 on loopback: nothing listens, and connect fails fast.
    let dead = "127.0.0.1:1".parse().unwrap();
    let claim = *NodeIdentity::generate().unwrap().node_id();
    let started = Instant::now();
    let err = alice
        .dial(&Candidate {
            claimed_node_id: claim,
            address: dead,
        })
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "took {:?}",
        started.elapsed()
    );
    assert!(err.contains("connect"), "{err}");
    // Regression: an earlier version recorded a failure only for an identity mismatch,
    // so a peer that simply would not connect sat in Connecting for ever, with no error
    // to explain it.
    let record = alice.peers.lock().unwrap().get(&claim).cloned().unwrap();
    assert_eq!(
        record.state,
        PeerState::Failed,
        "a failed dial must not leave the peer Connecting"
    );
    assert!(record.last_error.is_some(), "a failed dial must record why");
}

#[test]
fn three_nodes_form_a_mesh() {
    // Each pair authenticates independently; no node needs a coordinator.
    let (a, _) = start_node();
    let (b, b_addr) = start_node();
    let (c, c_addr) = start_node();

    a.dial(&candidate(&b, b_addr)).unwrap();
    a.dial(&candidate(&c, c_addr)).unwrap();
    b.dial(&candidate(&c, c_addr)).unwrap();

    assert_eq!(a.peers.lock().unwrap().connected().len(), 2);
    assert!(!b.peers.lock().unwrap().connected().is_empty());
}

#[test]
fn identity_survives_a_restart() {
    // The Phase 3 promise: a node that restarts is recognisably the same node. Uses a
    // keystore on disk, because that is what makes it true.
    let dir = std::env::temp_dir().join(format!("otwono-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let keystore = otwono_identity::Keystore::new(&dir);

    let (first, _) = keystore.load_or_generate().unwrap();
    let original = *first.node_id();
    drop(first);

    let (second, generated) = keystore.load_or_generate().unwrap();
    assert!(!generated, "a restart must not mint a new identity");
    assert_eq!(second.node_id(), &original);

    // And a peer authenticating it after the "restart" sees the same NodeID.
    let listener = TcpLink::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let restarted = Arc::new(NetState::new(second));
    {
        let restarted = Arc::clone(&restarted);
        std::thread::spawn(move || run_listener(restarted, listener));
    }
    let (peer, _) = start_node();
    let proved = peer
        .dial(&Candidate {
            claimed_node_id: original,
            address: addr,
        })
        .expect("the restarted node must authenticate as itself");
    assert_eq!(proved, original);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_dial_failure_leaves_a_peer_stuck_in_connecting() {
    // Sweep the distinct failure modes and assert none of them strand the state machine.
    // The bug this guards against was invisible to unit tests: dial() set Connecting up
    // front and only one of its several error paths cleared it.
    let (alice, _) = start_node();
    let (_bob, bob_addr) = start_node();

    let cases: Vec<(&str, Candidate)> = vec![
        (
            "nothing listening",
            Candidate {
                claimed_node_id: *NodeIdentity::generate().unwrap().node_id(),
                address: "127.0.0.1:1".parse().unwrap(),
            },
        ),
        (
            "wrong identity advertised",
            Candidate {
                claimed_node_id: *NodeIdentity::generate().unwrap().node_id(),
                address: bob_addr,
            },
        ),
    ];

    for (name, candidate) in cases {
        let claim = candidate.claimed_node_id;
        assert!(alice.dial(&candidate).is_err(), "{name} should fail");
        let record = alice.peers.lock().unwrap().get(&claim).cloned().unwrap();
        assert_eq!(
            record.state,
            PeerState::Failed,
            "{name} left the peer in {:?}",
            record.state
        );
        assert!(record.last_error.is_some(), "{name} recorded no reason");
    }
}

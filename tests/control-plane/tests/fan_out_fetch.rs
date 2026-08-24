//! ADR-0015's central claim, tested: a fetch draws from every peer holding pieces.
//!
//! `NEIGHBOURHOOD-CACHE.md` §8 asks for "a fetch with three peers holding disjoint pieces
//! completes and is byte-identical to origin". That is what most of this file is.
//!
//! The peers are made genuinely partial rather than pretend-partial: each store gets the
//! whole object and then has different chunk *files* deleted from disk. So no single peer
//! can serve the object, `store.serve_chunk` really does fail for the chunks each is
//! missing, and the transfer only completes if the fan-out actually spread the work.
//!
//! The security argument is the other half. Once the manifest is known authentic — it
//! hashes to the id that was asked for — any holder of a chunk is as good as any other,
//! because the digest is checked at this end. A peer that serves rubbish wastes bandwidth
//! and cannot corrupt data. Both halves are asserted here.

use otwono_identity::NodeIdentity;
use otwono_net::content::{self, ChunkEntry, ChunkPart, ManifestPage, ProtocolError, Request, Response};
use otwono_net::{LinkProperties, MemoryLink, SecureChannel};
use otwono_netd::{ContentResponder, PeerSource};
use otwono_permd::{ActionRegistry, AuditLog, Broker, Policy};
use otwono_proto::{Client, Server, Shutdown};
use otwono_store::Store;
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

/// One node with a store, its own broker, and a responder that answers peers out of it.
struct Node {
    dir: PathBuf,
    perm_socket: PathBuf,
    store_socket: PathBuf,
    store_dir: PathBuf,
    shutdown: Shutdown,
}

impl Node {
    fn start(tag: &str) -> Node {
        let dir = std::env::temp_dir().join(format!("otw-fan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::write(dir.join("policy.d/10-test.toml"), POLICY).unwrap();

        let perm_socket = dir.join("perm.sock");
        let store_socket = dir.join("store.sock");
        let store_dir = dir.join("store");
        let shutdown = Shutdown::new();

        let policy = Policy::load_dir(&dir.join("policy.d")).unwrap();
        policy.validate(&ActionRegistry::builtin()).unwrap();
        let broker = Arc::new(Broker::new(
            policy,
            AuditLog::open(dir.join("audit.jsonl")).unwrap(),
        ));
        let s = shutdown.clone();
        let server = Server::bind(&perm_socket).unwrap();
        std::thread::spawn(move || server.serve(broker, s));

        // Deliberately *unencrypted*: this test reaches into the store to delete individual
        // chunk files, which is how a peer is made genuinely partial. Encryption is tested
        // elsewhere and would only obscure what is being arranged here.
        let store = Store::new(&store_dir);
        store.ensure_layout().unwrap();
        let service = Arc::new(StoreService::new(store, perm_socket.clone()));
        let s = shutdown.clone();
        let server = Server::bind(&store_socket).unwrap();
        std::thread::spawn(move || server.serve(service, s));

        for sock in [&perm_socket, &store_socket] {
            Client::connect_waiting(sock, Duration::from_secs(5)).unwrap();
        }
        Node {
            dir,
            perm_socket,
            store_socket,
            store_dir,
            shutdown,
        }
    }

    fn token(&self, action: &str) -> String {
        let mut broker = Client::connect(&self.perm_socket).unwrap();
        broker
            .call(
                "perm.request",
                json!({ "action": action, "reason": "fan-out test" }),
            )
            .unwrap()
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn put(&self, bytes: &[u8]) -> Value {
        let token = self.token("store.write");
        let mut client = Client::connect(&self.store_socket).unwrap();
        client
            .call_with_capability(
                "store.put",
                json!({ "data": data_encoding::BASE64.encode(bytes), "visibility": "public" }),
                &token,
            )
            .unwrap()
            .unwrap()
    }

    /// Delete chunk files from this node's store, making it a partial holder.
    fn drop_chunks(&self, digests: &[String]) {
        for hex in digests {
            let path = self
                .store_dir
                .join("chunks")
                .join(&hex[0..2])
                .join(&hex[2..4])
                .join(hex);
            std::fs::remove_file(&path).unwrap_or_else(|e| panic!("removing {}: {e}", path.display()));
        }
    }

    fn responder(&self) -> ContentResponder {
        ContentResponder::new(&self.store_socket, &self.perm_socket)
    }
}

impl Drop for Node {
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

/// Stand a responder up on one end of an in-memory link and hand back the other end,
/// already authenticated, ready to be asked for chunks.
fn source(name: &str, responder: ContentResponder) -> (PeerSource<MemoryLink>, std::thread::JoinHandle<()>) {
    let us = NodeIdentity::generate().unwrap();
    let them = NodeIdentity::generate().unwrap();
    let (ours, theirs) = MemoryLink::pair();
    let serving = std::thread::spawn(move || {
        if let Ok(mut channel) = SecureChannel::accept(theirs, &them) {
            let _ = otwono_netd::serve_session(&mut channel, &responder);
        }
    });
    let channel = SecureChannel::initiate(ours, &us).expect("handshake");
    (
        PeerSource {
            name: name.to_string(),
            channel,
            link: LinkProperties::internet(),
        },
        serving,
    )
}

#[test]
fn three_peers_holding_disjoint_pieces_complete_a_fetch() {
    // NEIGHBOURHOOD-CACHE.md §8's criterion, and the reason this subsystem exists.
    let bytes = payload(400 * 1024, 1);
    let nodes = [Node::start("d0"), Node::start("d1"), Node::start("d2")];

    let record = nodes[0].put(&bytes);
    let id = record["content_id"].as_str().unwrap().to_string();
    for n in &nodes[1..] {
        assert_eq!(n.put(&bytes)["content_id"], record["content_id"]);
    }

    // Learn the chunk list, then give each node a different third of it by deleting the
    // other two thirds from its disk.
    let token = nodes[0].token("store.serve");
    let mut client = Client::connect(&nodes[0].store_socket).unwrap();
    let manifest = client
        .call_with_capability(
            "store.serve_manifest",
            json!({ "content_id": id, "max_chunks": 4096 }),
            &token,
        )
        .unwrap()
        .unwrap();
    let digests: Vec<String> = manifest["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["blake3"].as_str().unwrap().to_string())
        .collect();
    assert!(
        digests.len() >= 3,
        "the fixture must span at least three chunks, got {}",
        digests.len()
    );

    for (i, node) in nodes.iter().enumerate() {
        let drop: Vec<String> = digests
            .iter()
            .enumerate()
            .filter(|(j, _)| j % 3 != i)
            .map(|(_, d)| d.clone())
            .collect();
        node.drop_chunks(&drop);
    }

    // Every node still advertises the object and none can serve it alone.
    for node in &nodes {
        let (peer, serving) = source("solo", node.responder());
        let mut channel = peer.channel;
        let alone = otwono_netd::fetch_object(&mut channel, &id, &LinkProperties::internet());
        assert!(alone.is_err(), "one partial peer served the whole object");
        drop(channel);
        let _ = serving.join();
    }

    let mut sources = Vec::new();
    let mut threads = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        let (peer, t) = source(&format!("peer-{i}"), node.responder());
        sources.push(peer);
        threads.push(t);
    }
    let (fetched, report) =
        otwono_netd::fetch_object_from_peers(sources, &id).expect("three partial peers must suffice");

    assert_eq!(fetched.bytes, bytes, "the assembled object is not byte-identical");
    assert_eq!(fetched.content_id, id);
    assert_eq!(
        report.peers_that_served(),
        3,
        "the work did not actually spread: {report:?}"
    );
    assert!(
        report.chunks_from.values().sum::<usize>() >= digests.len(),
        "fewer chunks were served than the object has: {report:?}"
    );
    for t in threads {
        let _ = t.join();
    }
}

#[test]
fn a_single_peer_is_the_degenerate_case_of_a_fan_out() {
    let bytes = payload(400 * 1024, 2);
    let node = Node::start("single");
    let id = node.put(&bytes)["content_id"].as_str().unwrap().to_string();

    let (peer, serving) = source("only", node.responder());
    let (fetched, report) = otwono_netd::fetch_object_from_peers(vec![peer], &id).unwrap();
    assert_eq!(fetched.bytes, bytes);
    assert_eq!(report.peers_that_served(), 1);
    assert_eq!(report.manifest_from, "only");
    assert!(report.dropped.is_empty());
    let _ = serving.join();
}

#[test]
fn a_peer_that_has_nothing_costs_the_transfer_only_a_demerit() {
    // The ordinary case on a street: most neighbours do not have the thing.
    let bytes = payload(300 * 1024, 3);
    let holder = Node::start("holder");
    let empty = Node::start("empty");
    let id = holder.put(&bytes)["content_id"].as_str().unwrap().to_string();

    let (a, ta) = source("empty", empty.responder());
    let (b, tb) = source("holder", holder.responder());
    let (fetched, report) = otwono_netd::fetch_object_from_peers(vec![a, b], &id)
        .expect("one holder among empty peers is enough");
    assert_eq!(fetched.bytes, bytes);
    assert_eq!(report.manifest_from, "holder");
    assert!(report.demerits.contains_key("empty"), "{report:?}");
    assert_eq!(report.chunks_from.get("empty"), Some(&0));
    for t in [ta, tb] {
        let _ = t.join();
    }
}

#[test]
fn no_peer_having_the_object_is_a_refusal_not_a_hang() {
    let bytes = payload(1024, 4);
    let id = {
        let n = Node::start("gone");
        n.put(&bytes)["content_id"].as_str().unwrap().to_string()
    };
    let empty = [Node::start("e0"), Node::start("e1")];
    let mut sources = Vec::new();
    let mut threads = Vec::new();
    for (i, n) in empty.iter().enumerate() {
        let (peer, t) = source(&format!("empty-{i}"), n.responder());
        sources.push(peer);
        threads.push(t);
    }
    let err = otwono_netd::fetch_object_from_peers(sources, &id).unwrap_err();
    assert!(matches!(err, ProtocolError::NotAvailable(_)), "{err}");
    for t in threads {
        let _ = t.join();
    }
}

#[test]
fn a_substituted_manifest_is_caught_before_a_single_chunk_is_fetched() {
    // The defect this closed: every chunk verifies against the digest a liar declared, so
    // nothing failed until reassembly — after downloading the lot. A ContentId is the hash
    // of the chunk list, so the lie is detectable immediately.
    let real = payload(400 * 1024, 5);
    let fake = payload(400 * 1024, 6);
    let wanted = otwono_store::ContentId::of(&otwono_store::chunk::slice(&real)).to_hex();
    let served = otwono_store::chunk::slice(&fake);

    let alice = NodeIdentity::generate().unwrap();
    let bob = NodeIdentity::generate().unwrap();
    let (ours, theirs) = MemoryLink::pair();
    let chunk_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&chunk_requests);

    let hostile = std::thread::spawn(move || {
        let Ok(mut channel) = SecureChannel::accept(theirs, &bob) else {
            return;
        };
        while let Ok(frame) = channel.recv() {
            let request: Request = content::decode(&frame).unwrap();
            let response = match request {
                Request::Manifest { content_id, .. } => Response::Manifest(ManifestPage {
                    content_id,
                    size_bytes: fake.len() as u64,
                    chunking: otwono_store::CHUNKING_VERSION.to_string(),
                    visibility: "public".into(),
                    total_chunks: served.len() as u32,
                    from_chunk: 0,
                    chunks: served
                        .iter()
                        .map(|c| ChunkEntry {
                            blake3: c.hex(),
                            length: c.length,
                        })
                        .collect(),
                }),
                Request::Chunk {
                    content_id, digest, ..
                } => {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Response::Chunk(ChunkPart {
                        content_id,
                        digest,
                        offset: 0,
                        total_length: 1,
                        data: data_encoding::BASE64.encode(b"x"),
                    })
                }
            };
            if channel.send(&content::encode(&response).unwrap()).is_err() {
                return;
            }
        }
    });

    let mut channel = SecureChannel::initiate(ours, &alice).unwrap();
    let err = otwono_netd::fetch_object(&mut channel, &wanted, &LinkProperties::internet())
        .expect_err("a substituted manifest must be refused");
    assert!(matches!(err, ProtocolError::ObjectIdMismatch { .. }), "{err}");
    assert_eq!(
        chunk_requests.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "chunks were requested against a manifest that had already been shown to be a lie"
    );
    drop(channel);
    let _ = hostile.join();
}

#[test]
fn a_peer_serving_rubbish_wastes_bandwidth_and_cannot_corrupt_the_result() {
    // ADR-0015's security argument, one hash long. The liar declares the *true* chunk list —
    // so the manifest check passes — and then serves garbage for every chunk. The honest
    // peer covers everything, and the liar is dropped.
    let bytes = payload(400 * 1024, 7);
    let honest = Node::start("honest");
    let id = honest.put(&bytes)["content_id"].as_str().unwrap().to_string();
    let truthful_chunks = otwono_store::chunk::slice(&bytes);

    let liar = NodeIdentity::generate().unwrap();
    let us = NodeIdentity::generate().unwrap();
    let (ours, theirs) = MemoryLink::pair();
    let total_len = bytes.len() as u64;
    let hostile = std::thread::spawn(move || {
        let Ok(mut channel) = SecureChannel::accept(theirs, &liar) else {
            return;
        };
        while let Ok(frame) = channel.recv() {
            let Ok(request) = content::decode::<Request>(&frame) else {
                return;
            };
            let response = match request {
                Request::Manifest { content_id, .. } => Response::Manifest(ManifestPage {
                    content_id,
                    size_bytes: total_len,
                    chunking: otwono_store::CHUNKING_VERSION.to_string(),
                    visibility: "public".into(),
                    total_chunks: truthful_chunks.len() as u32,
                    from_chunk: 0,
                    chunks: truthful_chunks
                        .iter()
                        .map(|c| ChunkEntry {
                            blake3: c.hex(),
                            length: c.length,
                        })
                        .collect(),
                }),
                Request::Chunk {
                    content_id,
                    digest,
                    offset,
                    max_bytes,
                } => {
                    // The right shape, the wrong bytes.
                    let length = truthful_chunks
                        .iter()
                        .find(|c| c.hex() == digest)
                        .map(|c| c.length)
                        .unwrap_or(1);
                    let n = max_bytes.min(length - offset) as usize;
                    Response::Chunk(ChunkPart {
                        content_id,
                        digest,
                        offset,
                        total_length: length,
                        data: data_encoding::BASE64.encode(&vec![0xAAu8; n]),
                    })
                }
            };
            if channel.send(&content::encode(&response).unwrap()).is_err() {
                return;
            }
        }
    });

    let liar_source = PeerSource {
        name: "liar".to_string(),
        channel: SecureChannel::initiate(ours, &us).unwrap(),
        link: LinkProperties::internet(),
    };
    let (honest_source, serving) = source("honest", honest.responder());

    let (fetched, report) = otwono_netd::fetch_object_from_peers(vec![liar_source, honest_source], &id)
        .expect("an honest peer among liars must still complete the fetch");
    assert_eq!(fetched.bytes, bytes, "a liar corrupted the result");
    assert!(
        report.demerits.get("liar").copied().unwrap_or(0) > 0,
        "the liar was not demerited: {report:?}"
    );
    assert!(report.dropped.contains(&"liar".to_string()), "{report:?}");
    let _ = hostile.join();
    let _ = serving.join();
}

//! OTWONO node mesh daemon.
//!
//! Brings up the overlay: announce on the LAN, browse for others, authenticate whatever
//! answers, and publish the result on the control plane.
//!
//! # What this daemon is not trusted with
//!
//! This is the Z3 process — the one that parses input from the network
//! (docs/security/SECURITY-MODEL.md). It holds the node's X25519 agreement secret, because
//! Noise needs it in-process, and **nothing else**. The Ed25519 signing key that a NodeID
//! names lives in `otwono-idd`, and this daemon asks for each session signature over the
//! control plane ([`signer::BrokeredSigner`], ADR-0010).
//!
//! Compromising this daemon costs the node its sessions and its agreement key. Both are
//! replaceable. It does not cost the node its name.

#![forbid(unsafe_code)]

pub mod content;
pub mod signer;

pub use content::{
    fetch_object, fetch_object_from_peers, fetch_object_to_file, fetch_pointer, fetch_shared_index,
    serve_session, ContentResponder, FanOutReport, FetchedMeta, FetchedObject, PeerSource,
};
pub use otwono_net::content::SharedIndexEntry;
pub use signer::{BindError, BrokeredSigner};

use otwono_identity::{NodeId, SessionSigner};
use otwono_net::{should_initiate, Candidate, LinkAdapter, PeerState, PeerTable, SecureChannel, TcpLink};
use otwono_proto::{
    unknown_method, CallContext, Client, MethodDescription, RpcError, Service, ServiceDescription,
};
use serde_json::{json, Value};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const SERVICE_NAME: &str = "otwono-netd";
pub const DESCRIBE_SCHEMA_VERSION: &str = "1.0.0";
pub const CAPABILITY_READ: &str = "net.read";
pub const CAPABILITY_CONNECT: &str = "net.connect";
/// Fetching content from a peer. Distinct from `net.connect`, and distinct from
/// `otwono-fetchd`'s `net.fetch`, which is outbound HTTPS to the Internet (ADR-0014) and
/// has nothing to do with the mesh.
pub const CAPABILITY_CONTENT: &str = "net.content";
pub const DEFAULT_PORT: u16 = 8443;

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// The first message each side sends once authenticated.
///
/// Carrying the NodeID again is redundant — the handshake already proved it — and that is
/// the point: a mismatch here would mean the session and the application disagree about
/// who is on the other end, which is worth failing on rather than papering over.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Hello {
    pub node_id: String,
    pub fingerprint: String,
    pub software: String,
    /// Where this node may be sealed to (ADR-0019), signed by its own Ed25519 key.
    ///
    /// Public information — it is what `node.pub` publishes — and carried here so a peer
    /// that has completed a handshake knows where to seal without a second exchange. Absent
    /// on a node that has no sharing key, which is a node nothing can be shared with rather
    /// than a node that cannot mesh: Noise needs the agreement key and nothing else, and
    /// tying the two together would mean a node that could not share also could not talk.
    ///
    /// `#[serde(default)]` so a peer running an older build still parses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing_binding: Option<otwono_identity::SharingBinding>,
}

pub struct NetState {
    /// Where objects too large to return inline are written for the caller (ADR-0018).
    /// `None` on a daemon started without one, and then `net.fetch` says so rather than
    /// silently capping at the inline size.
    pub handoff: Option<otwono_store::Handoff>,
    /// Answers peers' content requests, when this node has a store to answer from. `None`
    /// on a node built without one: such a node meshes and authenticates, and refuses every
    /// content request by never listening for one.
    pub responder: Option<Arc<ContentResponder>>,
    /// Where a replica goes, when this node holds any (ADR-0026 §10). `None` on a node that
    /// does not replicate, and then no replication request ever reaches the wire — which is
    /// §9's rule, kept structural rather than left to a check somebody could forget.
    ///
    /// Not a `Cache`: the cache belongs to `otwono-stored`, and this daemon reaches it over
    /// the control plane precisely so there is only ever one writer to its index.
    pub holder: Option<Arc<dyn otwono_store::ReplicaHolder + Send + Sync>>,
    /// Whatever can sign for this node. In the daemon it is a [`BrokeredSigner`]; in a
    /// test it is usually a whole `NodeIdentity`, which signs locally.
    pub signer: Arc<dyn SessionSigner>,
    node_id: NodeId,
    pub peers: Mutex<PeerTable>,
    pub listen_addr: Mutex<Option<SocketAddr>>,
}

impl NetState {
    pub fn new(signer: Arc<dyn SessionSigner>) -> Self {
        let node_id = signer.node_id();
        NetState {
            handoff: None,
            responder: None,
            holder: None,
            signer,
            node_id,
            peers: Mutex::new(PeerTable::new()),
            listen_addr: Mutex::new(None),
        }
    }

    /// Give this node somewhere to write objects too large to return inline.
    pub fn with_handoff(mut self, handoff: otwono_store::Handoff) -> Self {
        self.handoff = Some(handoff);
        self
    }

    /// Give this node a store to serve peers from.
    pub fn with_responder(mut self, responder: ContentResponder) -> Self {
        self.responder = Some(Arc::new(responder));
        self
    }

    /// Give this node somewhere to put replicas it takes from peers.
    ///
    /// Takes the trait rather than a concrete cache so a test can drive a real
    /// `otwono_store::Cache` in-process, while the daemon passes a [`content::BrokeredCache`]
    /// that goes over the control plane. The pass cannot tell them apart, which is the point.
    pub fn with_holder(mut self, holder: Arc<dyn otwono_store::ReplicaHolder + Send + Sync>) -> Self {
        self.holder = Some(holder);
        self
    }

    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    fn now(&self) -> u64 {
        otwono_identity::now_unix_ms()
    }

    /// Handle one inbound connection: authenticate, exchange hello, record the peer.
    pub fn serve_inbound(&self, link: TcpLink) {
        let address = link.peer_addr();
        let _ = link.set_timeout(Some(HANDSHAKE_TIMEOUT));
        match SecureChannel::accept(link, self.signer.as_ref()) {
            Ok(mut channel) => {
                let node_id = channel.peer().node_id;
                let sharing = match self.exchange_hello(&mut channel, false) {
                    Ok(sharing) => sharing,
                    Err(e) => {
                        eprintln!("otwono-netd: hello with {} failed: {e}", node_id.fingerprint());
                        self.peers.lock().unwrap().record_failure(&node_id, e, self.now());
                        return;
                    }
                };
                let now = self.now();
                self.peers
                    .lock()
                    .unwrap()
                    .record_authenticated(node_id, address, now, sharing);
                eprintln!(
                    "otwono-netd: inbound peer authenticated: {}",
                    node_id.fingerprint()
                );

                // The accepting side answers; the dialling side asks (ADR-0017). A node
                // with no store simply drops the channel here, as it always did.
                if let Some(responder) = self.responder.clone() {
                    if let Err(e) = content::serve_session(&mut channel, responder.as_ref()) {
                        eprintln!(
                            "otwono-netd: content session with {} ended: {e}",
                            node_id.fingerprint()
                        );
                    }
                }
            }
            Err(e) => eprintln!("otwono-netd: inbound handshake refused: {e}"),
        }
    }

    /// Dial a candidate and authenticate it.
    ///
    /// The candidate's claimed NodeID is checked against what the handshake proved. A
    /// mismatch means something on the LAN advertised an identity it does not hold, which
    /// is an incident, not a retry.
    pub fn dial(&self, candidate: &otwono_net::Candidate) -> Result<otwono_identity::NodeId, String> {
        let now = self.now();
        {
            let mut peers = self.peers.lock().unwrap();
            peers.observe(candidate.claimed_node_id, candidate.address, now);
            peers.set_state(&candidate.claimed_node_id, PeerState::Connecting, now);
        }

        // Every failure path must land here. An earlier version recorded only the
        // identity-mismatch case, so a peer that simply would not connect sat in
        // `Connecting` for ever, with no error to explain it.
        let result = self.dial_inner(candidate);
        if let Err(e) = &result {
            let now = self.now();
            self.peers
                .lock()
                .unwrap()
                .record_failure(&candidate.claimed_node_id, e.clone(), now);
        }
        result
    }

    fn dial_inner(&self, candidate: &otwono_net::Candidate) -> Result<otwono_identity::NodeId, String> {
        let link = TcpLink::connect(candidate.address, HANDSHAKE_TIMEOUT)
            .map_err(|e| format!("connect to {}: {e}", candidate.address))?;
        link.set_timeout(Some(HANDSHAKE_TIMEOUT))
            .map_err(|e| e.to_string())?;

        let mut channel = SecureChannel::initiate(link, self.signer.as_ref()).map_err(|e| e.to_string())?;
        let proved = channel.peer().node_id;

        if proved != candidate.claimed_node_id {
            return Err(format!(
                "{} advertised {} but authenticated as {}",
                candidate.address,
                candidate.claimed_node_id.fingerprint(),
                proved.fingerprint()
            ));
        }

        let sharing = self.exchange_hello(&mut channel, true)?;
        let now = self.now();
        self.peers
            .lock()
            .unwrap()
            .record_authenticated(proved, Some(candidate.address), now, sharing);
        Ok(proved)
    }

    /// Dial a peer and fetch one object from it.
    ///
    /// A fresh channel every time, because roles are fixed for a channel's life and this
    /// node has to be the one that dialled in order to ask. It is also the reason this does
    /// not reuse a connection the peer opened to us.
    pub fn fetch_from(&self, candidate: &Candidate, content_id: &str) -> Result<FetchedObject, String> {
        let link = TcpLink::connect(candidate.address, content::FETCH_TIMEOUT)
            .map_err(|e| format!("connect to {}: {e}", candidate.address))?;
        link.set_timeout(Some(content::FETCH_TIMEOUT))
            .map_err(|e| e.to_string())?;
        // Taken before the link is moved into the channel: it is what bounds every message,
        // and a LoRa link and an Ethernet link get very different numbers from it.
        let properties = link.properties();

        let mut channel = SecureChannel::initiate(link, self.signer.as_ref()).map_err(|e| e.to_string())?;
        let proved = channel.peer().node_id;
        if proved != candidate.claimed_node_id {
            return Err(format!(
                "{} advertised {} but authenticated as {}",
                candidate.address,
                candidate.claimed_node_id.fingerprint(),
                proved.fingerprint()
            ));
        }
        self.exchange_hello(&mut channel, true)?;
        content::fetch_object(&mut channel, content_id, &properties).map_err(|e| e.to_string())
    }

    /// Ask one peer what it has sealed to this node (ADR-0020).
    ///
    /// The one question a recipient cannot answer for itself: a `SHARED` object's id is over
    /// ciphertext keyed by a fresh per-object key, so it cannot be derived from the content
    /// the way a `PUBLIC` object's can.
    pub fn shared_with_me(&self, candidate: &Candidate) -> Result<Vec<SharedIndexEntry>, String> {
        let link = TcpLink::connect(candidate.address, content::FETCH_TIMEOUT)
            .map_err(|e| format!("connect to {}: {e}", candidate.address))?;
        link.set_timeout(Some(content::FETCH_TIMEOUT))
            .map_err(|e| e.to_string())?;
        let properties = link.properties();

        let mut channel = SecureChannel::initiate(link, self.signer.as_ref()).map_err(|e| e.to_string())?;
        let proved = channel.peer().node_id;
        if proved != candidate.claimed_node_id {
            return Err(format!(
                "{} advertised {} but authenticated as {}",
                candidate.address,
                candidate.claimed_node_id.fingerprint(),
                proved.fingerprint()
            ));
        }
        self.exchange_hello(&mut channel, true)?;
        content::fetch_shared_index(&mut channel, &properties).map_err(|e| e.to_string())
    }

    /// Ask one peer what it is offering for replication, and take at most one (ADR-0026 §9).
    ///
    /// A fresh channel, for the same reason [`Self::fetch_from`] opens one: roles are fixed
    /// for a channel's life, and this node has to be the one that dialled in order to ask.
    ///
    /// A node with no holder does nothing and says so, without dialling. That is §9's
    /// "a node that does not replicate makes no replication traffic at all" enforced before
    /// the TCP connect rather than after it — a node that connected and then discovered it
    /// had nowhere to put anything would still have told the peer it was interested.
    pub fn replicate_from(&self, candidate: &Candidate) -> Result<content::ReplicationPass, String> {
        let Some(holder) = self.holder.clone() else {
            return Ok(content::ReplicationPass::NotReplicating);
        };
        // Asked before the connect, not after. A holder that will take nothing -- no cache,
        // no budget left, or `cache.replicate` refused -- must not cause a connection at
        // all: a node that dialled and then discovered it had nowhere to put anything would
        // still have told the peer it was interested, which is exactly what §9 rules out.
        // The pass asks again once the channel is up; it is a local socket call, and it
        // keeps the pass correct on its own for the tests that drive it directly.
        match holder.replica_room(&[], self.now()) {
            Some(room) if room.room_bytes > 0 => {}
            _ => return Ok(content::ReplicationPass::NotReplicating),
        }
        let source = self.open_content_channel(candidate)?;
        let content::PeerSource {
            mut channel, link, ..
        } = source;
        content::replication_pass(&mut channel, &link, holder.as_ref(), self.now()).map_err(|e| e.to_string())
    }

    /// Ask one peer what one of its names points at (ADR-0027).
    ///
    /// A fresh channel, for the reason [`Self::fetch_from`] opens one: roles are fixed for a
    /// channel's life and this node has to be the one that dialled in order to ask.
    ///
    /// The answer is verified against the key the handshake proved, inside
    /// [`content::fetch_pointer`], so a peer cannot serve a record for anyone but itself.
    /// `Ok(None)` means it does not publish that name, or would not say — one answer, so
    /// asking cannot enumerate a node's names.
    pub fn pointer_from(
        &self,
        candidate: &Candidate,
        service: &str,
        name: &str,
    ) -> Result<Option<otwono_pointer::Pointer>, String> {
        let source = self.open_content_channel(candidate)?;
        let content::PeerSource {
            mut channel, link, ..
        } = source;
        content::fetch_pointer(&mut channel, service, name, &link).map_err(|e| e.to_string())
    }

    /// Fetch one object from several peers at once (ADR-0015).
    ///
    /// A candidate that cannot be dialled or cannot be authenticated is dropped here rather
    /// than carried into the transfer: a peer this node cannot prove the identity of is not
    /// a peer, whatever it is serving.
    pub fn fetch_from_peers(
        &self,
        candidates: &[Candidate],
        content_id: &str,
    ) -> Result<(FetchedObject, FanOutReport), String> {
        let mut sources = Vec::new();
        let mut unreachable = Vec::new();
        for candidate in candidates {
            match self.open_content_channel(candidate) {
                Ok(source) => sources.push(source),
                Err(e) => unreachable.push(format!("{}: {e}", candidate.address)),
            }
        }
        if sources.is_empty() {
            return Err(format!(
                "no candidate could be reached and authenticated: {}",
                unreachable.join("; ")
            ));
        }
        content::fetch_object_from_peers(sources, content_id).map_err(|e| e.to_string())
    }

    /// Fetch one object from several peers straight into a file owned by `uid`.
    ///
    /// The whole point is that nothing here is ever the size of the object: the workers hold
    /// one chunk each and the bytes go to disk as they are verified (OQ-25).
    pub fn fetch_to_file(
        &self,
        candidates: &[Candidate],
        content_id: &str,
        uid: u32,
    ) -> Result<(FetchedMeta, FanOutReport, std::path::PathBuf), String> {
        let handoff = self
            .handoff
            .as_ref()
            .ok_or("this daemon was started without an export directory")?;
        let mut sources = Vec::new();
        let mut unreachable = Vec::new();
        for candidate in candidates {
            match self.open_content_channel(candidate) {
                Ok(source) => sources.push(source),
                Err(e) => unreachable.push(format!("{}: {e}", candidate.address)),
            }
        }
        if sources.is_empty() {
            return Err(format!(
                "no candidate could be reached and authenticated: {}",
                unreachable.join("; ")
            ));
        }

        // The size is not known until the manifest arrives, so the free-space check cannot
        // be exact. Zero here means "check the floor only"; a fetch that runs out of room
        // fails on a short write, and the file is truncated to nothing when it does.
        let mut outcome = None;
        let exported = handoff
            .export(uid, 0, |file| {
                let file = file
                    .try_clone()
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                match content::fetch_object_to_file(sources, content_id, file) {
                    Ok(v) => {
                        outcome = Some(v);
                        Ok(())
                    }
                    Err(e) => Err(std::io::Error::other(e.to_string())),
                }
            })
            .map_err(|e| e.to_string())?;
        let (meta, report) = outcome.ok_or("the fetch reported neither success nor failure")?;
        Ok((meta, report, exported.path))
    }

    fn open_content_channel(&self, candidate: &Candidate) -> Result<content::PeerSource<TcpLink>, String> {
        let link = TcpLink::connect(candidate.address, content::FETCH_TIMEOUT)
            .map_err(|e| format!("connect: {e}"))?;
        link.set_timeout(Some(content::FETCH_TIMEOUT))
            .map_err(|e| e.to_string())?;
        let properties = link.properties();
        let mut channel = SecureChannel::initiate(link, self.signer.as_ref()).map_err(|e| e.to_string())?;
        let proved = channel.peer().node_id;
        if proved != candidate.claimed_node_id {
            return Err(format!(
                "advertised {} but authenticated as {}",
                candidate.claimed_node_id.fingerprint(),
                proved.fingerprint()
            ));
        }
        self.exchange_hello(&mut channel, true)?;
        Ok(content::PeerSource {
            name: proved.fingerprint(),
            channel,
            link: properties,
        })
    }

    /// Exchange `Hello` and return where the peer says it may be sealed to, verified.
    ///
    /// Returned rather than recorded here, because at this point the peer is not yet in the
    /// table — the caller inserts it — and a setter called before the insert would silently
    /// do nothing. That is exactly what the first version of this did.
    fn exchange_hello<L: LinkAdapter>(
        &self,
        channel: &mut SecureChannel<L>,
        initiator: bool,
    ) -> Result<Option<otwono_identity::SharingBinding>, String> {
        let mine = Hello {
            node_id: self.node_id.to_text(),
            fingerprint: self.node_id.fingerprint(),
            software: format!("otwono-netd/{}", env!("CARGO_PKG_VERSION")),
            // Best effort: a node whose identity daemon cannot be reached still meshes, and
            // simply cannot be sealed to until it can.
            sharing_binding: self.signer.sharing_binding().ok(),
        };
        let encoded = serde_json::to_vec(&mine).map_err(|e| e.to_string())?;
        let expected = channel.peer().node_id;

        let theirs: Hello = if initiator {
            channel.send(&encoded).map_err(|e| e.to_string())?;
            let raw = channel.recv().map_err(|e| e.to_string())?;
            serde_json::from_slice(&raw).map_err(|e| e.to_string())?
        } else {
            let raw = channel.recv().map_err(|e| e.to_string())?;
            let theirs = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
            channel.send(&encoded).map_err(|e| e.to_string())?;
            theirs
        };

        if theirs.node_id != expected.to_text() {
            return Err(format!(
                "hello claims {} but the handshake authenticated {}",
                theirs.node_id,
                expected.to_text()
            ));
        }

        // A peer that sends no binding is one nothing can be shared with, which is fine. A
        // peer that sends one that does not check out is making a signed claim that is
        // false, and the session ends — treating a lie as an absence would teach this
        // daemon to ignore lies, and the claim is about where somebody's data would go.
        if let Some(binding) = &theirs.sharing_binding {
            if binding.node_id != expected {
                return Err(format!(
                    "{} offered a sharing binding for {}",
                    expected.to_text(),
                    binding.node_id.to_text()
                ));
            }
            binding
                .verify()
                .map_err(|e| format!("{}'s sharing binding does not verify: {e}", expected.to_text()))?;
        }
        Ok(theirs.sharing_binding)
    }
}

/// Accept loop. Runs until the listener errors.
pub fn run_listener(state: Arc<NetState>, listener: TcpListener) {
    if let Ok(addr) = listener.local_addr() {
        *state.listen_addr.lock().unwrap() = Some(addr);
    }
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => match TcpLink::from_stream(stream) {
                Ok(link) => {
                    let state = Arc::clone(&state);
                    std::thread::spawn(move || state.serve_inbound(link));
                }
                Err(e) => eprintln!("otwono-netd: rejecting connection: {e}"),
            },
            Err(e) => {
                eprintln!("otwono-netd: accept failed: {e}");
                return;
            }
        }
    }
}

/// Take at most one replica from a peer that just authenticated (ADR-0026 §9).
///
/// Inline, on the discovery thread, and that is a deliberate cost rather than an oversight.
/// §9 chose "on connection" over a timer, and a timer is what a background worker would
/// become — it would need a queue, a bound on how many passes run at once, and a policy for
/// what to do with a peer that disconnected while queued. Running here means the pass is
/// naturally serialised, naturally rate-limited by how often this node meets peers, and has
/// no state of its own.
///
/// What it costs: on a node that has enabled replication, discovering the next peer waits
/// for this pass. The pass is bounded — one object, at most [`content::MAX_FETCH_BYTES`] —
/// but on a slow link that is still a real delay, and it is worth naming. On a stock node no
/// holder is configured and this returns without opening anything.
///
/// A failure is logged and swallowed: the peer is authenticated and connected either way,
/// and a replication pass that did not work out is not a reason to forget a peer.
fn replication_pass_after_dial(state: &Arc<NetState>, candidate: &Candidate) {
    if state.holder.is_none() {
        return;
    }
    match state.replicate_from(candidate) {
        Ok(content::ReplicationPass::Took {
            content_id,
            size_bytes,
        }) => eprintln!(
            "otwono-netd: holding a replica of {} ({size_bytes} bytes) from {}",
            &content_id[..content_id.len().min(16)],
            candidate.claimed_node_id.fingerprint()
        ),
        Ok(content::ReplicationPass::NothingTaken { offered }) => eprintln!(
            "otwono-netd: {} offered {offered} object(s) for replication, took none",
            candidate.claimed_node_id.fingerprint()
        ),
        Ok(content::ReplicationPass::NotReplicating) => {}
        Err(e) => eprintln!(
            "otwono-netd: replication pass with {} failed: {e}",
            candidate.claimed_node_id.fingerprint()
        ),
    }
}

/// How long the discovery loop waits for a new advertisement before retrying known peers.
const DISCOVERY_SWEEP: Duration = Duration::from_secs(30);

/// Take at most one replica from one already-connected peer, on the sweep tick.
///
/// The dial-time pass is not enough on its own, and finding out why corrected ADR-0026 §9.
/// A peer is marked `Connected` once and `retry_candidates` then skips it forever, while
/// `dial_inner` drops its channel as soon as the handshake is done. So a "connection" is a
/// momentary event that happens **once per peer for the life of the daemon** — which would
/// have made a dial-time-only pass replicate one object per peer, ever, and only content
/// that already existed when the two nodes first met. Anything published afterwards would
/// never be offered to anybody.
///
/// This rides the sweep the discovery loop already runs, so §9's reasons survive intact:
/// no new timer and no interval to configure; nothing happens offline, because a node with
/// no peers has nothing to iterate; and it is still rate-limited, now by the sweep rather
/// than by chance meetings.
///
/// **It is self-limiting rather than bounded by a rule.** A node whose budget is full
/// answers `NotReplicating` before the dial, so it makes no traffic at all. A node with room
/// asks around until it has none. One peer per tick, rotating, so no single peer is asked
/// repeatedly while another is never asked.
fn replication_sweep(state: &Arc<NetState>, turn: &mut usize, last: &mut std::time::Instant) {
    if state.holder.is_none() {
        return;
    }
    // Elapsed time, not "the browse timed out". Tying this to the timeout branch would mean
    // a segment whose peers re-announce briskly never sweeps at all: every iteration would
    // take the `Some(candidate)` path and the sweep would starve exactly where there are
    // most peers to ask.
    if last.elapsed() < DISCOVERY_SWEEP {
        return;
    }
    *last = std::time::Instant::now();
    let connected = state.peers.lock().unwrap().connected();
    if connected.is_empty() {
        return;
    }
    let peer = &connected[*turn % connected.len()];
    *turn = turn.wrapping_add(1);
    let Some(address) = peer.addresses.first().and_then(|a| a.parse().ok()) else {
        return;
    };
    replication_pass_after_dial(
        state,
        &Candidate {
            claimed_node_id: peer.node_id,
            address,
        },
    );
}

/// Discovery loop: browse the LAN, dial whoever we are supposed to dial, and keep trying.
///
/// The retry sweep is not optional. mDNS delivers `ServiceResolved` once per resolution,
/// so a dial that loses a startup race — the peer's listener not yet bound, an address
/// still settling — would otherwise never be attempted again, and the two nodes would sit
/// forever having discovered each other and connected to nothing.
pub fn run_discovery(state: Arc<NetState>, discovery: otwono_net::Discovery) {
    let local = *state.node_id();
    let mut turn = 0usize;
    // Starts one interval in the past so the first sweep does not wait for one: a node that
    // has just met its peers is a node with the most to ask them.
    let mut last_sweep = std::time::Instant::now() - DISCOVERY_SWEEP;
    loop {
        let Some(candidate) = discovery.next_candidate(DISCOVERY_SWEEP) else {
            retry_known_peers(&state, &local);
            replication_sweep(&state, &mut turn, &mut last_sweep);
            continue;
        };
        replication_sweep(&state, &mut turn, &mut last_sweep);
        if candidate.claimed_node_id == local {
            continue; // our own advertisement
        }

        // Both nodes see each other at once. Without an election both dial, and each ends
        // up holding a half-used channel while refusing the other's.
        if !should_initiate(&local, &candidate.claimed_node_id) {
            let now = otwono_identity::now_unix_ms();
            state
                .peers
                .lock()
                .unwrap()
                .observe(candidate.claimed_node_id, candidate.address, now);
            continue;
        }

        let already_connected = state
            .peers
            .lock()
            .unwrap()
            .get(&candidate.claimed_node_id)
            .is_some_and(|p| p.state == PeerState::Connected);
        if already_connected {
            continue;
        }

        match state.dial(&candidate) {
            Ok(id) => {
                eprintln!("otwono-netd: outbound peer authenticated: {}", id.fingerprint());
                replication_pass_after_dial(&state, &candidate);
            }
            Err(e) => eprintln!("otwono-netd: dial failed: {e}"),
        }
    }
}

/// Re-dial every known peer this node should be initiating to and is not connected to.
fn retry_known_peers(state: &Arc<NetState>, local: &otwono_identity::NodeId) {
    let candidates: Vec<_> = state
        .peers
        .lock()
        .unwrap()
        .retry_candidates()
        .into_iter()
        .filter(|(node_id, _)| should_initiate(local, node_id))
        .collect();

    for (claimed_node_id, address) in candidates {
        let candidate = Candidate {
            claimed_node_id,
            address,
        };
        match state.dial(&candidate) {
            Ok(id) => {
                eprintln!("otwono-netd: peer authenticated on retry: {}", id.fingerprint());
                replication_pass_after_dial(state, &candidate);
            }
            // Expected while the other side is still coming up. The reason is kept on the
            // peer record either way, so `otwono-netd --peers` can explain a mesh that
            // will not form.
            Err(e) => eprintln!(
                "otwono-netd: retry of {} failed: {e}",
                claimed_node_id.fingerprint()
            ),
        }
    }
}

pub struct NetService {
    state: Arc<NetState>,
    perm_socket: PathBuf,
}

impl NetService {
    pub fn new(state: Arc<NetState>, perm_socket: PathBuf) -> Self {
        NetService { state, perm_socket }
    }

    /// A peer to talk to: an address, and the NodeID the caller expects to find there.
    ///
    /// Both are required. Dialling an address without saying who should answer would make
    /// this daemon connect to whatever is listening, which is exactly the check `dial`
    /// exists to perform.
    fn candidate(params: &Value) -> Result<Candidate, RpcError> {
        let address = params
            .get("address")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("an address is required"))?;
        let node_id = params
            .get("node_id")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("a node_id is required"))?;
        Ok(Candidate {
            claimed_node_id: otwono_identity::NodeId::parse(node_id)
                .map_err(|e| RpcError::invalid_params(e.to_string()))?,
            address: address
                .parse()
                .map_err(|e| RpcError::invalid_params(format!("bad address: {e}")))?,
        })
    }

    fn authorize(&self, ctx: &CallContext, action: &str) -> Result<(), RpcError> {
        let token = ctx
            .capability
            .as_deref()
            .ok_or_else(|| RpcError::unauthorized(format!("{action} requires a capability token")))?;
        let mut client = Client::connect(&self.perm_socket).map_err(|e| {
            RpcError::unavailable(format!(
                "cannot reach the permission broker at {}: {e}",
                self.perm_socket.display()
            ))
        })?;
        client
            .call(
                "perm.verify",
                json!({ "token": token, "action": action, "subject": ctx.peer.subject() }),
            )
            .map_err(|e| RpcError::unavailable(format!("broker call failed: {e}")))?
            .map(|_| ())
    }
}

impl Service for NetService {
    fn describe(&self) -> ServiceDescription {
        ServiceDescription {
            schema_version: DESCRIBE_SCHEMA_VERSION.to_string(),
            service: SERVICE_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            methods: vec![
                MethodDescription::open("net.status", "This node's overlay identity and link state"),
                MethodDescription::guarded(
                    "net.peers",
                    "Peers this node has met, and their authentication state",
                    CAPABILITY_READ,
                ),
                MethodDescription::guarded(
                    "net.connect",
                    "Dial a peer at a given address and authenticate it",
                    CAPABILITY_CONNECT,
                ),
                MethodDescription::guarded(
                    "net.shared_with_me",
                    "Ask one peer what it has sealed to this node (ADR-0020)",
                    CAPABILITY_CONTENT,
                ),
                MethodDescription::guarded(
                    "net.fetch",
                    "Fetch one content-addressed object from a peer, verified on arrival",
                    CAPABILITY_CONTENT,
                ),
            ],
        }
    }

    fn call(&self, ctx: &CallContext, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "net.status" => {
                let peers = self.state.peers.lock().unwrap();
                Ok(json!({
                    "node_id": self.state.node_id().to_text(),
                    "fingerprint": self.state.node_id().fingerprint(),
                    "listen_addr": self.state.listen_addr.lock().unwrap().map(|a| a.to_string()),
                    "discovery": otwono_net::SERVICE_TYPE,
                    "peers_known": peers.len(),
                    "peers_connected": peers.connected().len(),
                }))
            }
            "net.peers" => {
                // Who a node has met is privacy-relevant even though each NodeID is public.
                self.authorize(ctx, CAPABILITY_READ)?;
                let peers = self.state.peers.lock().unwrap().all();
                serde_json::to_value(json!({ "peers": peers })).map_err(|e| RpcError::internal(e.to_string()))
            }
            "net.connect" => {
                self.authorize(ctx, CAPABILITY_CONNECT)?;
                let candidate = Self::candidate(&params)?;
                let proved = self
                    .state
                    .dial(&candidate)
                    .map_err(|e| RpcError::new(otwono_proto::code::UNAVAILABLE, e))?;
                Ok(json!({ "node_id": proved.to_text(), "fingerprint": proved.fingerprint() }))
            }
            // net.content, the same capability a fetch needs: every id in the reply is one
            // this node could then fetch, so being allowed to ask is being allowed to fetch.
            "net.shared_with_me" => {
                self.authorize(ctx, CAPABILITY_CONTENT)?;
                let candidate = Self::candidate(&params)?;
                let entries = self
                    .state
                    .shared_with_me(&candidate)
                    .map_err(|e| RpcError::new(otwono_proto::code::UNAVAILABLE, e))?;
                Ok(json!({
                    "schema_version": DESCRIBE_SCHEMA_VERSION,
                    "peer": candidate.claimed_node_id.to_text(),
                    "entries": entries
                        .iter()
                        .map(|e| json!({
                            "content_id": e.content_id,
                            "plaintext_size_bytes": e.plaintext_size_bytes,
                        }))
                        .collect::<Vec<_>>(),
                }))
            }
            "net.fetch" => {
                self.authorize(ctx, CAPABILITY_CONTENT)?;
                let content_id = params
                    .get("content_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| RpcError::invalid_params("net.fetch needs a content_id"))?;
                // One peer or several. Several is the point of ADR-0015 — every holder of a
                // chunk is as good as any other, so a dense cluster transfers faster —
                // and one peer is just the degenerate case of it.
                let candidates: Vec<Candidate> = match params.get("peers").and_then(Value::as_array) {
                    Some(list) => list.iter().map(Self::candidate).collect::<Result<_, _>>()?,
                    None => vec![Self::candidate(&params)?],
                };
                if candidates.is_empty() {
                    return Err(RpcError::invalid_params("net.fetch needs at least one peer"));
                }
                let names: Vec<String> = candidates.iter().map(|c| c.claimed_node_id.to_text()).collect();
                // Explicit, not guessed. A caller that does not know an object's size asks
                // for a file; one that knows it is small saves itself a file to clean up.
                let to_file = params.get("to_file").and_then(Value::as_bool).unwrap_or(false);
                if to_file {
                    let (meta, report, path) = self
                        .state
                        .fetch_to_file(&candidates, content_id, ctx.peer.uid)
                        .map_err(|e| RpcError::new(otwono_proto::code::UNAVAILABLE, e))?;
                    return Ok(json!({
                        "schema_version": DESCRIBE_SCHEMA_VERSION,
                        "content_id": meta.content_id,
                        "visibility": meta.visibility,
                        "chunking": meta.chunking,
                        "size_bytes": meta.size_bytes,
                        // Present when the object is shared: the file on disk is ciphertext
                        // until this is used, so leaving it behind would hand the caller a
                        // file they cannot read and no way to find out why.
                        "sharing": meta.sharing,
                        "path": path.display().to_string(),
                        "owner_uid": ctx.peer.uid,
                        "asked": names,
                        "manifest_from": report.manifest_from,
                        "chunks_from": report.chunks_from,
                        "peers_that_served": report.peers_that_served(),
                        "dropped": report.dropped,
                        // Caching a file-delivered object would need a cache.import that
                        // does not exist; cache.put is inline and inherits the 640 KiB cap.
                        "cached": false,
                        "note": "this file is plaintext and yours; read it and unlink it",
                    }));
                }
                let (fetched, report) = self
                    .state
                    .fetch_from_peers(&candidates, content_id)
                    .map_err(|e| RpcError::new(otwono_proto::code::UNAVAILABLE, e))?;

                // Never by default. Caching a peer's content is storing bytes the operator
                // did not choose one at a time, so it is asked for or it does not happen
                // (CLUSTER-CACHE.md §5).
                let wanted_cache = params.get("cache").and_then(Value::as_bool).unwrap_or(false);
                let cached = match (wanted_cache, self.state.responder.as_ref()) {
                    (false, _) => json!(false),
                    (true, None) => json!({ "error": "this daemon has no store to cache into" }),
                    (true, Some(responder)) => match responder.cache(&fetched) {
                        Ok(v) => v,
                        // A cache miss is not a fetch failure. The bytes are verified and in
                        // the caller's hands either way, and saying so beats discarding them.
                        Err(e) => json!({ "error": e }),
                    },
                };
                Ok(json!({
                    "schema_version": DESCRIBE_SCHEMA_VERSION,
                    "content_id": fetched.content_id,
                    "visibility": fetched.visibility,
                    "chunking": fetched.chunking,
                    "size_bytes": fetched.bytes.len(),
                    "sharing": fetched.sharing,
                    "asked": names,
                    "manifest_from": report.manifest_from,
                    "chunks_from": report.chunks_from,
                    "peers_that_served": report.peers_that_served(),
                    "dropped": report.dropped,
                    "cached": cached,
                    "data": data_encoding::BASE64.encode(&fetched.bytes),
                }))
            }
            other => Err(unknown_method(other)),
        }
    }
}

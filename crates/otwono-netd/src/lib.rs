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

pub mod signer;

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
pub const DEFAULT_PORT: u16 = 8443;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

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
}

pub struct NetState {
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
            signer,
            node_id,
            peers: Mutex::new(PeerTable::new()),
            listen_addr: Mutex::new(None),
        }
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
                if let Err(e) = self.exchange_hello(&mut channel, false) {
                    eprintln!("otwono-netd: hello with {} failed: {e}", node_id.fingerprint());
                    self.peers.lock().unwrap().record_failure(&node_id, e, self.now());
                    return;
                }
                let now = self.now();
                self.peers
                    .lock()
                    .unwrap()
                    .record_authenticated(node_id, address, now);
                eprintln!(
                    "otwono-netd: inbound peer authenticated: {}",
                    node_id.fingerprint()
                );
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

        self.exchange_hello(&mut channel, true)?;
        let now = self.now();
        self.peers
            .lock()
            .unwrap()
            .record_authenticated(proved, Some(candidate.address), now);
        Ok(proved)
    }

    fn exchange_hello<L: LinkAdapter>(
        &self,
        channel: &mut SecureChannel<L>,
        initiator: bool,
    ) -> Result<(), String> {
        let mine = Hello {
            node_id: self.node_id.to_text(),
            fingerprint: self.node_id.fingerprint(),
            software: format!("otwono-netd/{}", env!("CARGO_PKG_VERSION")),
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
        Ok(())
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

/// How long the discovery loop waits for a new advertisement before retrying known peers.
const DISCOVERY_SWEEP: Duration = Duration::from_secs(30);

/// Discovery loop: browse the LAN, dial whoever we are supposed to dial, and keep trying.
///
/// The retry sweep is not optional. mDNS delivers `ServiceResolved` once per resolution,
/// so a dial that loses a startup race — the peer's listener not yet bound, an address
/// still settling — would otherwise never be attempted again, and the two nodes would sit
/// forever having discovered each other and connected to nothing.
pub fn run_discovery(state: Arc<NetState>, discovery: otwono_net::Discovery) {
    let local = *state.node_id();
    loop {
        let Some(candidate) = discovery.next_candidate(DISCOVERY_SWEEP) else {
            retry_known_peers(&state, &local);
            continue;
        };
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
            Ok(id) => eprintln!("otwono-netd: outbound peer authenticated: {}", id.fingerprint()),
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
            Ok(id) => eprintln!("otwono-netd: peer authenticated on retry: {}", id.fingerprint()),
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
                let address = params
                    .get("address")
                    .and_then(Value::as_str)
                    .ok_or_else(|| RpcError::invalid_params("net.connect needs an address"))?;
                let node_id = params
                    .get("node_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| RpcError::invalid_params("net.connect needs a node_id"))?;
                let candidate = Candidate {
                    claimed_node_id: otwono_identity::NodeId::parse(node_id)
                        .map_err(|e| RpcError::invalid_params(e.to_string()))?,
                    address: address
                        .parse()
                        .map_err(|e| RpcError::invalid_params(format!("bad address: {e}")))?,
                };
                let proved = self
                    .state
                    .dial(&candidate)
                    .map_err(|e| RpcError::new(otwono_proto::code::UNAVAILABLE, e))?;
                Ok(json!({ "node_id": proved.to_text(), "fingerprint": proved.fingerprint() }))
            }
            other => Err(unknown_method(other)),
        }
    }
}

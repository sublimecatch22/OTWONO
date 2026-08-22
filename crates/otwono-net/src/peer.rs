//! The peer table.
//!
//! Tracks who this node has met, over which addresses, and in what state. Trust is *not*
//! recorded here: every peer in this table is authenticated, and authenticated is not
//! trusted (docs/network/NODE-IDENTITY.md). Naming a peer is a separate, user-visible act.

use otwono_identity::NodeId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::SocketAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerState {
    /// Seen by discovery, not yet authenticated.
    Discovered,
    /// A Noise handshake is in progress.
    Connecting,
    /// Authenticated. Its NodeID has been checked against the key it handshook with.
    Connected,
    /// Was connected, now not reachable.
    Lost,
    /// A handshake was attempted and refused.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRecord {
    pub node_id: NodeId,
    pub fingerprint: String,
    pub addresses: Vec<String>,
    pub state: PeerState,
    pub first_seen_unix_ms: u64,
    pub last_seen_unix_ms: u64,
    /// Why the last attempt failed, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Default)]
pub struct PeerTable {
    peers: BTreeMap<NodeId, PeerRecord>,
}

impl PeerTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that discovery saw a peer, without claiming anything about it.
    pub fn observe(&mut self, node_id: NodeId, address: SocketAddr, now_ms: u64) {
        let entry = self.peers.entry(node_id).or_insert_with(|| PeerRecord {
            node_id,
            fingerprint: node_id.fingerprint(),
            addresses: Vec::new(),
            state: PeerState::Discovered,
            first_seen_unix_ms: now_ms,
            last_seen_unix_ms: now_ms,
            last_error: None,
        });
        entry.last_seen_unix_ms = now_ms;
        let address = address.to_string();
        if !entry.addresses.contains(&address) {
            entry.addresses.push(address);
        }
    }

    pub fn set_state(&mut self, node_id: &NodeId, state: PeerState, now_ms: u64) {
        if let Some(p) = self.peers.get_mut(node_id) {
            p.state = state;
            p.last_seen_unix_ms = now_ms;
            if state == PeerState::Connected {
                p.last_error = None;
            }
        }
    }

    pub fn record_failure(&mut self, node_id: &NodeId, error: String, now_ms: u64) {
        if let Some(p) = self.peers.get_mut(node_id) {
            p.state = PeerState::Failed;
            p.last_error = Some(error);
            p.last_seen_unix_ms = now_ms;
        }
    }

    /// Insert a peer learned by authenticating it, which may be the first we hear of it.
    pub fn record_authenticated(&mut self, node_id: NodeId, address: Option<SocketAddr>, now_ms: u64) {
        if let Some(addr) = address {
            self.observe(node_id, addr, now_ms);
        } else {
            self.peers.entry(node_id).or_insert_with(|| PeerRecord {
                node_id,
                fingerprint: node_id.fingerprint(),
                addresses: Vec::new(),
                state: PeerState::Discovered,
                first_seen_unix_ms: now_ms,
                last_seen_unix_ms: now_ms,
                last_error: None,
            });
        }
        self.set_state(&node_id, PeerState::Connected, now_ms);
    }

    pub fn get(&self, node_id: &NodeId) -> Option<&PeerRecord> {
        self.peers.get(node_id)
    }

    pub fn all(&self) -> Vec<PeerRecord> {
        self.peers.values().cloned().collect()
    }

    pub fn connected(&self) -> Vec<PeerRecord> {
        self.peers
            .values()
            .filter(|p| p.state == PeerState::Connected)
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

/// Should this node dial that one, or wait to be dialled?
///
/// Both nodes see each other over mDNS at the same moment, so without a rule they both
/// dial and end up with two half-used channels — or, worse, each accepts the other's while
/// its own is still handshaking. Comparing NodeIDs gives both sides the same answer with
/// no negotiation: the lower one dials.
pub fn should_initiate(local: &NodeId, remote: &NodeId) -> bool {
    local < remote
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(seed: u8) -> NodeId {
        NodeId::from_public_key(&[seed; 32])
    }

    fn addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    #[test]
    fn observing_a_peer_records_it_as_discovered_not_connected() {
        let mut t = PeerTable::new();
        t.observe(node(1), addr(9000), 100);
        let p = t.get(&node(1)).unwrap();
        assert_eq!(
            p.state,
            PeerState::Discovered,
            "seeing a peer proves nothing about it"
        );
        assert_eq!(p.addresses, vec!["127.0.0.1:9000"]);
        assert!(t.connected().is_empty());
    }

    #[test]
    fn repeated_observations_accumulate_addresses_without_duplicating() {
        let mut t = PeerTable::new();
        t.observe(node(1), addr(9000), 100);
        t.observe(node(1), addr(9000), 200);
        t.observe(node(1), addr(9001), 300);
        let p = t.get(&node(1)).unwrap();
        assert_eq!(p.addresses.len(), 2);
        assert_eq!(p.first_seen_unix_ms, 100, "first_seen must not move");
        assert_eq!(p.last_seen_unix_ms, 300);
    }

    #[test]
    fn authentication_promotes_a_peer_to_connected() {
        let mut t = PeerTable::new();
        t.observe(node(2), addr(9000), 100);
        t.record_authenticated(node(2), Some(addr(9000)), 200);
        assert_eq!(t.connected().len(), 1);
    }

    #[test]
    fn an_inbound_peer_we_never_discovered_is_still_recorded() {
        // Discovery is not the only way to meet: a peer may dial us first.
        let mut t = PeerTable::new();
        t.record_authenticated(node(3), None, 100);
        assert_eq!(t.get(&node(3)).unwrap().state, PeerState::Connected);
    }

    #[test]
    fn a_failure_is_recorded_with_its_reason() {
        let mut t = PeerTable::new();
        t.observe(node(4), addr(9000), 100);
        t.record_failure(&node(4), "binding replayed".into(), 200);
        let p = t.get(&node(4)).unwrap();
        assert_eq!(p.state, PeerState::Failed);
        assert_eq!(p.last_error.as_deref(), Some("binding replayed"));
    }

    #[test]
    fn reconnecting_clears_a_stale_error() {
        let mut t = PeerTable::new();
        t.observe(node(5), addr(9000), 100);
        t.record_failure(&node(5), "timeout".into(), 200);
        t.set_state(&node(5), PeerState::Connected, 300);
        assert_eq!(t.get(&node(5)).unwrap().last_error, None);
    }

    #[test]
    fn exactly_one_side_of_a_pair_initiates() {
        // The property that matters: run it both ways round and never get two dialers or
        // two waiters.
        let (a, b) = (node(10), node(20));
        assert_ne!(
            should_initiate(&a, &b),
            should_initiate(&b, &a),
            "exactly one of the pair must dial"
        );
    }

    #[test]
    fn the_election_is_consistent_across_many_pairs() {
        for i in 0..30u8 {
            for j in 0..30u8 {
                if i == j {
                    continue;
                }
                let (x, y) = (node(i), node(j));
                assert_ne!(should_initiate(&x, &y), should_initiate(&y, &x), "{i} vs {j}");
            }
        }
    }

    #[test]
    fn a_node_does_not_dial_itself() {
        let me = node(7);
        assert!(!should_initiate(&me, &me));
    }
}

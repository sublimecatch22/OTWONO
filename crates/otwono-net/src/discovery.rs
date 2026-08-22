//! LAN peer discovery over mDNS/DNS-SD.
//!
//! Nodes advertise `_otwono._tcp.local.` and browse for the same. This is the LAN case
//! from docs/network/NODE-NETWORK.md Section 5; the DHT and radio beacons are separate
//! mechanisms for environments where multicast does not reach.
//!
//! **Discovery yields candidates, never peers.** Anything learned here is unauthenticated
//! hearsay: the NodeID in a TXT record is a claim by whoever sent the packet, and an
//! attacker on the same LAN can advertise any NodeID they like. It becomes a peer only
//! after a Noise handshake proves possession of the matching key. Nothing in this module
//! may be used for a trust decision.

use otwono_identity::NodeId;
use std::net::SocketAddr;

/// DNS-SD service type. Changing this partitions the network.
pub const SERVICE_TYPE: &str = "_otwono._tcp.local.";
/// TXT key carrying the advertised NodeID.
pub const TXT_NODE_ID: &str = "node";

/// A candidate seen on the LAN. The NodeID is claimed, not proved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub claimed_node_id: NodeId,
    pub address: SocketAddr,
}

pub struct Discovery {
    daemon: mdns_sd::ServiceDaemon,
    events: mdns_sd::Receiver<mdns_sd::ServiceEvent>,
    instance: String,
}

impl Discovery {
    /// Announce this node and start browsing for others.
    pub fn start(node_id: &NodeId, port: u16) -> Result<Self, DiscoveryError> {
        let daemon = mdns_sd::ServiceDaemon::new().map_err(|e| DiscoveryError(e.to_string()))?;

        // The instance name must be unique on the LAN and stable for this node. The
        // fingerprint is both, and unlike a hostname it does not leak what the machine is
        // called or who owns it.
        let instance = node_id.fingerprint().replace(':', "-");
        let hostname = format!("{instance}.local.");
        let properties = [(TXT_NODE_ID, node_id.to_text())];

        let service =
            mdns_sd::ServiceInfo::new(SERVICE_TYPE, &instance, &hostname, (), port, &properties[..])
                .map_err(|e| DiscoveryError(e.to_string()))?
                .enable_addr_auto();

        daemon
            .register(service)
            .map_err(|e| DiscoveryError(e.to_string()))?;
        let events = daemon
            .browse(SERVICE_TYPE)
            .map_err(|e| DiscoveryError(e.to_string()))?;
        Ok(Discovery {
            daemon,
            events,
            instance,
        })
    }

    pub fn instance_name(&self) -> &str {
        &self.instance
    }

    /// Block for the next resolved candidate, discarding events that are not one.
    ///
    /// Returns `None` when the daemon shuts down.
    pub fn next_candidate(&self, timeout: std::time::Duration) -> Option<Candidate> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.checked_duration_since(std::time::Instant::now())?;
            let event = self.events.recv_timeout(remaining).ok()?;
            if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
                if let Some(c) = candidate_from(&info) {
                    return Some(c);
                }
            }
        }
    }

    pub fn shutdown(&self) {
        let _ = self.daemon.shutdown();
    }
}

/// Extract a candidate from a resolved service, or nothing if it is not one of ours.
pub fn candidate_from(info: &mdns_sd::ServiceInfo) -> Option<Candidate> {
    let claimed = info.get_property_val_str(TXT_NODE_ID)?;
    let claimed_node_id = NodeId::parse(claimed).ok()?;
    let address = info.get_addresses().iter().next().copied()?;
    Some(Candidate {
        claimed_node_id,
        address: SocketAddr::new(address, info.get_port()),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryError(pub String);

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mDNS discovery failed: {}", self.0)
    }
}

impl std::error::Error for DiscoveryError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_type_is_the_documented_one() {
        // Changing this silently partitions a network into two that cannot see each other.
        assert_eq!(SERVICE_TYPE, "_otwono._tcp.local.");
    }

    #[test]
    fn the_instance_name_is_derived_from_the_fingerprint_not_the_hostname() {
        // A hostname would leak what the machine is called and who owns it. The
        // fingerprint is stable, unique enough, and reveals only the NodeID, which is
        // already public.
        let id = NodeId::from_public_key(&[3u8; 32]);
        let instance = id.fingerprint().replace(':', "-");
        assert!(instance.starts_with("otw1-"));
        assert!(
            !instance.contains(':'),
            "colons are not valid in a DNS-SD instance name"
        );
    }

    #[test]
    fn a_txt_record_without_a_node_id_yields_no_candidate() {
        // Anything on the LAN can publish this service type. A record we cannot parse is
        // discarded, not guessed at.
        let info = mdns_sd::ServiceInfo::new(
            SERVICE_TYPE,
            "stranger",
            "stranger.local.",
            "127.0.0.1",
            9999,
            &[("other", "value")][..],
        )
        .unwrap();
        assert_eq!(candidate_from(&info), None);
    }

    #[test]
    fn a_malformed_node_id_in_a_txt_record_is_discarded() {
        let info = mdns_sd::ServiceInfo::new(
            SERVICE_TYPE,
            "liar",
            "liar.local.",
            "127.0.0.1",
            9999,
            &[(TXT_NODE_ID, "not-a-node-id")][..],
        )
        .unwrap();
        assert_eq!(candidate_from(&info), None);
    }

    #[test]
    fn a_well_formed_advertisement_yields_a_candidate() {
        let id = NodeId::from_public_key(&[5u8; 32]);
        let info = mdns_sd::ServiceInfo::new(
            SERVICE_TYPE,
            "peer",
            "peer.local.",
            "10.0.2.15",
            7777,
            &[(TXT_NODE_ID, id.to_text().as_str())][..],
        )
        .unwrap();
        let c = candidate_from(&info).expect("should resolve");
        assert_eq!(c.claimed_node_id, id, "the NodeID is *claimed*, not proved");
        assert_eq!(c.address.port(), 7777);
    }
}

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
    let addresses: Vec<std::net::IpAddr> = info.get_addresses().iter().copied().collect();
    let address = dialable_address(&addresses)?;
    Some(Candidate {
        claimed_node_id,
        address: SocketAddr::new(address, info.get_port()),
    })
}

/// Pick an address a peer can actually be reached at.
///
/// mDNS hands back a *set* of addresses, and the previous version of this took whichever
/// one iteration happened to yield first. On a two-VM segment with no DHCP that was
/// sometimes the IPv6 link-local address, and connecting to one of those without an
/// interface scope fails with `EINVAL`:
///
/// ```text
/// connect to [fe80::5054:ff:fe07:1101]:8443: link I/O failed: Invalid argument (os error 22)
/// ```
///
/// So an unscoped `fe80::` address is not a candidate at all — it is a string that looks
/// like one. It is dropped rather than ranked last, because ranking it last would still
/// leave it chosen on a node that advertises nothing else, and a clear "no reachable
/// address" is a better answer than a confusing `EINVAL` twenty seconds later.
///
/// Everything else is ranked: routable before link-local, and IPv4 before IPv6 among
/// equals. The ordering is total and deterministic, so two nodes seeing the same
/// advertisement make the same choice — a set's iteration order is not something a mesh
/// should depend on.
pub fn dialable_address(addresses: &[std::net::IpAddr]) -> Option<std::net::IpAddr> {
    let mut usable: Vec<std::net::IpAddr> = addresses
        .iter()
        .copied()
        .filter(|a| match a {
            // Cannot be dialled without a scope id, which an advertisement does not carry.
            std::net::IpAddr::V6(v6) => !is_ipv6_link_local(v6) && !v6.is_unspecified() && !v6.is_multicast(),
            std::net::IpAddr::V4(v4) => !v4.is_unspecified() && !v4.is_multicast() && !v4.is_broadcast(),
        })
        .collect();
    usable.sort_by_key(|a| (rank(a), a.to_string()));
    usable.into_iter().next()
}

fn is_ipv6_link_local(v6: &std::net::Ipv6Addr) -> bool {
    // fe80::/10. `Ipv6Addr::is_unicast_link_local` is unstable, so it is spelled out.
    v6.segments()[0] & 0xffc0 == 0xfe80
}

/// Lower sorts first: routable, then link-local, then loopback.
fn rank(a: &std::net::IpAddr) -> u8 {
    let (link_local, loopback, v6) = match a {
        std::net::IpAddr::V4(v4) => (v4.is_link_local(), v4.is_loopback(), 0),
        std::net::IpAddr::V6(v6) => (false, v6.is_loopback(), 1),
    };
    // Loopback last: a peer advertising it is describing a machine that is not this one.
    let class = if loopback {
        4
    } else if link_local {
        2
    } else {
        0
    };
    class + v6
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
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("a literal address")
    }

    #[test]
    fn an_unscoped_ipv6_link_local_address_is_not_a_candidate() {
        // Defect 43. Two VMs on a segment with no DHCP: mDNS advertised both a working
        // IPv4 link-local address and an fe80:: one, the set yielded the fe80:: first, and
        // every dial failed with EINVAL for fifteen minutes.
        assert_eq!(
            dialable_address(&[ip("fe80::5054:ff:fe07:1101"), ip("169.254.19.24")]),
            Some(ip("169.254.19.24"))
        );
    }

    #[test]
    fn a_node_advertising_only_an_ipv6_link_local_address_has_no_reachable_one() {
        // Dropped rather than ranked last: a clear "nowhere to dial" beats an EINVAL later.
        assert_eq!(dialable_address(&[ip("fe80::1")]), None);
        assert_eq!(dialable_address(&[]), None);
    }

    #[test]
    fn a_routable_address_beats_a_link_local_one() {
        assert_eq!(
            dialable_address(&[ip("169.254.19.24"), ip("192.168.1.10")]),
            Some(ip("192.168.1.10"))
        );
        assert_eq!(
            dialable_address(&[ip("169.254.19.24"), ip("2001:db8::1")]),
            Some(ip("2001:db8::1"))
        );
    }

    #[test]
    fn ipv4_wins_a_tie_because_it_has_no_scope_to_get_wrong() {
        assert_eq!(
            dialable_address(&[ip("2001:db8::1"), ip("192.168.1.10")]),
            Some(ip("192.168.1.10"))
        );
    }

    #[test]
    fn loopback_is_last_because_a_peer_advertising_it_means_another_machine() {
        assert_eq!(
            dialable_address(&[ip("127.0.0.1"), ip("169.254.19.24")]),
            Some(ip("169.254.19.24"))
        );
        // But still usable if it is genuinely all there is.
        assert_eq!(dialable_address(&[ip("127.0.0.1")]), Some(ip("127.0.0.1")));
    }

    #[test]
    fn the_choice_does_not_depend_on_the_order_the_set_yielded() {
        // A mesh whose behaviour depends on a HashSet's iteration order is a mesh that
        // works on some boots.
        let a = [ip("fe80::1"), ip("169.254.19.24"), ip("192.168.1.10")];
        let b = [ip("192.168.1.10"), ip("fe80::1"), ip("169.254.19.24")];
        let c = [ip("169.254.19.24"), ip("192.168.1.10"), ip("fe80::1")];
        assert_eq!(dialable_address(&a), dialable_address(&b));
        assert_eq!(dialable_address(&b), dialable_address(&c));
        assert_eq!(dialable_address(&a), Some(ip("192.168.1.10")));
    }

    #[test]
    fn addresses_that_cannot_be_a_peer_are_refused() {
        assert_eq!(dialable_address(&[ip("0.0.0.0")]), None);
        assert_eq!(dialable_address(&[ip("::")]), None);
        assert_eq!(dialable_address(&[ip("224.0.0.251")]), None);
        assert_eq!(dialable_address(&[ip("ff02::fb")]), None);
    }

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

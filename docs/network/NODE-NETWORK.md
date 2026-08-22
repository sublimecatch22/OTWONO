# OTWONO Node Mesh (ONM)

**Status:** `SPECIFIED`. No network code exists yet. This document is the design contract
that `otwono-netd` must satisfy.

## 1. Position in the OS

ONM is an operating-system subsystem, not an application. It starts at boot, loads the node
identity, brings up every available link, and offers local services regardless of Internet
availability.

Ordinary Internet networking is untouched: NetworkManager or systemd-networkd continue to
own the normal IP stack. ONM is an overlay that runs **beside** it. A user who never touches
ONM still has a completely normal Linux network experience.

## 2. Component decomposition

```
otwono-netd
├── link/         LinkAdapter implementations
├── transport/    Noise-secured channels, QUIC over IP
├── discovery/    mDNS, radio beacons, DHT, manual pairing
├── routing/      reachability, next-hop, store-and-forward spool
├── gateway/      opt-in Internet bridging and federation
└── rpc/          Local Control Plane surface
```

Each is a module behind a trait, independently testable with an in-memory fake link.

## 3. The `LinkAdapter` interface

Every physical medium is wrapped in one interface. This is what stops LoRa support from
metastasizing through the codebase.

```rust
pub trait LinkAdapter: Send + Sync {
    fn id(&self) -> LinkId;
    fn kind(&self) -> LinkKind;              // Ethernet | WiFi | WiFiDirect | Ble
                                             // | LoRa | Ieee802154 | Ax25 | UsbGadget | Internet
    fn properties(&self) -> LinkProperties;
    fn send(&self, to: LinkPeer, frame: &[u8]) -> Result<(), LinkError>;
    fn subscribe(&self) -> Receiver<LinkEvent>;
}

pub struct LinkProperties {
    pub mtu: usize,
    pub bandwidth_class: BandwidthClass,     // Trickle | Narrow | Broad | Wide
    pub duty_cycle: Option<DutyCycle>,       // legal/regulatory limit, e.g. EU868 1%
    pub energy_cost: EnergyCost,             // Low | Medium | High
    pub broadcast_capable: bool,
    pub typical_latency: Duration,
}
```

### Bandwidth classes

| Class | Throughput | Media | What is allowed |
|---|---|---|---|
| `Trickle` | <1 KB/s | LoRa, AX.25 | Text messages, presence, tiny signed records. **Never** bulk transfer. |
| `Narrow` | 1 KB/s–1 MB/s | 802.15.4, BLE, poor Wi-Fi | Text, small images, incremental sync |
| `Broad` | 1–100 MB/s | Wi-Fi, 100M Ethernet | Everything, with care |
| `Wide` | >100 MB/s | Gigabit+, local Internet | Everything |

The router **must** consult the bandwidth class. Attempting a 4 MB image transfer over a
duty-cycle-limited LoRa link is not slow, it is illegal in most jurisdictions and it jams
the channel for everyone. Enforcement lives in the router, not in each service.

## 4. Secure transport

- **Noise `XX`** handshake over every link: mutual authentication with the node's static
  key, forward secrecy, identity hiding from passive observers.
- Over IP: **QUIC** (via `quinn`/rust-libp2p) for multiplexed streams, congestion control,
  and migration across network changes.
- Over non-IP links: a thin length-prefixed framing carrying the same Noise channel, with
  fragmentation and reassembly sized to the link MTU.
- Replay protection with per-channel nonce sequencing; rekey on a byte and time budget.

Authenticated is **not** trusted. Every peer is authenticated; trust is a separate,
explicit, user-visible decision.

## 5. Discovery

| Environment | Method |
|---|---|
| LAN | mDNS/DNS-SD (`_otwono._udp.local`) |
| Direct radio | Periodic signed beacons, duty-cycle aware, with jitter |
| Internet | Kademlia DHT via rust-libp2p, plus configurable bootstrap and relay nodes |
| In person | QR code / short pairing code carrying the NodeID fingerprint |
| Explicit | Manual peer entry, and import of a signed peer list |

Discovery yields *candidates*. A candidate becomes a peer only after a successful Noise
handshake, and a **known** peer only after the user names it (a petname). SSH's
known-hosts model, with a UI that a normal person can actually use.

## 6. Routing

Constraints that make this hard, and that a naive design gets wrong:

- Links have wildly different capacity (six orders of magnitude between LoRa and gigabit).
- The network partitions constantly and normally.
- Many nodes are battery- or duty-cycle-limited and cannot participate in chatty protocols.
- Nodes appear and disappear; addresses are identity-based, not topological.

Approach:

1. **Within a connected partition:** link-state with metrics weighted by bandwidth class,
   latency, and energy cost.
2. **Across partitions:** store-and-forward with a delay-tolerant queue — messages are
   held, signed, TTL-bounded, and forwarded on contact.
3. **On `Trickle` links:** epidemic/opportunistic forwarding for small signed records only,
   with strict per-neighbour rate limits.

**We will not write our own mesh routing protocol.** `routing/` is an interface with a
simple, documented reference implementation for bring-up and testing, behind which we
integrate an existing protocol. Candidates evaluated in OQ-4:

| Candidate | Strength | Concern |
|---|---|---|
| **Reticulum** | Designed exactly for this: identity-addressed, radio-first, DTN semantics, works on LoRa | Python reference implementation; a Rust interop path needs assessment |
| **Yggdrasil** | Mature, self-healing IPv6 overlay, cryptographic addressing | IP-centric; poor fit for `Trickle` links |
| **Babel** | Proven Wi-Fi mesh, RFC 8966, loop-avoiding | Layer-3 IP only; no identity layer, no DTN |
| **libp2p only** | Already a dependency; excellent on the Internet | No radio/DTN story at all |

Likely outcome: libp2p for the IP world, Reticulum-style semantics for the radio world,
unified behind our `routing/` interface. That must be proven with measurements before it
becomes an ADR.

## 7. Gateways

A node with an uplink may, **only if the user opts in**, act as a gateway:

- **Outbound:** ONM peers reach the Internet through it, under an explicit policy
  (allowed destinations, bandwidth cap, per-peer quota, logging). Off by default. The legal
  and abuse implications are presented to the user in plain language before enabling.
- **Rendezvous:** it helps ONM nodes on different physical networks find each other via the
  DHT and relays.
- **Federation:** it exchanges signed network descriptors with other ONM networks and
  selectively replicates agreed collections.

## 8. Offline behaviour matrix

| Available | Discovery | Messaging | Content | Search | AI |
|---|---|---|---|---|---|
| Internet + peers | Full (DHT + LAN) | Real-time + offline queue | Full fetch and replication | Local + federated | Local + authorized peers |
| LAN only | mDNS | Real-time to LAN peers, queued for others | LAN peers + local | Local + LAN peers | Local + LAN peers |
| Radio only | Beacons | Trickle-safe text, store-and-forward | Text-first; blobs only on `Narrow`+ | Local + trickle queries | Local only |
| Fully isolated | None | Queued for later delivery | Local store only | Local index only | Local only |

The bottom row is the important one: with nothing attached, the wiki, notes, profile site,
messaging drafts, media library, and assistant must all still work.

## 9. Testing

- Fake in-memory `LinkAdapter` with configurable bandwidth, latency, loss, and duty cycle.
- Multi-node tests in Linux network namespaces; multi-arch tests across QEMU VMs.
- **Partition tests are mandatory:** partition, exchange, heal, assert convergence.
- Negative tests: a `PRIVATE` object must never appear on any link; an unauthenticated
  peer must never receive `SHARED` content.

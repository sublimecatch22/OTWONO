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
    fn kind(&self) -> LinkKind;              // Ethernet | WiFi | WiFiDirect | WiFiMesh | Ble
                                             // | LoRa | Ieee802154 | Ax25 | UsbGadget | Internet
    fn role(&self) -> LinkRole;              // Station | AccessPoint | Peer
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

### A node may create the network, not only join it

`LinkRole` exists because "Wi-Fi" is two different things to this system, and only one of
them was covered before.

| Role | Meaning | Why it matters |
|---|---|---|
| `Station` | The node joins an existing network | The ordinary home case: a router already exists |
| `AccessPoint` | The node **is** the network — it runs `hostapd` and others associate to it | A node in a place with no infrastructure, a field deployment, or a cluster where one house's node serves the street |
| `Peer` | Symmetric, no infrastructure: Wi-Fi Direct, 802.11s mesh, LoRa | Nodes find each other with nothing in between |

The `AccessPoint` role is what turns a household node into local infrastructure. Combined
with the cluster cache, it is the difference between "there is a fast copy nearby" and
"there is a fast copy nearby **and a way to reach it**" — a street where every house has a
node does not need anyone's uplink to move data between houses.

802.11s (`WiFiMesh`) is the middle ground: several nodes form a self-healing mesh at layer 2
with no single node designated as the infrastructure. Where the hardware and drivers support
it, it beats a chain of access points. Where they do not — and consumer Wi-Fi chipsets are
uneven here — `AccessPoint` plus `Station` is the fallback that always works.

Three things this must get right, and each is a way to cause real harm:

- **Regulatory.** Channel, power and DFS behaviour are jurisdiction-bound. The same
  per-region profile-as-data approach that **OQ-10** specifies for LoRa duty cycles applies
  to Wi-Fi, and for the same reason: these are legal limits, not preferences.
- **An open access point is an abuse vector.** A node broadcasting an unsecured network is
  offering an anonymous uplink to anyone in range, with the operator's name on the line.
  Access points are authenticated by default, and the gateway rules in §7 — opt-in, with the
  legal exposure explained in plain language — apply in full.
- **Bringing up an AP disturbs the household.** A single-radio device cannot be a station and
  an access point on different channels at once without cost. Do not silently reconfigure
  someone's working Wi-Fi; this is opt-in, and it says what it will do first.

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

### 4a. Content fetch over an established channel

**STATUS: IMPLEMENTED** — ADR-0017, `schemas/onm-content.schema.json`.

Once a channel is up and `Hello` exchanged, the node that **dialled** may ask; the node that
**accepted** answers. Roles are fixed for the channel's life, so this is a loop rather than a
state machine — a node wanting content from a peer that called it dials its own channel.

Two requests, both ranged, both naming the object they concern:

| Message | Answers |
|---|---|
| `content.manifest` | one window of an object's chunk list, plus its size, chunking version and label |
| `content.chunk` | one range of one chunk of one object |

and exactly one error, `not_available`, carrying no reason — absent, private, shared,
damaged and not-part-of-that-object must be indistinguishable, or a peer can enumerate what
this node holds by asking.

A chunk request names its `content_id` as well as the digest. Without that, a
content-addressed store answers "do you hold these exact bytes" for any digest a stranger
cares to guess, and since chunks are shared between objects, a private object and a public
one can contain the same one.

The responder is `otwono-netd`, which calls `store.serve_manifest` / `store.serve_chunk` on
`otwono-stored` and re-checks the label itself before anything reaches the link
(`DATA-VISIBILITY.md` §4). It holds `store.serve` and no other store capability.

**Measured limits (2026-08-24).** The bandwidth-class table above says `Trickle` is for
"text messages, presence, tiny signed records" and never bulk transfer. The protocol agrees
by arithmetic: a manifest reply costs 262 bytes before a single entry, against a 256-byte
EU868 LoRa frame, so a fetch over a `Trickle` link is refused before anything is sent. The
Noise handshake does not fit one either — a session-proof frame is 447 bytes. Content fetch
therefore works from `Narrow` upward. See OQ-23 and OQ-24.

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

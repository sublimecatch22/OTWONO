# ADR-0015 — A content-addressed cluster cache, not a ledger

**Status:** accepted · **Date:** 2026-08-24

## Context

The requirement, in the words it arrived in: every node contributes a small slice of disk,
locked and encrypted and reachable only by the node system; that slice is used to transfer,
cache and store data; it works "almost like a blockchain but for verifying that the data is
safe"; and when many nodes sit close together — a street where every house has one — a node
pulls from several verified sources at once so transfers get *faster* as the cluster
gets denser.

The goal is right and the mechanism named for it is not. It is worth separating the two,
because the difference is several orders of magnitude of cost.

**What a blockchain provides** is a total order agreed among parties who do not trust each
other, secured by making disagreement expensive. It answers "which of these conflicting
histories is the real one?"

**What this requirement actually needs** is two different guarantees:

- **Integrity** — the bytes I received are the bytes that were meant, whoever handed them
  to me.
- **Availability** — the bytes can be got from whoever is nearest, not only from origin.

Neither needs consensus. There is no conflicting history to arbitrate: a block of data
either hashes to its name or it does not. Content addressing settles that with one hash,
and it is already the project's first primitive (`docs/services/DISTRIBUTED-SERVICES.md`
§1) and its existing hash function (BLAKE3, per ADR-0007 and the model store).

## Decision

**Every node contributes a bounded, encrypted, content-addressed cache, and fetches are
served in parallel from any peer holding the chunks — verified by hash on arrival, never
by trusting the peer.**

1. **The name is the hash.** A chunk is addressed by its BLAKE3 digest. A peer that serves
   it cannot alter it without the digest failing, so **a source does not have to be
   trusted to be useful.** This is the whole of the "verifying that the data is safe"
   requirement, and it costs one hash per chunk.
2. **Density makes it faster.** Because any holder of a chunk is as good as any other, a
   fetch fans out across every reachable peer that has pieces. Ten nodes on a street with
   overlapping caches serve each other at LAN speed instead of ten separate uplink
   downloads. This is the property the requirement is really asking for, and it is a
   consequence of (1) rather than an extra mechanism.
3. **The cache is bounded and tier-scaled.** A fixed, operator-set slice, defaulted from
   the capability tier (ADR-0004) — a T0 SBC contributes gigabytes, a T3 desktop tens of
   them. One place decides, as §2.6 requires.
4. **Encrypted at rest, owned by one daemon.** The cache directory is opened by
   `otwono-stored` and nothing else, and its contents are encrypted on disk.
5. **Only `PUBLIC` and `REPLICATED` objects are ever cached or served.** `PRIVATE` and
   `SHARED` data never enters the shared cache, in any form, at any time.

**No ledger, no chain, no consensus, no token.**

## Consequences

**Good.** The mechanism is small enough to be right: a hash check on receipt, a set of
chunks we hold, and a fan-out fetch. It reuses primitives that exist rather than adding a
fourth (§1's own test). A malicious or merely broken peer can waste our bandwidth and
cannot corrupt our data. The dense-cluster speedup is real and needs no coordination
between neighbours — no registry, no accounting, nobody to run it. And it degrades
correctly: with no peers, the cache is just a local cache.

**Bad, and worth naming.**

- **Holding is publishing.** A node that serves a chunk tells its neighbours it has that
  chunk. Over time, what a household is interested in is inferable from what its node
  serves. Restricting the cache to `PUBLIC`/`REPLICATED` bounds the damage; it does not
  eliminate it, because *which* public things you hold is itself information. This is a
  real privacy cost of the design and the UI must say so before an operator opts in.
- **Serving is carrying.** Caching content for neighbours means storing bytes the operator
  did not choose one at a time. The existing rule — nothing replicated without per-collection
  opt-in (`DISTRIBUTED-SERVICES.md` §5) — is what keeps this bounded, and it applies here
  without exception.
- **No fairness mechanism.** A node that only takes and never gives is not punished. That
  is deliberate: reputation and accounting systems are where "almost like a blockchain"
  usually creeps back in, and they bring a coordination problem this design does not
  otherwise have. Revisit only with evidence of real freeloading harm (**OQ-17**).
- **Chunking is a compatibility decision.** Chunk size and boundary algorithm determine
  whether two nodes that fetched the same file independently can serve each other. Get it
  wrong and the swarm silently does not form. It is fixed by the schema, versioned, and
  changing it splits the network (**OQ-16**).

**Where something ledger-like would genuinely be needed** — and is therefore not ruled out
forever — is resource accounting between mutually distrusting neighbours, or a tamper-evident
public record of who published what and when. Neither is required to make transfers fast,
which is what this ADR is about.

## Alternatives rejected

- **A blockchain or DLT.** Buys consensus, which nothing here needs; costs storage growth,
  a synchronisation protocol, and a participation incentive on hardware chosen for being
  small and cheap. On a Raspberry Pi with an 8 GB eMMC, a monotonically growing chain is
  not a design, it is a countdown.
- **A signed index of "safe" data.** Someone must sign it, which reintroduces an authority
  the network exists to avoid, and it verifies a *list* rather than the *bytes*.
- **Trusting peers that authenticated.** Noise `XX` proves who a peer is, not that what it
  sent is what we asked for. Authenticated is not trusted — `SECURITY-MODEL.md` says so
  about remote peers, and it applies to bytes as much as to instructions.
- **Origin-only fetch with a plain HTTP cache.** Simple, and it throws away exactly the
  property being asked for: neighbours cannot help each other.

## References

- `docs/services/CLUSTER-CACHE.md` — the specification this decides.
- ADR-0004 (capability tiers — where the cache size comes from), ADR-0007 (visibility
  labels — what may be cached at all).
- **OQ-16** (chunking parameters), **OQ-17** (freeloading and fairness).

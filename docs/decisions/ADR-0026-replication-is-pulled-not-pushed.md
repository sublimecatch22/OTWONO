# ADR-0026 — Replication is pulled, never pushed, and it is best-effort by construction

**Status:** accepted · **Date:** 2026-08-26 · **STATUS: SPECIFIED** — no code exists yet.

## Context

`REPLICATED` is the last unimplemented visibility label and the last unstarted item in
Phase 5. `DATA-VISIBILITY.md` §1 defines it as "explicitly permitted to be copied to other
nodes for availability and resilience", §3 sketches a policy block on the object record —
`target_replicas`, `max_hops`, `ttl_days`, `max_size_bytes`, `allow_rereplication` — and
none of it exists. Today `REPLICATED` behaves exactly like `PUBLIC`: it serves on request
and nothing ever copies it anywhere.

Two things already decided constrain this, and they turn out to settle most of it:

- **`content_replication` is a capability-engine feature gate**, tier-derived and forced off
  on constrained storage. Whether a machine replicates at all is already decided in the one
  place CLAUDE.md §2.6 permits, and this ADR must not re-derive it.
- **ADR-0015's cluster cache already solved consent for one direction.** A node caches what
  passed *through* it, inside a budget its operator set. Nothing arrives unasked.

The question this settles is who initiates a copy, and the answer decides everything else.

## Decision

### 1. A replica is pulled by its holder. Nothing is ever pushed

A node with replication enabled and budget to spare **asks** peers what `REPLICATED` content
they hold, and chooses what to take. An owner never sends an object to a node that did not
ask for it.

The naive design is the opposite — the owner pushes to `target_replicas` peers — and it
fails on the question that matters: *whose disk is this, and did they agree?* Pushing means
inventing a consent mechanism, an accounting of who agreed to what, and a way to refuse.
Pulling means **the holder decides, so consent is inherent** and none of that machinery
exists to get wrong.

It also reuses a shape this system already has. ADR-0020's recipient asks a peer what has
been sealed to it; a replica asks a peer what is available to hold. Same round trip, same
per-session snapshot, same privacy discipline.

### 2. Therefore replication is best-effort, and the documents must say so

If nobody asks, nothing is replicated. `target_replicas: 3` is **a wish, not a guarantee**,
and an object may sit at zero replicas indefinitely.

This is stated rather than engineered around. Guaranteeing a replica count requires either
pushing onto unwilling disks or paying somebody to hold it, and the first is refused above
while the second is a contract this OS does not have. A UI that shows "3 replicas" as though
it were a setting is lying; it may show how many replicas this node has *heard about*, which
is a lower bound and is honest.

### 3. The owner cannot count its replicas, and asking would leak

There is no reliable replica count. A node could ask peers "do you hold this?", and the
answer would be a truthful lower bound — but the question, asked across a cluster, builds a
map of who holds what, which is the graph ADR-0015 and ADR-0020 both took care not to
publish.

So: an owner learns of replicas only from nodes that volunteer the fact, and the number is
always a floor. **Under-replication is invisible by design**, which is a real cost and the
honest consequence of not surveilling the cluster.

### 4. What the object record carries

The block from `DATA-VISIBILITY.md` §3, with one field removed and the rest given meanings
that survive §1:

| Field | Meaning under pull |
|---|---|
| `target_replicas` | How many copies the owner *hopes* for. Advisory; a holder may take an object already at target, and nothing prevents it |
| `ttl_days` | After this, a holder should drop it unless it is re-offered. The only bound on unbounded growth |
| `max_size_bytes` | A holder refuses anything larger, regardless of budget. A cheap guard against one object filling a small node |
| `allow_rereplication` | Whether a *replica* may itself offer the object onward. This is how content outlives its origin, and it is the field with teeth |

**`max_hops` is dropped.** It counts distance from an origin, which is a push concept: under
pull there is no chain to count, only a holder that offers and a holder that takes. Keeping
a field nobody could compute correctly would be worse than not having it.

### 5. `allow_rereplication: false` is a request, not a control

An object that forbids re-replication is asking each holder to be a leaf. Holders that
respect it are the ones that keep the network healthy; a holder that ignores it is a peer
running modified software, and **nothing stops that** — it already has the bytes.

Said plainly because the alternative is a UI that implies control the system does not have.
The label model is about what *this* node will do, and it has never been about what other
people's computers do. That is the same honesty `store.demote` already applies.

### 6. Nothing about ranking, and deliberately

A holder chooses what to pull. It may choose randomly, oldest-first, or smallest-first.
**It may not choose by peer capability**, and no peer advertises its power here.

This ADR is being written while `docs/roadmap/CLUSTER-VISION.md` proposes ranking nodes by
benchmark, and the interaction is worth heading off: replication that preferred powerful
peers would concentrate the network's durable copies on its largest nodes, which is how a
decentralised store quietly becomes three data centres with extra steps. If ranking arrives,
it arrives with its own ADR and argues that case explicitly.

### 7. How a holder discovers offers: one ONM request, in ADR-0017's shape

**Amended 2026-08-26, on building it.** §1 needed a mechanism and left it to the next ADR;
it turned out small enough to belong here.

`content.replicable` asks a peer what it is willing to have copied. It mirrors ADR-0020's
`content.shared_with_me` — paged by content id, bounded by the same `max_entries` ceiling,
and **taken once per session**, because producing the list scans every object record and a
peer that could force a fresh scan per request would have a cheap way to make an
SD-card-backed node miserable. The same cost follows: an object marked `REPLICATED` during
a session is not visible to that session.

**The reply is the same for every asker**, which is the one place it differs from the
sharing index. `REPLICATED` means copying is permitted, so there is nothing to scope — and
therefore no filter that could be got wrong. ADR-0020 had to scope its computation to the
asker precisely because a bug there would publish the store; here the question does not
arise.

Each entry carries the content id, the size, and the parts of the policy a holder acts on:
`ttl_days`, `max_size_bytes`, `allow_rereplication`.

**`target_replicas` is deliberately not sent.** A holder cannot count replicas (§3), so it
could not act on the number, and putting a figure on the wire that nobody can use is an
invitation to build a UI on it.

**The request is `content.replicable`; the reply is tagged `replicable`.** The method name
belongs on the question, and every other reply in ADR-0017's protocol is a bare noun
(`manifest`, `chunk`, `shared_with_you`, `not_available`). The first version of this code
tagged the reply with the method name too, and that was found by writing the JSON Schema
rather than by any test — which is the argument for CLAUDE.md §4.2 in one line: a contract
written down is read by something other than the code that produced it.

### 8. A replica is a cache entry with an expiry

**Amended 2026-08-26, on building the holder side.**

A replica lives in the cluster cache rather than in a store of its own. It reuses the
budget, the encryption at rest, the refcounted chunks and the serving path that ADR-0015
already built; the only thing replication adds is a reason not to evict it yet.

**The hold is separate from an operator's pin**, and that separation is the part worth
stating. Both mean "do not evict", but a pin is indefinite and a hold expires — folding them
into one flag would make a TTL sweep silently unpin something a person chose to keep. Two
fields, either sufficient to keep an object.

**Expiry releases the hold; it does not delete the object.** The bytes become an ordinary
cache entry, evictable when the budget needs room. A node that fetched something recently
should not lose it the instant a TTL lapses, and letting the cache's own LRU decide is both
cheaper and kinder than a second deletion policy.

`take_replica` applies **both** rules — the owner's `max_size_bytes` and this node's own
remaining budget — so no caller can remember one and forget the other. Refusing returns
`Ok(None)` rather than an error: on a small node "not this one" is the normal answer and
not something to escalate.

### 9. A node asks on connection, never on a timer, and takes at most one object

**Amended 2026-08-26, on wiring the last piece.** §1 said a holder asks; this says when.

**On connection.** When a node authenticates a peer and has replication enabled with budget
to spare, it asks that peer once for offers. Not on a timer, and not in the background.

Four reasons, and the first is the one that decides it:

- **A timer that runs while offline is a timer that does nothing, badly.** Prime directive 2
  says the system works with the cable out; asking on connection means an offline node does
  no replication work at all, rather than waking up to discover it has no peers.
- **The offer index is already per-session** (§7). Asking twice in one session returns the
  same snapshot, so a timer would either re-ask pointlessly or force new handshakes to get
  fresh answers — and a handshake per scan is exactly the price §7 chose.
- **It needs no configuration.** No interval to tune, no jitter to get wrong, nothing for an
  operator to set differently on two nodes and then wonder why they diverge.
- **It is naturally rate-limited.** Replication work is bounded by how often this node meets
  peers, which is the same bound ADR-0020 §4 relies on.

**At most one object per connection.** A node that took everything on offer could have its
whole replication budget filled by the first peer it meets, which is neither fair to other
peers nor what "spread across the cluster" means. One per connection converges over time and
degrades gracefully — it is slower to reach `target_replicas`, and §2 already said that
number is a wish.

**Nothing is asked when replication is off or the budget is full.** The capability engine's
`content_replication` gate is the operator's consent and is checked before any request goes
out, so a node that does not replicate makes no replication traffic at all rather than
asking and discarding the answer.

**Expiry is swept on the same trigger.** Releasing lapsed holds is cheap and needs no timer
either; doing it when a connection happens keeps the whole subsystem free of background
work, which on an SD-card-backed T0 board is worth more than promptness.

### §10 The pass runs in the network daemon, and the cache stays in one process

**Amended 2026-08-27, on wiring the trigger.** §9 said "on connection" without saying which
process. It matters, because the two halves of a replication pass live in different daemons.

The link is `otwono-netd`'s: it is the only process that speaks Noise to a peer, and by the
security model it is the one deliberately *not* given the storage key. The cluster cache is
`otwono-stored`'s: that daemon opens it at startup, holds its index in memory, and persists
on every change.

So `otwono-netd` **must not open the cache itself**. Two processes each holding their own
in-memory copy of one on-disk index lose each other's updates and double-count the same
budget — a corruption bug that would show up as a cache that overruns its limit and then
forgets objects it is still serving. CLAUDE.md §2.4 says the same thing in the general case:
cross-subsystem calls go through the Local Control Plane, never through shared mutable state.
A file on disk that two daemons both write *is* shared mutable state, whatever it looks like
in the type system.

**The pass is therefore generic over where the replica goes.** `ReplicaHolder` (in
`otwono-store`) is the two questions and one action a pass needs: how much room is there,
which of these do you already have, and will you take this. `Cache` implements it directly —
which is what the tests drive, in-process, with no daemon. `otwono-netd`'s `BrokeredCache`
implements it over the control plane against `otwono-stored`. Neither implementation knows
about the other, and the pass logic — the ordering, the smallest-that-fits choice, the
label re-check — exists once.

**Two new control-plane methods**, mirroring `cache.put`'s existing shape and its inline size
cap:

- `cache.replica_room` — sweeps lapsed holds, then answers with the room remaining and which
  of the offered ids this node already has. Called *before* anything reaches the wire, so §9's
  "a node that does not replicate makes no replication traffic at all" survives the split.
  Bounded: it answers about the ids it was asked about, and does not return the cache listing.
- `cache.take_replica` — `cache.put` plus the hold, applying the owner's policy inside the one
  process that can see the budget.

**A new capability, `cache.replicate`, not `cache.write`.** These are different authorities
and an operator must be able to hold one without the other. `cache.write` is *keep what I
fetched* — the bytes are already on the machine because someone asked for them. `cache.replicate`
is *keep what a stranger offered* — bytes that arrive because a peer had them, on a node that
may be unattended. "Cache what I fetch, but do not host for strangers" is a sentence a person
will want to say, and only a separate capability lets them say it.

It is `Reversible` and not `always_confirm`, for the reason §9 gives: a replica is disposable
by construction, and a node that had to prompt before holding one could not replicate while
unattended, which is the only condition under which replication is worth anything.

**What this does not change.** The choice of *what* to take is still the holder's and still
smallest-that-fits (§9); the owner still cannot count replicas (§3); nothing new is pushed.
The split is about which process holds the bytes, not about who decides.

## Consequences

**Good.** No consent mechanism to design, because nothing arrives unasked. No push
scheduler, no delivery retries, no per-peer state on the owner. Content outlives its origin
when `allow_rereplication` permits, which is what the label is for. It degrades to today's
behaviour — serve on request, copy nothing — when nobody has budget.

**Bad, and worth naming.**

- **An object may never be replicated at all**, and its owner will not reliably know.
- **A popular object gets many replicas and an unpopular one gets none**, since holders
  choose. Availability follows interest, not need — the opposite of what an archive wants,
  and archival is a different design.
- **TTL means replicas evaporate** unless re-offered, so durability needs a live network
  rather than a one-time copy.
- **A holder learns what it holds**, which for `REPLICATED` is fine — the label means
  public copying is permitted — but it is a per-peer record of who was willing to host
  what, and ADR-0015's "holding is publishing" applies with full force.
- **Nothing here bounds aggregate cluster storage.** Each node bounds its own budget, and
  the sum is whatever the participants chose.

## Alternatives rejected

- **Push to `target_replicas` chosen peers.** The obvious design, and the reason
  `target_replicas` reads like a guarantee. Requires a consent protocol, a refusal path,
  retry state, and an answer to "whose disk is this" that pull gets for free. §1.
- **Owner queries peers to count replicas**, so `target_replicas` can be enforced. Builds
  the who-holds-what map the rest of the system avoids. §3.
- **Enforce `allow_rereplication` cryptographically.** Cannot be done: the holder has the
  bytes. Anything claiming otherwise is DRM with the same track record. §5.
- **Keep `max_hops`.** Specified in `DATA-VISIBILITY.md` §3 and meaningless under pull.
  Dropped rather than kept as a field that is always zero.
- **Prefer capable peers as replicas**, for faster serving later. Concentrates durability on
  the largest nodes. §6.
- **Let `REPLICATED` keep behaving as `PUBLIC`.** Zero work, and leaves a label in the model
  that does nothing — which is worse than not having the label.

## What is deliberately not decided

- **How a holder discovers offers** — a new ONM request in ADR-0017's shape, or riding
  ADR-0020's index. It is the next ADR and the first slice of code.
- **How a holder chooses** among more offers than it has budget for.
- **Whether replicas are counted for the contribution/reward system.** They are a
  contribution by any reasonable reading, and the reward design must handle "storage held
  over time" differently from "bytes served". Not answered here.
- **Archival**, meaning deliberate durability for content nobody is asking for. It is the
  case this design serves worst, and it needs its own ADR.

## References

- `docs/security/DATA-VISIBILITY.md` §1 and §3 (the label and the policy block this
  narrows), ADR-0015 (the cluster cache, and "holding is publishing"), ADR-0020 (the
  ask-a-peer shape this reuses), CLAUDE.md §2.6 (`content_replication` lives in the
  capability engine), `docs/roadmap/CLUSTER-VISION.md` §8 (the ranking interaction §6 heads
  off).

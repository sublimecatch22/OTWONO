# ADR-0028 — Addressed messages: the envelope already exists, so this is about who carries it while you are away

**Status:** accepted
**Date:** 2026-08-27
**Phase:** 6 (first distributed services)

## Context

`DISTRIBUTED-SERVICES.md` §1 names three primitives. Two are built and verified between
booted nodes: content-addressed blocks (ADR-0016) and signed mutable pointers (ADR-0027).
The third is "addressed messages — end-to-end encrypted envelopes to a NodeID or UserID,
with store-and-forward for offline recipients", and it blocks the rest of Phase 6.

### Most of it is already built, and naming that changes what this ADR is about

The obvious reading is that primitive 3 means building encrypted messaging. It does not.
ADR-0019 and ADR-0020 together already give:

- a payload encrypted under a fresh per-object key,
- that key wrapped per recipient by X25519 to the recipient's **sharing key**, which
  `otwono-idd` vouches for and which travels in the `Hello` after every Noise handshake,
- and a recipient that **discovers** what was sealed to it over the mesh, fetches it, and
  opens it, with no identifier passing by any other route.

That is an end-to-end encrypted envelope addressed to a NodeID. It is verified between two
booted nodes. Building a second one would be exactly the rewrite CLAUDE.md §2.3 forbids.

What is missing is the clause after the comma. `content.shared_with_me` is a question asked
of **the sender**, answered from the sender's own store. So delivery requires both parties
online at once, which is the one thing a message is supposed not to require. A person whose
laptop is shut when the message is sent never receives it, and the sender has no way to know.

So this ADR is about custody: **who holds an envelope while its recipient is away.**

## Decision

### 1. An envelope is a sealed object with an address and an expiry

No new cryptography, no second sealing path. An envelope is ADR-0019's sealed object with
exactly one recipient, plus two fields the relay needs to do its job and can read without
opening anything:

```
envelope_id   the content id of the ciphertext, as for any object
recipient     the NodeID this is for
expires_at_ms absolute, chosen by the sender, capped by whoever carries it
```

The sender is **not** a field. It is inside the ciphertext, where only the recipient can
read it. See §4 for what that does and does not buy.

### 2. Store-and-forward is pulled, not pushed

This is the decision the rest follows from, and it is the one that had to be reconciled with
ADR-0026 rather than waved past.

ADR-0026 §1 says: "A replica is pulled by its holder. Nothing is ever pushed... the holder
decides, so consent is inherent." Store-and-forward reads like the counterexample — the
sender hands a message to a relay — and if it were built that way it would need precisely
the machinery ADR-0026 avoided: a consent mechanism, an accounting of who agreed to carry
what, and a way to refuse. On a network of strangers, "anyone may push bytes onto your disk"
is not a messaging feature, it is a free disk.

So the relay pulls, exactly like a replica holder:

1. A node with undelivered envelopes **offers** an index of them — recipient, size, expiry.
2. A node willing to relay **asks** for that index when it meets a peer, and takes what it
   chooses, within its own budget.
3. A recipient **asks** whichever peers it meets what they are holding for it, and takes it.

Nobody is ever sent anything they did not ask for, at either hop. Consent stays inherent,
the budget and eviction rules are the cache's, and the pass has the shape
`replication_pass` already has — the difference is that the offer is filtered by address
rather than by label.

The alternative — a push with a consent protocol — was rejected on the same grounds
ADR-0026 rejected it, and more sharply here, because the pushed object would be chosen by a
stranger rather than by someone whose content you had opted into replicating.

### 3. Expiry is absolute and a relay never extends it

A replica's TTL restarts when the object is offered again, because the point of replication
is durability: content should outlive its origin (ADR-0026 §5).

A message is the opposite. It has a moment, and after it the envelope should **stop
existing** — on the relay's disk and everywhere else. So `expires_at_ms` is absolute, set
once by the sender, and a relay may shorten it to its own ceiling but never lengthen it. An
envelope past its expiry is dropped whether or not it was delivered, and whether or not it
was ever offered again.

This is what stops a relay network becoming an accidental permanent archive of everyone's
undelivered mail, which is both a storage problem and a much worse privacy one.

### 4. A relay learns the recipient. It does not learn the sender

A relay has to know who an envelope is for — it cannot decide whether to carry it, or who to
hand it to, otherwise. So it learns: **recipient, size, and timing**. That is real metadata
and it is stated here rather than glossed.

It does not learn the sender, because the sender is inside the ciphertext. That asymmetry is
free — the relay has no use for the sender — and it is worth having: it means a relay
observing a busy node learns that node receives traffic, not who its correspondents are.

What this does **not** provide is protection against a relay that is also one of your peers
and can correlate. Nor against traffic analysis across several relays. Mixnet-grade
anonymity is not on offer here and claiming otherwise would be worse than not having it.

### 5. Delivery is best effort, and the sender is not told

A relay may drop an envelope under budget pressure, may never meet the recipient, may be
running modified software, or may simply refuse. Nothing in this design prevents any of
that, and no amount of protocol would.

There is deliberately **no delivery acknowledgement** in this slice. An ack is a second
envelope with all the same problems, and it hands the sender a timing oracle for when the
recipient came online — which is exactly the metadata §4 works to keep away from relays,
handed instead to the one party guaranteed to be interested in it.

A sender that needs to know a message arrived should get that from a reply written by a
person, which is the only acknowledgement that ever meant anything.

### 6. Replay is not the pointer problem

ADR-0027 needed sequence numbers because a pointer names something *current*, so an old
record served again is a lie about the present.

An envelope names nothing. It is immutable, content-addressed, and true whenever it is read.
A relay that serves the same envelope twice has handed over a duplicate the recipient
discards by id, and one that serves a three-day-old envelope has done its job. So there is no
sequence, no reader-held state, and none of ADR-0027's machinery here. Staleness is bounded
by §3's expiry and nothing else needs bounding.

### 7. A carrier may re-relay, and it could not have been otherwise

**Settled 2026-08-27.** The question as posed — *may a relay offer onward what it is
carrying?* — presupposes that a carrier can tell an offer from the sender apart from an offer
from another carrier. **It cannot.** §1 removed the sender field and there is no hop count,
so the two cases are indistinguishable in the record. Re-relay is not a permission that could
be granted or withheld; it is what the descriptor's shape already implies.

Which leaves the real question: should a distinguisher be added so the choice becomes
available? No, and the reason is §4.

A hop count is the obvious candidate, and it leaks. An envelope arriving with its counter at
the maximum has travelled nowhere, so the carrier receiving it is almost certainly talking to
the sender. That hands every relay a decent guess at exactly the thing §4 works to keep away
from it. A field whose purpose is to bound amplification would end up de-anonymising the
sender, which is a bad trade for a bound that only honest carriers respect anyway.

So amplification is bounded by three things instead, and only one of them asks a carrier to
be honest:

- **Absolute expiry** (§3). A hard stop that no carrier can extend.
- **Each carrier's own budget and size cap.** The bound that matters, and the one nobody
  else can forge: a carrier holds what it agreed to hold and no more. No node can be harmed
  past the limit its own operator set, which is ADR-0026's consent argument unchanged.
- **Drop on delivery.** A carrier that has handed an envelope to its recipient drops it.
  **Built 2026-08-28** as `content.delivered`, reported by the recipient after the bytes are
  on its own disk and never before — `CARRIAGE.md` §3b has the mechanism and the ordering
  argument. It reaches only the carrier the recipient collected from; earlier hops keep their
  copies until they lapse.

That last one is free, and it is worth being precise about why, because §5 refuses an
acknowledgement on privacy grounds and this looks like one. It is not. A carrier that hands
an envelope over has just spoken to the recipient — it already knows the recipient is
reachable, and dropping the envelope tells it nothing further. **Local delivery knowledge is
free; end-to-end delivery knowledge is not.** §5's refusal is about telling the *sender*,
who learned nothing by being there.

The cost of allowing re-relay, stated plainly: **every hop is another party that learns the
recipient, the size, and the timing.** That is inherent to carrying mail through strangers
and cannot be designed away while multi-hop delivery is wanted — and it is wanted, because a
single-hop rule would mean delivery only ever happens when one carrier meets both parties,
which on a sparse or mobile mesh is most of the time never.

Note what makes this different from ADR-0026 §5, where `allow_rereplication: false` exists as
a request. A replica's audience is everyone, so an owner may reasonably want holders to be
leaves. An envelope's audience is exactly one node. There is nothing to scope, and no
sensible thing for a sender to ask for.

### 8. Carrying mail is a separate budget and a separate capability from the cluster cache

**Settled 2026-08-27.** Two budgets, not one, and `envelope.carry` in the broker rather than
riding on `cache.replicate`. Four reasons, and the first is on its own sufficient.

**Consent.** `cache.replicate` means "hold some of the neighbourhood's content" — content
that is `PUBLIC` or `REPLICATED`, that the operator can inspect, and that they can purge on
sight. An envelope is opaque ciphertext addressed to a stranger. Those are different things
to agree to, and one budget would mean that granting the first silently enrols an operator in
the second. That is the failure mode ADR-0026 §10 built a separate capability to avoid, and
the reasoning carries here without modification.

**Eviction means different things.** A cache entry is a convenience copy: evicting it costs
somebody a round trip to fetch the object from its origin, which still exists. Evicting an
envelope may mean a message is never delivered, and **nobody finds out** — not the sender
(§5), not the recipient, not the operator. Under one eviction policy those compete, and the
cache wins by construction: it is refreshed by traffic and the envelope is not.

**The lifetime rules are opposites.** A replica's TTL restarts when the object is re-offered,
because replication is about durability (ADR-0026 §5). An envelope's expiry is absolute and
can only be brought closer (§3). One store enforcing two contradictory lifetime rules is
exactly the sort of thing that gets confused a year later by somebody adding a sweep.

**They exhaust differently.** Cache objects are content-addressed and shared: a hundred nodes
wanting one object cost one copy. Envelopes are sealed per recipient under a fresh key, so
the same message to two people is two ciphertexts with two ids and no deduplication at all.
Sizing one number for both behaviours would mean sizing it wrong for at least one.

The number itself comes from **one place**, as CLAUDE.md §2.6 requires: a new
`envelope_carry_bytes` on `FeatureGates`, derived per tier alongside `cluster_cache_bytes`
and zeroed on a storage-constrained machine by the same rule. The gate and the capability
answer different questions and both must pass — the gate says what this machine can afford,
the broker says what its operator permits.

### 9. Two wire methods, because a carrier and a collector ask different questions

**Settled 2026-08-27**, on building the wire and finding that one method would have leaked.

The obvious shape is a single `content.relayable` — "what are you holding?" — serving both the
carrier taking custody and the recipient collecting. It conflates two genuinely different
questions, and the conflation is a metadata surface §4 did not price in.

- **A carrier** asking what it might take custody of needs the *whole bag*, because it is
  deciding about envelopes addressed to people it has never met. §7 accepts that cost: every
  hop is another party that learns recipients.
- **A recipient** collecting needs only what is addressed to *it*, and has no business
  learning who else the carrier serves.

One unscoped method would give every peer that can complete a handshake the ability to
enumerate every recipient a carrier holds mail for — **without carrying anything**. §4 says a
relay learns the recipients *of what it carries*; an open enumeration endpoint is strictly
broader, and it follows from §7's own reasoning that nothing distinguishes a genuine carrier
from somebody reading the mailbag.

So:

- **`content.relayable`** — the custody-transfer question, answered the same for every asker,
  paged like `content.replicable` and taken once per session for the same reason (ADR-0026 §7).
  This is the broad path, and it is the one §7 reasoned about.
- **`content.addressed_to_me`** — the collection question, scoped to the authenticated asker,
  mirroring ADR-0020's `content.shared_with_me` exactly. A node that only wants its own mail
  never sees anyone else's.

This does **not** close the enumeration hole on the carrier path, and cannot while §7 stands:
re-relay requires exposing recipients to prospective carriers. What it does is stop the
*collection* path — the one every ordinary recipient uses — from being an enumeration oracle,
and make the broad path explicitly the one the ADR reasoned about rather than an accident of
having written one method instead of two.

An empty reply from a carrier holding nothing for the asker is identical to an empty reply
from a node that carries nothing at all, per ADR-0020's rule: asking must not be a way to find
out whether a node carries.

### 10. Expiry is evaluated on the carrier's clock, from the moment it took custody

**Settled 2026-08-27.** §3 made expiry an absolute wall-clock timestamp and said nothing about
whose clock. ADR-0027 §2 had already rejected wall clocks for *ordering* on the grounds that
"on a mesh with no NTP guarantee that is not a rare edge case", and the same absence of NTP
applies here.

Unaddressed, a carrier whose clock is badly wrong either refuses every envelope on receipt or
holds past every expiry, and **nothing detects either** — a node that silently discards all
mail looks identical from outside to one nobody is sending to.

So a carrier commits to a deadline of its own at the moment it accepts custody:

```
until_ms = min(sender's expires_at_ms, took_at_ms + carrier's max_hold_ms)
```

and **sweeps against that stored value**, not against the sender's field re-read later. Two
consequences worth stating:

- The sender's expiry remains a ceiling and is never exceeded (§3 is unchanged).
- A carrier with a skewed clock still drops the envelope in bounded time, because the second
  term is measured from *its own* custody moment rather than from a comparison between two
  clocks that disagree.

What this does not fix: a carrier whose clock is far *ahead* still refuses on receipt, because
the sender's expiry looks already past. That is the gross-skew case, it is recorded in
`OPEN-QUESTIONS.md`, and treating it needs something that can detect skew rather than merely
tolerate it.

## Consequences

**Good.** No new cryptography and no second envelope format — this composes ADR-0019's
sealing, ADR-0020's discovery, and ADR-0026's pull-and-budget machinery. A T0 node can
relay, which `DISTRIBUTED-SERVICES.md` §4.4 already asked for. An envelope is small enough
for a `Trickle` link, which §4.2 requires of messaging natively.

**Bad, and worth naming.**

- **Delivery is not guaranteed and failure is silent.** §5. This is a real limitation of a
  network with no always-on infrastructure, not a gap to be closed later by trying harder.
- **Relays see recipients and timing.** §4.
- **A relay spends its disk on strangers' traffic.** Bounded by its own budget and by
  expiry, and entered into deliberately, but it is a cost with no direct return — the same
  bargain as the cluster cache, and it needs saying in the UI before anyone enables it.
- **Every hop is another party that learns the recipient, size, and timing** (§7). The price
  of multi-hop delivery, and it cannot be designed away while multi-hop delivery is wanted.
- **No ordering.** Two envelopes may arrive in either order, and nothing here fixes that. A
  service that needs ordering must carry its own sequence inside the ciphertext.
- **NodeID only.** `DISTRIBUTED-SERVICES.md` §1 says "NodeID or UserID". A person with a
  laptop and a phone has two NodeIDs and must be messaged twice. This is the same
  two-devices question ADR-0027 §3 deferred, and it is deferred here for the same reason:
  it is a question about what a "user" is, not about envelopes.

## Alternatives rejected

- **Pushing an envelope to a relay.** §2. Needs a consent mechanism, an accounting, and a
  refusal path — all of which ADR-0026 removed by making the holder decide.
- **A second, message-specific encryption path.** Would mean two sealing implementations,
  two places for a mistake, and a rewrite of something verified between booted nodes.
- **Delivery receipts in this slice.** §5. A timing oracle handed to the party most
  motivated to use it.
- **Sequence numbers on envelopes.** §6. Solves a problem immutable records do not have.
- **A designated relay or set of relays.** Concentrates metadata and creates the
  infrastructure this OS exists not to require. Any peer may relay, or none.
- **TTL that restarts on re-offer**, as replication has. §3. Turns a message network into an
  archive nobody asked for.
- **A hop count to bound amplification.** §7. An envelope whose counter is untouched has
  travelled nowhere, so the field would tell every carrier it is probably talking to the
  sender — de-anonymising the one party §4 protects, to buy a bound that only honest
  carriers respect. The budget bounds it already and nobody can forge somebody else's
  budget.
- **Single-hop carriage** — a carrier takes only from the sender. §7. Delivery would then
  need one carrier that meets both parties, which on a sparse or mobile mesh is usually
  never, and it is unenforceable anyway since the record cannot distinguish the cases.
- **One budget shared with the cluster cache.** §8. Granting `cache.replicate` would
  silently enrol an operator in carrying strangers' opaque mail, and one eviction policy
  cannot serve a convenience copy and an undeliverable message at once.

## What is deliberately not decided

- ~~**Whether a relay may re-relay.**~~ **Settled 2026-08-27**, §7. Yes, and the record's
  shape already decided it.
- ~~**How a relay's budget for envelopes relates to the cluster cache's.**~~ **Settled
  2026-08-27**, §8. Two budgets, and its own capability.
- **Forward secrecy.** The sharing key is long-lived, so compromising it opens every
  envelope ever sealed to it. A ratchet is the answer and it needs a design of its own.
- **UserID addressing**, per the consequence above.
- **Ordering and grouping**, per the consequence above.

## References

ADR-0019 (the sealing this reuses), ADR-0020 (discovery, and why a recipient can find what
was sealed to it), ADR-0026 (pulled not pushed, and the budget machinery §2 borrows),
ADR-0027 (why §6 is *not* like the pointer case), `docs/services/DISTRIBUTED-SERVICES.md`
§1 and §4, `docs/security/DATA-VISIBILITY.md`.

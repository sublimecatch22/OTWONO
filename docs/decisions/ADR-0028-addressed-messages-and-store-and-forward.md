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

## What is deliberately not decided

- **Whether a relay may re-relay.** ADR-0026 §5 makes onward replication a request rather
  than a control; whether the same applies to envelopes needs its own thinking, because
  "content outlives its origin" is a virtue for a replica and a hazard for a message.
- **Forward secrecy.** The sharing key is long-lived, so compromising it opens every
  envelope ever sealed to it. A ratchet is the answer and it needs a design of its own.
- **UserID addressing**, per the consequence above.
- **Ordering and grouping**, per the consequence above.
- **How a relay's budget for envelopes relates to the cluster cache's.** One budget or two
  is a capability-policy question and belongs with whoever builds the pass.

## References

ADR-0019 (the sealing this reuses), ADR-0020 (discovery, and why a recipient can find what
was sealed to it), ADR-0026 (pulled not pushed, and the budget machinery §2 borrows),
ADR-0027 (why §6 is *not* like the pointer case), `docs/services/DISTRIBUTED-SERVICES.md`
§1 and §4, `docs/security/DATA-VISIBILITY.md`.

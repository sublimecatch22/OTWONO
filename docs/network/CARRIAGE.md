# Carrying Mail: store-and-forward on the ONM

**Status:** `VERIFIED`. The custody rules, both wire methods, the carry pass, collection and
the daemon plumbing exist, are covered by unit and integration tests over a real Noise
channel, and on 2026-08-28 a three-node QEMU run showed an envelope sealed by one node,
carried by a second, and collected *and opened* by a third with the sender powered off for the
whole collection.

That does not make every part of this document verified. Drop on delivery is not implemented,
nothing on the receiving side collects unprompted, and the run's take was driven by the CLI
rather than by the daemon's own sweep — see §7 and `docs/build/VERIFICATION-LOG.md` for what
the runs have and have not demonstrated.

This document describes the third of `DISTRIBUTED-SERVICES.md`'s three primitives. The
decisions are ADR-0028's; this is how they are built.

---

## 1. What was already there

Most of "addressed messages" predates this work, and saying so changes what the rest of the
document is about. ADR-0019 and ADR-0020 together already give:

- a payload encrypted under a fresh per-object key,
- that key wrapped per recipient by X25519 to the recipient's **sharing key**, which
  `otwono-idd` vouches for and which travels in the `Hello` after every Noise handshake,
- and a recipient that **discovers** what was sealed to it over the mesh, fetches it, and
  opens it, with no identifier passing by any other route.

That is an end-to-end encrypted envelope addressed to a NodeID, and it is verified between
booted nodes. What was missing is that `content.shared_with_me` is a question asked of **the
sender**, answered from the sender's own store — so delivery required both parties online at
once, which is the one thing a message is supposed not to require.

So carriage is about **custody**: who holds an envelope while its recipient is away.

## 2. The shape: a replication pass with an address

A carry pass is `replication_pass` with the offer filtered by address instead of by label,
and it is deliberately the same code shape. Both answer "a node with room meets a peer and
takes at most one thing", and a second pattern for one problem is two places to get the budget
arithmetic wrong.

```
carrier                                     holder of undelivered mail
   |                                                        |
   |-- content.relayable { after, max_entries } ----------->|
   |<-- carried { entries: [{envelope_id, recipient,        |
   |              size_bytes, expires_at_ms}, ...] } -------|
   |                                                        |
   |   decides: within budget? not already held?            |
   |   not addressed to me? soonest deadline?               |
   |                                                        |
   |-- manifest + chunks (ADR-0017, unchanged) ------------>|
   |<-- the sealed ciphertext, and the content key sealed --|
   |    to the *recipient* (§4)                             |
   |                                                        |
   |   store.accept_shared -> keeps the ciphertext          |
   |   envelope.take -> custody until min(sender's expiry,   |
   |                    now + this carrier's max hold)      |
```

**The bytes are kept before custody is claimed**, and the order is load-bearing: custody of
bytes a node does not hold is a promise it cannot keep. A carrier that recorded custody and
dropped the ciphertext would count the envelope against its budget, offer it onward, and have
nothing to serve when the recipient finally came. The first implementation did exactly that,
and it took three booted nodes to notice, because a carrier holding nothing is
indistinguishable from a carrier holding something until somebody asks for the object.

**Pulled, never pushed** (ADR-0028 §2). The consent check happens before anything reaches the
wire: a node that carries no mail makes no carriage traffic at all, rather than asking and
discarding. That is ADR-0026 §1's rule kept structural rather than left to a check somebody
could forget.

**At most one envelope per pass**, so the first peer a node meets cannot fill its whole
budget. The choice among offers is **the soonest to expire**, which differs from replication's
smallest-first on purpose: a replica is about durability and any of them will do, while an
envelope has a deadline and the one closest to missing it is the one worth moving.

## 3. Two questions, not one

`content.relayable` and `content.addressed_to_me` return the same shape and answer different
questions (ADR-0028 §9).

| | asks | answered with |
|---|---|---|
| `content.relayable` | what may I take custody of? | everything this carrier holds |
| `content.addressed_to_me` | what are you holding for me? | only what is addressed to the authenticated asker |

The split is not tidiness. One unscoped method would let **any peer that can complete a
handshake enumerate every recipient a carrier holds mail for**, without carrying anything —
strictly broader than ADR-0028 §4's "a relay learns the recipients of what it carries". The
split does not close that hole on the carrier path, which §7 requires to stay open, but it
stops the path every ordinary recipient uses from being an enumeration oracle.

**The scoping happens in `otwono-stored`, not in `otwono-netd`.** The daemon answering a
scoped question never receives the full bag, so it cannot leak one through a mistake in its
own filtering.

Neither question distinguishes "nothing for you" from "I will not say": both are an empty
page, per ADR-0020's rule that asking must not reveal whether a node carries at all.

## 4. Custody is what authorises serving

A carrier is by definition **not** the recipient, and ADR-0019's serving rule admits only the
recipient — `may_go_to_peer` compares the sealed key's recipient against the asking peer.
Taken together those two rules make carriage impossible: a carrier could never obtain the
ciphertext it is meant to carry.

The exception lives in **both** daemons, and it has to. `otwono-stored` applies its own copy
of ADR-0019 §4 before `otwono-netd` ever sees the object, so an exception in the mesh daemon
alone changes nothing — the store has already answered "not available". The first
implementation had it in `otwono-netd` only, and the sender refused every carrier.

So a node may serve a shared object to any peer **when it holds a custody record for it**.
Carrying is exactly the act of holding another party's sealed bytes in order to pass them on,
and every hop sees only ciphertext plus a sealed key it cannot use.

This does not widen ADR-0019:

- Custody records are created by `envelope.take`, which needs `envelope.carry`, which a
  release image grants to nobody.
- An attacker cannot manufacture custody of somebody else's object to make a node serve it:
  the carry pass fetches the bytes *before* recording custody and verifies them against the
  content id, so anyone able to complete that already possessed the bytes.
- The exception applies **inside** the `SHARED`-and-own-store check, never beside it. A
  custody record is keyed by content id and `envelope.take` needs only a local capability, so
  an exception applied first would have made a `PRIVATE` object servable to anyone by taking
  custody of its id. It can only widen the audience of an object that was already going to
  leave the node sealed.

### Whose key travels

When a node serves an object it is *carrying*, the copy of the content key in the manifest is
the one sealed to **the recipient named in the custody record**, not to the peer asking. A
carrier is not on the recipient list and has no copy of its own; an envelope that reached it
without a key would be ciphertext nobody could ever open.

The carrier is correspondingly the one caller that fetches an object expecting a key sealed to
somebody else. Every other fetch requires the key to name *this* node, because a shared object
this node cannot open is a download thrown away — which is why the carry pass says so
explicitly rather than the check being relaxed for everyone.

## 5. Whose clock, and for how long

`expires_at_ms` is the sender's, absolute, and a **ceiling**. A carrier commits to

```
until_ms = min(sender's expires_at_ms, took_at_ms + this carrier's max hold)
```

at the moment it accepts, and sweeps against **that stored value** rather than re-reading the
sender's field later (ADR-0028 §10). The mesh has no NTP guarantee — ADR-0027 §2 rejected
wall clocks for ordering on exactly those grounds — so comparing a sender's instant against a
carrier's clock later is comparing two numbers that disagree for invisible reasons.

The second term is measured from this carrier's own custody moment, so a skewed clock changes
*when* an envelope is dropped, not *whether* it ever is. The gross-skew case remains open: a
carrier whose clock is far ahead refuses on receipt, because the sender's expiry looks already
past.

Expiry is absolute rather than a TTL that restarts on re-offer, which is the opposite of
replication (ADR-0026 §5). A replica should outlive its origin; a message should stop
existing.

## 6. Budgets and capabilities

Two gates, and **both** must pass:

- **`envelope.carry`** in the permission broker — what the operator permits. Deliberately not
  implied by `cache.replicate`: holding neighbourhood content you can inspect and purge is a
  different thing to agree to than carrying a stranger's sealed mail. A release image grants
  neither.
- **`FeatureGates::envelope_carry_bytes`** — what the machine can afford, from the capability
  policy engine and nowhere else (CLAUDE.md §2.6). Zero on a storage-constrained machine, for
  a sharper reason than the cache's: a carrier that runs out of room drops envelopes, and a
  dropped envelope is a message that may never arrive with nobody told.

See `docs/services/DISTRIBUTED-SERVICES.md` §3a for the per-tier figures and why the curve is
flatter than the cache's.

## 7. What is not built

- **Drop on delivery.** ADR-0028 §7 names it as one of three bounds on amplification. A
  carrier currently holds until expiry even after the recipient has collected. Closing it
  needs either an explicit release from the recipient — a third wire method — or per-envelope
  chunk-serving state, and neither is written.
- **Nothing collects automatically.** `content.addressed_to_me` exists and works, and the
  carriage sweep runs unprompted, but nothing on the receiving side ever asks. A node learns
  it has mail only when somebody runs `otwono-netd --collect`. The carriage half is a
  daemon; the collection half is a command, and until that is fixed "the message arrives"
  means "the message becomes fetchable".
- **Re-relay in practice.** §7 concludes that a carrier may pass an envelope on, and the
  record's shape makes it structural rather than a permission. No test exercises a second hop.
- **Forward secrecy.** The sharing key is long-lived, so compromising it opens every envelope
  ever sealed to it.
- **UserID addressing.** NodeID only; a person with two devices must be messaged twice.
- **Ordering.** Two envelopes may arrive in either order.

## References

ADR-0028 (the decisions), ADR-0019 and ADR-0020 (the sealing and discovery this reuses),
ADR-0026 (pulled not pushed, and the pass shape), ADR-0017 (the fetch protocol the ciphertext
moves over), `docs/services/DISTRIBUTED-SERVICES.md` §3a.

# Carrying Mail: store-and-forward on the ONM

**Status:** `VERIFIED`. The custody rules, both wire methods, the carry pass, collection and
the daemon plumbing exist, are covered by unit and integration tests over a real Noise
channel, and on 2026-08-28 a three-node QEMU run showed an envelope sealed by one node,
carried by a second, and collected *and opened* by a third with the sender powered off for the
whole collection.

A second run on 2026-08-28 showed both halves doing it **unprompted**: the carrier's sweep
took custody and the recipient's sweep collected, with no command run by hand on either.

A later run on 2026-08-28 added drop on delivery (§3b) and ADR-0031's carriage store, and the
harness asserted the whole round trip: sealed, carried unprompted, sender powered off,
collected and opened by the recipient's own sweep, and the carrier's custody store empty
again — the last of those a marker the test waits for rather than a line somebody read
afterwards.

The second hop is exercised too, but not on booted nodes: sender to A to B to the recipient,
over real Noise channels and real daemons, in
`an_envelope_crosses_two_carriers_and_only_the_last_is_told_it_arrived`. The three-node
harness builds a one-hop path.

A run after that one closed the gap those markers left. `OTWONO-ENVELOPE-DROPPED` watches the
custody store, which was already emptying before ADR-0031 moved the ciphertext to the cache —
so it looked the same whether the bytes went or stayed. The release now reports what it freed
and the carrier's check fails on zero: `released … (87 bytes freed)`, `DROPPED … freed=87`.

Expiry — the other way a carrier lets go, and the one that happens when the release does not
arrive — frees the same two things, and is covered by
`an_envelope_that_expires_frees_its_bytes_as_well_as_its_record` over real daemons.

That does not make every part of this document verified. No envelope has ever expired on a
booted node, the release's failure paths have only unit tests, and the freed count a booted
run reports is the daemon's own rather than an independent look at the disk. See §7 and
`docs/build/VERIFICATION-LOG.md` for what the runs have and have not demonstrated.

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

## 3a. Both halves are daemons

A node sweeps for mail addressed to it the way it sweeps for mail to carry: one peer per
turn, on the same thirty-second cadence, on the same thread that finds peers. Nobody has to
ask it to.

That was not true at first, and the asymmetry was easy to miss. The carriage half swept
unprompted from the day it was written while the receiving half waited to be told, so "the
message arrives" meant "the message becomes fetchable by a node that thinks to look" — and
the first three-node run reached its recipient only because the test harness ran
`otwono-netd --collect` itself.

Three things a sweep needs that a command does not:

| | |
|---|---|
| **Not re-download** | §3b's release is best effort, so a carrier that refuses it, answers something else or never hears it keeps the envelope to its deadline and keeps offering it. A sweep that did not check would refetch the same message every thirty seconds until then. `Inbox::holds` is the check, and in that case the only one. |
| **Not need `store.read`** | `otwono-netd` is the Z3 hostile-input daemon and does not hold it. So the check is `store.holds`, which answers one **named** content id with a bool and nothing else, guarded by `store.write`. That is the authority to avoid a redundant write, not the authority to read the store — see ADR-0030. |
| **Not ask when it cannot keep** | `Inbox::accepting` is checked before the connect, as carriage checks its budget before the connect. A node that dialled and then found it had nowhere to put the mail would have told a carrier it was interested for nothing. |

`otwono-netd --mail` asks what is waiting without fetching any of it. It is the read-only
half of the same question, and it is what lets an operator — or a test — tell mail that
arrived on its own apart from mail that was fetched by hand.

## 3b. Drop on delivery

ADR-0028 §7's third bound on amplification, and the only one that asks a node to be honest.
A carrier used to hold every envelope it took until the sender's expiry whether or not it had
been delivered, so a message with a week-long deadline sat on a stranger's disk for a week
after arriving.

`content.delivered { envelope_id }` is the release. It carries **no recipient field**, for the
reason `content.addressed_to_me` has none: the only recipient a carrier acts on is the one the
handshake authenticated, so a peer naming somebody else's envelope finds nothing in the scoped
index and is told `released: false`. The worst a peer can do with this is tell a carrier to
stop holding *its own* mail, which is its own business — and that is what makes an
unacknowledged, best-effort, unverifiable message safe to act on here.

`released: false` is also the answer for "I was not holding that", "I do not carry mail" and
"I will not say". Distinguishing them would make this a way to ask whether a given carrier
holds a given envelope for a given node, which is the enumeration question §3 split two
methods apart to avoid.

### The ordering is the safety argument

A recipient reports delivery **after** the bytes are on its own disk, never after merely
fetching them:

```
fetch  ->  verify against the content id  ->  store  ->  report delivered
```

The sender may be gone, so a carrier's copy can be the last one in existence. A release sent
on the strength of a fetch would lose the message permanently and tell nobody — §5 refuses to
acknowledge to the sender, so nothing anywhere would notice. `collect_from` therefore reports
inside the loop that keeps each object, after the keep has returned `Ok`, and a keep that
fails takes the whole call with it.
`a_recipient_that_could_not_store_its_mail_does_not_release_the_carrier` asserts exactly that,
and fails if the two are swapped.

Everything after the store is best effort. A carrier that refuses, answers something else, or
never hears the report keeps the envelope until its deadline — which is what every carrier did
before this existed, so the failure mode is the old behaviour rather than a new one.

### What it does not reach

Only the carrier the recipient collected *from*. If A handed the envelope to B and B to C, and
the recipient collects from C, then A and B hold their copies until they lapse. Telling them
would need either gossip or the sender's involvement, and §5 rules the second out.

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

### Where a carrier's copy lives

Not with this node's own objects. A carried envelope's ciphertext goes in the **cluster
cache**, and its custody record in the envelope store (ADR-0031).

The reason is that a carrier must be able to give the bytes back. The permanent content store
has no delete — nothing in this repository can remove an object from it — so a carrier that
put strangers' mail there freed the *record* on §3b's release and never freed a byte of disk.
Its footprint grew without bound at strangers' request, and `bytes_held` reported zero while
it happened, because it sums custody records rather than bytes. The cache has a budget, an
eviction policy and `remove`.

That means the serving rule above is really two rules, one per place:

- In this node's **own store**, a `SHARED` object is normally mail addressed to this node,
  which it can open, so ADR-0019's rule applies and the custody exception is the way past it.
- In the **cache**, a `SHARED` object is only ever a carried envelope — the ordinary cache
  door refuses the label and carriage's does not — so custody is the whole rule. A
  recipient's own mail cannot reach that branch, because it is never put there.

A carrier therefore needs a cache. `otwono-stored` refuses carriage outright on a machine
whose profile gives it a carriage budget and no cache budget, rather than accepting custody
all day and failing every keep.

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

- **Re-relay on booted nodes.** The second hop runs over real daemons and real Noise channels
  and has never run in QEMU: the three-node harness arranges one carrier, and a second would
  need a fourth node or a different power-off order.
- **Forward secrecy.** The sharing key is long-lived, so compromising it opens every envelope
  ever sealed to it.
- **UserID addressing.** NodeID only; a person with two devices must be messaged twice.
- **Ordering.** Two envelopes may arrive in either order.

## References

ADR-0028 (the decisions), ADR-0019 and ADR-0020 (the sealing and discovery this reuses),
ADR-0026 (pulled not pushed, and the pass shape), ADR-0017 (the fetch protocol the ciphertext
moves over), `docs/services/DISTRIBUTED-SERVICES.md` §3a.

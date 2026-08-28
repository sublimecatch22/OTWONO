# ADR-0031 — A carried envelope's ciphertext belongs in the cache, not the permanent store

**Status:** accepted
**Date:** 2026-08-28
**Phase:** 6 (first distributed services)

## Context

ADR-0026 §8 splits store-and-forward in two: `EnvelopeStore` holds the **custody record** —
which envelope, for whom, until when — and the sealed ciphertext is "an ordinary object" held
by the cluster cache, which already has a budget, a TTL, eviction, encryption at rest and
refcounted chunks. `crates/otwono-store/src/envelopes.rs` opened by saying so.

The implementation does not do that. `otwono-netd`'s `keep_sealed` calls
`store.accept_shared`, which writes to the **permanent content store**. And nothing in this
repository can delete an object from the permanent store: `Cache` has `remove` and `purge`,
the CAS has neither.

The consequences are all in one direction.

- ADR-0028 §7's drop on delivery frees the custody record and the carriage budget with it,
  and leaves the bytes on the carrier's disk for ever. Expiry does the same.
- `bytes_held` sums custody records, so a carrier that has delivered everything reports **zero
  bytes held** while holding every byte it ever carried. The budget is not wrong; it is
  measuring the wrong thing.
- Therefore a carrier's disk footprint grows without bound, at the request of strangers. That
  is the amplification ADR-0028 §7 exists to bound, arriving through the dimension §7 does not
  measure.
- And it grows in the wrong place. A carrier accumulates other people's ciphertext in the same
  store as the user's own objects, with no label distinguishing "mine" from "a stranger's mail
  I agreed to hold for six hours".

Found by reading, after drop on delivery was verified between booted nodes; no test catches
it, because there is nothing to assert against until it is fixed.

## The rule that is in the way

`Visibility::may_be_cached_for_peers()` is `Public | Replicated`. `SHARED` is excluded, and
for a good reason: a `SHARED` object in this node's store is normally **mail addressed to this
node**, which it can open. Letting the cache serve those to peers would turn every recipient
into a redistributor of its own correspondence.

But that reason is about *openable* mail. A carried envelope is ciphertext this node cannot
open, addressed to a NodeID that is not its own, which it agreed to hold for a bounded time.
It is exactly what a peer-serving cache is for.

So the rule as written is a proxy for the rule that is meant: **a `SHARED` object this node can
open is not cacheable for peers.** Custody is what tells the two apart, and custody is already
the discriminator everywhere else in carriage — `may_go_to`'s carriage exception and
`serve_manifest`'s choice of which sealed key travels both turn on it (ADR-0028 §11).

## Decision

**A carried envelope's ciphertext goes in the cache. A recipient's own mail goes in the
permanent store. Custody decides which, and nothing else does.**

1. `Cache` gains a dedicated entry point for carriage, rather than `may_be_cached_for_peers`
   being widened. Widening the label predicate would make every `SHARED` object cacheable
   everywhere it is consulted; a separate entry point makes carriage the only caller that can
   put one there, and makes that visible at the call site.
2. `keep_sealed` splits along the same line it already has two callers on: `BrokeredInbox`
   (this node's own mail) keeps `store.accept_shared`; `BrokeredCarrier` (a stranger's mail)
   uses the new path.
3. `may_go_to` serves a cached `SHARED` object **only** when this node holds custody of it.
   Not when it is this node's own received mail, which is the case the current rule exists to
   refuse and which stays refused.
4. `envelope.release` removes the cached object as well as the custody record. Release,
   expiry and dropping for room then all free bytes, which is what §7 assumes they do.
5. **Carriage neither takes over nor deletes bytes this node holds for another reason.**
   `put_object` overwrites, so storing a carried envelope over an existing entry would
   relabel it `SHARED`; a release — which is a message from a peer — would then delete a
   replica this node promised to hold, or an object an operator pinned. Content addressing
   means the collision needs the exact ciphertext, so it takes a peer that already had the
   bytes, which a former carrier is. "Unlikely" is not an access rule. `take_carried`
   declines over a pin or a live replica, and `release_carried` frees only an entry that is
   neither.

## What this does not decide, and the hazards it creates

- **Eviction can now lose an envelope under custody.** A carrier whose cache evicted the
  bytes still holds the record, offers the envelope, and cannot serve it. That failure already
  exists — a carrier can record custody for bytes it failed to keep — and the answer is the
  same either way: a carrier that cannot serve what it offers should drop custody rather than
  keep advertising it. That reconciliation is not in this decision and is not built.
- **Two budgets now apply.** `envelope_carry_bytes` is checked by `CarryPolicy::decide` before
  custody; the cache budget applies to the bytes. The cache's is the outer limit and can
  refuse first. A `keep` that fails must therefore stop the pass **before** custody is taken —
  which is the order the code already runs in, and the order that must not be reversed.
- **A declined take after a successful keep leaves a held cache entry.** Both steps ask the
  same `CarryPolicy` with the same inputs, so a keep that succeeded and a take that declined
  needs the budget to have moved between them. The entry is held until the deadline the keep
  committed to and evictable after, so it is a waste rather than a leak; the pass does not yet
  undo it.
- **The two steps' deadlines differ by the milliseconds between them.** The keep asks the
  policy with an earlier `now`, so the custody record's deadline is the later of the two, and
  a record that outlived its hold could see an envelope evicted while this node still says it
  holds it. `envelope.take` closes that by extending the hold to the deadline it committed to.
  A carrier with no cache entry for the id — every node still on the old path — is a no-op.
- **The permanent store still has no delete.** That is a larger gap — a local-first OS in which
  the user cannot delete an object is wrong on its own terms — and it is not this decision's
  to fix. This one only stops carriage from feeding it.

## Consequences

`docs/network/CARRIAGE.md` §7 loses the entry that records the leak and §4 gains the cached
case. `schemas/` is unaffected: nothing here crosses a machine boundary — a peer cannot tell
which side of this line an envelope it is offered came from, and must not be able to.

## References

ADR-0026 §8 (the split this implements), ADR-0028 §7 and §11 (the amplification bounds and the
custody exception), ADR-0019 (sealing), CLAUDE.md §8 (labels are enforced in storage, not
advisory).

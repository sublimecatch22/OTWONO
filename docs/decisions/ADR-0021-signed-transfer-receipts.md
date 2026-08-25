# ADR-0021 — Signed transfer receipts: the receiver counts, and says so under its own key

**Status:** accepted · **Date:** 2026-08-25 · **STATUS: SPECIFIED** — no code exists yet.

## Context

A future system outside this OS is intended to reward nodes for contributing — bytes served,
storage held, uptime. That system is deliberately not being built here (`OTWONO-ARCHITECTURE.md`
§Non-goals: *"Not a blockchain, not a token, not a consensus system"*), and this ADR does not
build any part of it.

But it forces one decision now, because one piece is expensive to add later and cheap to add
today: **what evidence does a node have that it did the work it claims?**

Left alone, the answer is "none." A node counting its own bytes served produces a number it
asserts about itself. Every such number is free to fabricate, so a reward system consuming
them is a reward system paying whoever lies most. That is not a flaw in the counters; it is
what a counter is.

Everything else in the metering design — which resources to count, at what granularity, how
to export — is additive and can be built whenever. Receipts are not: they are a change to the
ONM wire protocol, and the protocol accumulates. Retrofitting one into a deployed protocol
means a version negotiation, a compatibility window, and two code paths. Adding one now means
a variant.

So this is decided now and built when metering is built.

## Decision

### 1. The receiver signs, and the receipt is cumulative

A **receipt** is a statement by the node that *received* bytes, signed by its Ed25519 node
key, that it has received a running total from a named counterparty during a named session.

Cumulative rather than per-transfer, and the reasons compound:

- **A signature per chunk is unaffordable.** ADR-0016 chunks average 64 KiB; a signature and
  a control-plane round trip per chunk would cost more than the transfer.
- **Later receipts supersede earlier ones.** The sender keeps only the highest total, so a
  lost or dropped intermediate receipt costs nothing and there is no gap to reconcile.
- **It is a construction with a track record.** This is how payment channels do it, and
  CLAUDE.md §2.3 says to take a mature idea rather than invent one at the point of use.

A receipt is issued on request and at session end. A sender that wants a mid-session receipt
asks for one; a sender that does not gets one when the session closes.

### 2. It is bound to a session by the Noise handshake hash

The single most important field. Without it a receipt is replayable: a sender could present
last week's receipt again, or the same receipt to a hundred different accountants.

The binding is the **Noise handshake hash** — already computed in
`SecureChannel::handshake`, already the thing `id.sign_session` signs over
(`SESSION_DOMAIN`, `HANDSHAKE_HASH_LEN`). It is unique per session, known to both parties and
to nobody else, and needs no clock.

A receipt therefore carries: the issuer's NodeID, the **counterparty's** NodeID, the session
hash, the cumulative byte count, and the signature. The counterparty is named inside the
signed message, so a receipt issued to A cannot be presented by B as evidence of B's work.

The handshake hash is presently a local inside `handshake()`. Exposing it on `SecureChannel`,
as `local()` already exposes the node's own identity, is the small change this needs.

### 3. Its own signing domain, and its own narrow method

The message signed is `otwono-transfer-receipt-v1: || ...`, joining the domains this system
already keeps distinct — `otwono-agreement-binding-v1:`, `otwono-sharing-binding-v1:`,
`otwono-session-v1:`, `otwono-object-v1:`, `otwono-application-v1:`. A signature made for one
purpose must never verify as another.

`otwono-netd` does not hold the node key and must not start (ADR-0010). It asks `otwono-idd`,
through a **new narrow method** `id.sign_receipt` — shaped like `id.sign_session`, not like
`id.sign`. It signs a receipt's fields under a fixed domain and nothing else. Handing the Z3
hostile-input daemon a general signing oracle to solve a metering problem would be an
absurd trade, and `id.sign`'s `APPLICATION_DOMAIN` exists precisely because that trade was
considered and refused once already.

### 4. Receipts are `PRIVATE`, and both ends already know what is in them

A receipt names two NodeIDs and a weight. That is an edge in a social graph, which is exactly
what ADR-0020 went to some trouble not to leak.

It leaks nothing **to the two parties**, who are the two ends of the edge and already know it
exists. The exposure is entirely in publication. So receipts are `PRIVATE` under CLAUDE.md §8
like everything else, held locally by both parties, and exporting them is an explicit,
confirmed, audited action.

Stated plainly for whoever designs the ledger: **a reward system that requires publishing
receipts is a reward system that publishes its users' social graph, weighted by volume.** If
that is the design, it is that ADR's decision to argue and defend, and it does not get to
inherit the assumption from here.

### 5. Issuing is optional, and refusing does not break the transfer

A node may decline to issue receipts. The transfer proceeds anyway.

This is not politeness. Making metering a precondition for content moving would mean a node
that cannot reach its identity daemon also cannot fetch, and a mesh that works less well
than the one without rewards in it. Prime directive 2 — *works offline* — is worth more than
any accounting.

A sender records "this peer issued no receipt," which is information rather than a failure.
Whether to prefer peers who receipt is a policy question for a later reciprocity design, and
is deliberately not answered here.

### 6. Where receipts are stored is deferred, with one constraint

The metering ADR decides that. One constraint binds it now: **not `otwono-netd`.** A receipt
is a durable claim of value and `otwono-netd` is the hostile-input boundary with no
filesystem write outside its spool. It may hold them for the life of a session; something in
Z2 or Z1 keeps them.

## What a receipt does not prove

Worth stating at length, because the temptation to overclaim here is enormous and a later
system will inherit whatever this document implies.

- **Two colluding nodes can sign receipts for transfers that never happened, at zero cost.**
  No receipt design fixes this. A receipt raises the cost of a lie from *nothing* to *needing
  a partner*, which is a real improvement over self-reporting and is not proof.
- **Identity is free here**, so "a partner" can be a second keypair on the same machine. Any
  reward system built on these must solve Sybil resistance by some other means; receipts do
  not help and must not be mistaken for helping.
- **It does not prove the bytes were wanted.** A receipt for content nobody needed counts the
  same as one for content somebody did.
- **It does not prove the sender held the data first.** The receiver verifies chunks against
  content ids, so real content did move — but two colluding nodes can manufacture content and
  ship it.
- **It carries no time.** Deliberately: SBCs boot without a real-time clock, and a timestamp
  is metadata for no gain here. A node that needs to know *when* records when it received the
  receipt, which is its own observation and not a claim by anyone else.

The honest summary: a receipt turns "I say I did the work" into "we both say I did the work."
That is worth having and it is one step, not a solution.

## Consequences

**Good.** The evidence exists from the day metering ships instead of being unavailable
forever. The wire change lands as a variant rather than a migration. The signing stays narrow
and the Z3 boundary holds. Nothing here commits the project to a token, a chain, or a
consensus system, and a node with no interest in any of that is unaffected.

**Bad, and worth naming.**

- **A new signing method on the identity daemon** is new attack surface on the most sensitive
  daemon in the system, added for a feature that daemon does not otherwise need.
- **Receipts are durable, sensitive, per-peer records** that a node did not previously keep.
  The safest data is data not collected, and this is a decision to collect.
- **Cumulative-and-superseding means the last receipt is the only one that matters**, so a
  session that ends badly may leave the sender holding a total lower than what it actually
  transferred. Accounting is best-effort by construction.
- **It invites overclaiming.** The failure mode is not technical: it is a later document
  citing "signed receipts" as though they were proof. The section above exists to make that
  citation embarrassing.

## Alternatives rejected

- **Self-reported counters alone.** What happens by default. Free to fabricate, so any reward
  system on top pays whoever lies most.
- **A signature per chunk or per object.** Correct-looking and unaffordable: the signing cost
  and the control-plane round trip would exceed the transfer for anything ADR-0016 chunks.
- **Sign with the X25519 agreement key that `otwono-netd` already holds**, avoiding the trip
  to `otwono-idd`. X25519 is for key agreement, not signatures; and the point of ADR-0010 is
  that the network daemon holds only what Noise needs.
- **Bind to a timestamp instead of the handshake hash.** Requires a trustworthy clock the
  hardware does not have, and is replayable across sessions that happen to share a second.
- **Bind to the content id instead of the session.** Makes each receipt name what was
  transferred, which is a per-object record of who fetched what from whom — the "holding is
  publishing" cost of ADR-0015, written down and signed.
- **Have the sender sign a claim and the receiver counter-sign it.** Two signatures for
  strictly less: it is the receiver's count that matters, and the sender's signature adds
  nothing an adversary could not already produce.
- **Make receipts mandatory.** Would let a metering feature break content transfer, which is
  backwards.

## References

- ADR-0010 (the two-key split this must not undo), ADR-0017 (the request/response protocol
  this adds a variant to), ADR-0015 (holding is publishing), ADR-0020 (the per-peer privacy
  discipline receipts sit in tension with).
- `docs/security/SECURITY-MODEL.md` §1 (trust zones), CLAUDE.md §8 (visibility labels,
  telemetry), `OTWONO-ARCHITECTURE.md` §Non-goals.
- `crates/otwono-identity/src/signer.rs` — `SESSION_DOMAIN`, `HANDSHAKE_HASH_LEN`, and
  `id.sign_session`, which `id.sign_receipt` is modelled on.

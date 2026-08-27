# ADR-0027 — Signed mutable pointers: sequence numbers, not timestamps, and rollback is the threat

**Status:** accepted
**Date:** 2026-08-27
**Phase:** 6 (first distributed services)

## Context

`DISTRIBUTED-SERVICES.md` §1 says every service in OTWONO is composed from three primitives:

1. content-addressed blocks — **built** (`otwono-store`, ADR-0016)
2. **signed mutable pointers** — this ADR
3. addressed messages — not yet

Primitive 1 gives immutability. Everything a person actually does is mutable: a wiki page is
edited, a profile changes, a forum thread grows. A content id names bytes forever, which is
exactly why it cannot name "my current profile".

So the second primitive is the one that makes the first useful over time, and it blocks all
three services Phase 6 names — the profile site, the wiki, and the forum are each *a
collection of blocks plus a pointer at the current one*. Nothing in Phase 6 can start
without it.

## Decision

A **pointer** is a signed, monotonically sequenced record binding a name to a content id,
owned by exactly one NodeID.

```
(node_id, service, name)  →  content_id, at sequence N
```

It is signed with `id.sign` (ADR-0010's brokered signing, `otwono-application-v1:` domain)
over a canonical encoding with an inner `otwono-pointer-v1:` prefix.

### 1. The threat is rollback, and it is not solved by signatures

This is the decision everything else follows from.

A signature proves the owner wrote a record. It does **not** prove the record is current. An
old pointer is a genuine, correctly signed statement by the rightful owner — it is simply
out of date. So an attacker who can serve responses (a malicious peer, a cache, a gateway,
anyone on the path) can roll a reader back to any historical version by replaying an old
record, and every signature check passes.

This is the whole security problem of mutable data on an untrusted network, and it is worth
being blunt that **cryptography alone does not solve it**. What solves it is state the
reader keeps:

- Every pointer carries a `sequence`, chosen by the owner, strictly increasing.
- A reader **remembers the highest sequence it has seen** for each pointer.
- A record at a lower sequence is **rejected**, not preferred-against. Rejected, because
  "prefer the higher one" makes it a ranking problem, and a reader with no memory has
  nothing to rank against.

A first-time reader has no memory and therefore no protection — it accepts whatever it is
given, which may be old. That is an unavoidable property of the design and is documented
rather than papered over: trust-on-first-use, with protection accruing from the second read.

### 2. Sequence numbers, not timestamps

The obvious alternative is "newest wins by timestamp". Rejected:

- **Clocks disagree**, and a distributed system that orders by wall clock orders by whose
  clock is furthest ahead. On a mesh with no NTP guarantee — Prime Directive 2 says this
  works offline — that is not a rare edge case.
- **A timestamp is forgeable in the useful direction.** An owner who wants their record to
  win writes a future date. There is nothing to check it against.
- **Sequence is total and owner-controlled.** The owner is the only writer, so it always
  knows its own last number. No coordination, no consensus, no clock.

A `published_at_ms` field is carried **and signed**, because a person reading a wiki page
wants to know when it changed. It is never used for ordering. That separation is enforced by
having the comparison live in one function and by a test that a record with a future
timestamp and a lower sequence still loses.

### 3. One writer per pointer, by construction

A pointer is owned by a NodeID, and a NodeID is one node's key. So there is exactly one
writer, and **there is no concurrent-write conflict to resolve** — no vector clocks, no CRDT,
no last-writer-wins heuristic.

That is a deliberate limitation with a real cost: a person with a laptop and a phone has two
NodeIDs and therefore two pointer namespaces. Their "profile" on each is a different pointer.
Reconciling those is a **user identity** question, not a pointer question, and ADR-0019's
sharing keys are where that thread will be picked up. Pretending a pointer could span
devices would mean inventing multi-writer coordination to solve a problem that is really
about what a "user" is.

The wiki's "per-page last-writer-wins with explicit merge on conflict"
(`DISTRIBUTED-SERVICES.md` §2) is therefore about *different people's* pages, not concurrent
edits to one — each author's revisions live under their own pointer.

### 4. Deletion is a pointer at nothing, not a missing pointer

`content_id` is optional. A record with none is a **tombstone**: the owner is saying "this
no longer exists", signed and sequenced like any other update.

The alternative — deleting the record — cannot work on a network where anyone may hold a
copy. A reader that saw version 5 and now sees no record cannot distinguish "deleted",
"never existed", "the peer is withholding it", and "the network is broken". A tombstone at
sequence 6 says exactly one of those, and the sequence rule stops it being rolled back to
version 5.

It does not, and cannot, retract copies others already hold. `DATA-VISIBILITY.md` already
requires the system to say so rather than imply recall.

### 5. What is signed, and how it is encoded

The record minus its `signature` field, canonically encoded, with the prefix
`otwono-pointer-v1:`.

Canonical means: JSON, object keys sorted, no insignificant whitespace, absent optional
fields omitted rather than null. The canonicalizer is **written out**, not delegated to
`serde_json`'s map ordering — ADR-0011 made that choice for model manifests after noting the
ordering is a consequence of a feature flag any transitive dependency could flip. The
meaning of a signature must not depend on a Cargo feature.

The inner prefix matters even though `id.sign` already domain-separates with
`otwono-application-v1:`. That outer domain stops a pointer signature being replayed as a
*session* signature; the inner one stops it being replayed as a different *application*
record. Both are needed, and neither substitutes for the other.

### 6. `service` and `name` are part of the signed record

Not derived from where the record was found. A pointer that only signed `content_id` and
`sequence` could be lifted from `wiki/Home` and served as `profile/index` — the signature
would verify, the sequence would look fine, and the reader would render the wrong thing
under the right name.

So the tuple the reader asked for must equal the tuple inside the signature, checked on
every read.

## Consequences

**Good.** All three Phase 6 services become composition rather than new mechanism. No
consensus, no coordination, no clock synchronisation. A pointer is small enough to cross a
`Trickle` link, which `DISTRIBUTED-SERVICES.md` §4.2 requires. Verification needs only the
owner's public key, which the NodeID already is.

**Bad, and worth naming.**

- **First read is unprotected.** Trust-on-first-use. A reader with no stored sequence takes
  what it is given, and a hostile first response wins until a higher sequence arrives.
- **A reader that loses its state loses its protection**, and reverts to first-use trust.
- **An owner can roll itself back** by signing a lower sequence — nothing stops the key
  holder rewriting history for readers who have no memory of it. The rule protects readers
  from third parties, not from the owner.
- **Sequence exhaustion** is theoretical (`u64`) but the type is fixed and cannot grow later
  without a schema break.
- **Nothing here distributes the pointer.** This is the record and its rules; fetching one
  from a peer is the next slice.

## Alternatives rejected

- **Timestamps for ordering.** §2. Orders by whose clock is furthest ahead.
- **A blockchain, or any consensus over pointer updates.** One writer means there is nothing
  to reach consensus about. It would buy global rollback resistance at the cost of
  needing the network to work, which Prime Directive 2 forbids.
- **Deleting the record to delete the thing.** §4. Indistinguishable from four other states.
- **Signing only the content id.** §6. Lets a valid record be served under the wrong name.
- **Multi-writer pointers with CRDT merge.** Solves a problem this design does not have
  (one key, one writer) and would need an answer to "which device is authoritative" that is
  really a question about user identity.

## What is deliberately not decided

- **How a pointer reaches another node.** A new ONM request in ADR-0017's shape, most
  likely, and the next slice.
- **Whether pointers are cached or replicated** the way blocks are (ADR-0015, ADR-0026). A
  cached pointer is a rollback risk with a friendly face, and it needs its own thinking.
- **Petnames.** `DISTRIBUTED-SERVICES.md` §3 says local assignment with no global registry;
  none of that is built and none of it affects this record.
- **Key rotation.** ADR-0009's succession exists; what a pointer signed by a superseded key
  means to a reader is a question for whoever builds the fetch path.

## References

`docs/services/DISTRIBUTED-SERVICES.md` §1 (the three primitives) and §3 (`onm://`
addressing), ADR-0010 (brokered signing, the domain this reuses), ADR-0011 (why the
canonicalizer is written out), ADR-0016 (content addressing, the thing a pointer points at),
ADR-0019 (sharing keys, where the two-devices question continues),
`docs/security/DATA-VISIBILITY.md` (why a tombstone cannot recall copies).

# ADR-0020 — A recipient discovers what was shared with it, by asking, and learns nothing else

**Status:** accepted · **Date:** 2026-08-25 · **STATUS: VERIFIED between two booted nodes**

`out/amd64-qemu-ubuntu/two-node/node-{a,b}.log`: each node asked its peer what that peer had
sealed to it, was told, and fetched it, with no id passing between the VMs by any other
route. The ids cross — A discovered exactly what B sealed to A, and the reverse — which is
what distinguishes a working index from one answering with what the node holds.

Not yet exercised on a machine: paging (both nodes had one object each), the per-session
snapshot's staleness on a long-lived link, and the scan's cost on a large store.

Settles **OQ-29**.

## Context

ADR-0019 made `SHARED` work and left it unusable.

A `PUBLIC` object's `ContentId` is the hash of its content, so two nodes holding the same
bytes derive the same name with no coordination. That is the property the whole content
layer rests on, and it is why `build/files/otwono-mesh-content-check` can have two booted
nodes fetch from each other with no channel between them: both compute the id from a fixed
string.

A `SHARED` object has none of that. It is encrypted before it is chunked with a fresh
per-object key (ADR-0019 §1), so its id is over ciphertext nobody can predict. A recipient
cannot derive it, cannot guess it, and has no way to ask for it. Sharing therefore works
today only alongside some other channel that carries the id — a phone call, an email, a
messaging app — which is most of the problem node identity was built to solve.

This is visible in the verification log: two booted nodes each seal an object to the other,
and neither can fetch what the other sealed, because neither knows what to ask for. Every
other piece is built and tested. The missing one is a question a recipient can ask.

## Decision

### 1. A peer asks what has been sealed to **it**, and gets only that

One new ONM request. The responder answers with the content ids of objects in its own store
whose sharing envelope names the asking peer, and nothing else — not `PUBLIC` content, not
what it holds for anyone else, not how large its store is.

The asking peer is the NodeID the Noise handshake authenticated. It is never a field in the
request, for the same reason `store.serve_chunk` names its object: a peer that could ask on
somebody else's behalf would be asking a different and much worse question.

### 2. This does not widen "holding is publishing"

ADR-0015 names the cost that a node holding content reveals it to anyone who can guess the
bytes, and a directory usually makes that worse. This one does not, and the reason is worth
being precise about rather than asserting.

Every id in the reply is one the asker **could already fetch**: it is on the object's
recipient list, so `store.serve_manifest` would hand it the manifest, and that manifest
carries a content key sealed to it (ADR-0019 §4). The reply therefore contains no id the
asker is not already entitled to have, and no id it could not already open. What changes is
that it no longer has to be told the id out of band.

An **unauthorized** peer gets an empty list — byte-identical to the list an authorized peer
gets when nothing has been shared with it. So the reply cannot be used to probe: "nothing
for you" and "nothing for anybody" are the same answer, which is the same discipline
`not_available` already applies to a single object.

What it does reveal, to a peer already on the list, is *how many* objects are sealed to it.
That is information the sender chose to give them by sharing, and they would learn it by
fetching each id anyway.

### 3. No timestamps, and ordering by content id

The obvious index entry carries "shared at". It is not here.

Sharing time is metadata this node does not currently record, and adding it would mean the
object record grows a field whose only consumer is this reply. It would also tell a
recipient the order in which a household did things, which is a small but real behavioural
leak for no gain the recipient has asked for.

Entries are ordered by content id instead: deterministic, needs no new state, and stable
across calls, which is what pagination requires. A recipient that wants its own ordering can
keep one; it knows when *it* learned of each object, which is the timestamp that is actually
theirs.

### 4. Computed once per session, paged from there

The answer is produced by scanning the object records — one JSON parse per object — and
filtering. That is O(objects), and a peer that could force a fresh scan per request would
have a cheap way to make an SD-card-backed node miserable.

So the index is computed **once per session**, when a peer first asks, and later pages are
served from that snapshot. A peer that wants a fresher answer opens a new session, and a new
session costs a Noise handshake — which is exactly the right price and needs no rate limiter,
no timer and no new configuration.

`otwono-netd` already keeps per-session state for ADR-0019 §4 (which shared objects it has
released a manifest for), so this lands beside something that exists rather than inventing a
place to put it.

The consequence to state plainly: an object shared *during* a session is not visible to that
session. The recipient sees it after reconnecting. For a mesh where sessions are short that
is invisible; for a long-lived one it is a delay, and it is the cost of not letting a peer
choose how much work this node does.

### 5. A maintained index is deferred, with the condition for revisiting

The alternative is a per-recipient index kept up to date by `store.share`, `accept_shared`
and `demote`. It turns an O(objects) scan into an O(1) lookup, and it is the right answer
eventually.

It is not the right answer first, because it is state that can disagree with the object
records — and every way it can disagree is a security bug: an index naming an object whose
envelope no longer does would advertise something the serve path then refuses, and an index
missing one would hide something the user believes they shared. The scan cannot disagree
with the records, because it *is* the records.

Revisit when a real node's scan is slow enough to matter. Until there is a number, an index
would be a consistency risk taken on a guess.

## Consequences

**Good.** `SHARED` becomes usable without a second channel. A recipient can be told nothing
at all and still find what is theirs. The reply is scoped so tightly that the privacy
analysis is short, which is itself the point — a directory that needed a long one would be
the wrong design.

**Bad, and worth naming.**

- **A peer learns how many objects are sealed to it, in one round trip.** Deliberate, and
  cheaper for it than fetching them, but it is a number this node volunteers.
- **The per-session snapshot means a delay.** Something shared now is discoverable on the
  next connection, not this one.
- **The scan is O(objects).** Bounded to once per session, but on a node with a very large
  store the first ask in each session is real work, and a peer that reconnects repeatedly
  can still cause repeated scans — cheaper than per-request, not free. A handshake per scan
  is the floor this design offers.
- **It says nothing about `PUBLIC` content.** A peer still cannot discover what a node
  publishes; that is a catalogue, it is a different problem with a different privacy
  analysis, and this ADR deliberately does not start it.
- **It does not solve revocation.** ADR-0019 §5 is now built, and `store.remove_recipients`
  edits the object record the index is computed from — so a removed recipient stops seeing
  the entry with no separate index to keep in step, which is §5 of this ADR paying off. What
  it cannot undo is a fetch that already happened: a recipient keeps what it has.

## Alternatives rejected

- **Wait for the Phase 6 messaging service and pass ids as messages.** Defensible, and it is
  how people will often actually share. Rejected as the *only* mechanism because it makes a
  storage primitive depend on an application that does not exist, and because a node that
  has been shared with should not need a second subsystem running to find out.
- **A global index of everything a node holds, filtered by the asker.** Same reply, far worse
  failure mode: one bug in the filter publishes the store. Scoping the *computation* to the
  asker rather than the *presentation* means there is no filter to get wrong.
- **Push a notification on the next handshake.** No request, no scan — the sender tells the
  recipient during `Hello`. Rejected because it makes `Hello` carry unbounded per-peer state,
  and because a recipient that was offline when the share happened would need the sender to
  remember to tell it, which is a delivery guarantee the mesh does not offer.
- **Let the asker name the peer it is asking about.** Would allow a node to answer "what has
  X shared with Y" for a UI. It is also an enumeration oracle for the entire recipient graph,
  and there is no version of it that is safe.
- **Return the sealed keys in the index too.** Saves a round trip. Rejected: the manifest
  already carries the asker's key, so this would duplicate the one piece of the envelope that
  matters, in a reply that is meant to be small enough to page over a narrow link.

## References

- ADR-0019 (`SHARED` objects, the sharing key, and §4's per-recipient serving), ADR-0017 (the
  request/response protocol this adds to), ADR-0015 (holding is publishing).
- **OQ-29**, which this settles.
- `docs/build/VERIFICATION-LOG.md` — the two-node run where each node sealed to the other and
  neither could fetch.

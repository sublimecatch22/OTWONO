# Wiki

**Status:** `IMPLEMENTED`. The record and its rules are in `crates/otwono-wiki` with unit
tests; the schema is `schemas/wiki-revision.schema.json`; the decisions are ADR-0032.
**Nothing is wired to a daemon and no page has crossed a link**, so this is not yet a service
you can use — see §6.

The first service composed from the three primitives rather than a fourth one.

---

## 1. What a page is

```
onm://<nodeid>/wiki/Getting-Started
```

Three layers, each one an existing primitive:

| Layer | What | Primitive |
|---|---|---|
| The name | a signed mutable pointer, service `wiki`, name `Getting-Started` | ADR-0027 |
| The head | the content id the pointer names: a `Revision` | ADR-0016 |
| The text | the content id the revision names: the page body | ADR-0016 |

A revision names its parent, so a page is a **chain** and the pointer names its head. Reading
history is walking that chain; the pointer moves forward as the page is edited, and the
sequence number is what stops a reader being rolled back to an older head (ADR-0027 §3).

## 2. One writer, and what that settles

A pointer has exactly one writer, and `onm://<nodeid>/wiki/<path>` puts the page in one
node's namespace. So **a page has exactly one author**, and two nodes editing "the same page"
is not a thing that can happen — node B's copy of node A's page is a different page under a
different NodeID.

That is why there is no merge rule here. `DISTRIBUTED-SERVICES.md` §2 describes the wiki as
"per-page last-writer-wins with explicit merge on conflict", which describes a *shared*
document with several contributors. That needs either a designated owner merging proposals
sent as envelopes (ADR-0028) or a CRDT, and ADR-0032 §7 chooses neither. What is built is the
single-author case, which is what the addressing already implied.

## 3. Every revision is signed, not just the head

The pointer's signature vouches for the head and nothing else. History is walked by fetching
parents from a peer, and that peer could otherwise substitute any ancestor: a reader would
verify the head, follow an unsigned link, and display a fabricated earlier version as the
author's words.

Content addressing stops the bytes being *altered* — a substituted parent has a different id —
but an id is only as good as whatever vouches for it, and nothing does once you step off the
head. So each revision carries its own Ed25519 signature over a domain-separated canonical
encoding, and `walk` checks every step.

Three things are checked at each step, and each has a test that fails if it is removed:

- **The signature**, against the key the claimed author's NodeID is a hash of. Checking the
  signature alone would accept anything from anyone, since an attacker signs with their own
  key and supplies it.
- **The page name**, which lives *inside* the signed record. Both revisions in a splice can
  be genuinely signed by the same author; the second is simply not part of the page being
  read, and without the field a peer could pad any history with the author's writing from
  anywhere else.
- **That the chain does not revisit an id.** `parent` is not trusted to be older — nothing in
  the record can prove that — so a loop is a malformed page rather than a hang.

## 4. What a reader gets back

A walk returns the steps it took **and how it ended**, because a bare list cannot tell "this
is the whole history" from "this is as much of it as I have":

- `Complete` — reached a revision with no parent.
- `Truncated { missing }` — a parent this reader does not have. The ordinary case just after
  fetching somebody's page: you have the head and none of the history, and that is not a
  fault.
- `Limited` — the caller's bound. How long a history is is the serving peer's choice, so a
  reader without a bound would follow it for as long as one kept answering.

## 5. The universal requirements

| Requirement (`DISTRIBUTED-SERVICES.md` §4) | How |
|---|---|
| Work with zero peers | A page is local objects and a local pointer. Reading and editing your own wiki needs no network at all. |
| `Trickle`-safe representation | The body text *is* the representation; there is nothing to degrade. History is optional and fetched on demand. |
| Respect visibility labels | The crate never touches storage. Bodies and revisions are ordinary objects written through `otwono-stored`, so a `PRIVATE` page is private by the same rule as everything else. |
| Tier-aware | Nothing tier-dependent is introduced. A T0 node hosts a text wiki; the bound on a history walk is the caller's. |
| Independently testable with a fake network | `Revisions` is a trait. The tests walk chains held in a map, with no daemon and no I/O. |

## 6. What is not built

- **Nothing is wired to a daemon.** There is no `otwono-wikictl`, no `wiki.*` control-plane
  method, and nothing that turns a directory of text into a chain. Publishing today would
  mean calling `store.put` and `pointer.publish` by hand.
- **No page has crossed a link.** Two booted nodes have published and resolved
  `wiki/Getting-Started` as a *pointer to an opaque blob* — that is the primitive, not this
  service. Phase 6's first exit clause needs a revision and its body to make the trip.
- **No rendering, no links between pages, no `onm://` in a browser.** That is §3's local
  resolver.
- **No multi-writer merge**, per §2 above.
- **A fork inside one author's own chain** — two revisions sharing a parent, from a restore or
  two racing processes — is not detected. A reader would have to walk both chains, and it has
  no reason to know there are two.

## References

ADR-0032 (the decisions), ADR-0027 (pointers, and the ordering rule this copies), ADR-0016
(content-addressed blocks), `docs/services/DISTRIBUTED-SERVICES.md` §2–§4.

# ADR-0032 — A wiki page is a signed chain of revisions, owned by one node

**Status:** accepted
**Date:** 2026-08-28
**Phase:** 6 (first distributed services)

## Context

All three primitives are verified between booted nodes and no service is built on them.
`DISTRIBUTED-SERVICES.md` §2 describes the wiki as *append-only signed page revisions;
per-page last-writer-wins with explicit merge on conflict*, and §3 addresses it as

```
onm://<nodeid>/wiki/<path>
```

Phase 6's remaining exit clause is "node A's wiki page is readable on node B".

The primitives already carry most of it. A page's current state is a **signed mutable
pointer** (ADR-0027) under the service namespace `wiki`; what it names is a
**content-addressed block** (ADR-0016). Two booted nodes have already published
`wiki/Getting-Started` and resolved each other's — but what they pointed at was an opaque
blob. A blob is not a wiki: there is no history, no author, and nothing that says which page
it belongs to.

## What the addressing already decides

`onm://<nodeid>/wiki/<path>` puts the page in one node's namespace, and a pointer has exactly
one writer by construction (ADR-0027 §2). So **a page has exactly one author**, and the
multi-writer merge the catalogue mentions cannot arise between two nodes editing "the same"
page — there is no such thing. Node B's copy of node A's page is a different page under a
different NodeID.

This is worth stating rather than leaving implicit, because it is the difference between this
being a fortnight of CRDT design and being a week of composition. §7 says what is still open.

## Decision

**A page is a chain of signed revisions. The pointer names the head; each revision names its
parent.**

```
Revision {
  schema_version, page, body, parent, author, written_at_ms, signature
}
```

- `page` — the name the revision belongs to. A revision is self-describing, so a reader that
  fetched one cannot be made to file it under a different page.
- `body` — the content id of the text. Separate from the revision so that identical text
  costs one copy, and so a body can be fetched, cached and replicated by the machinery that
  already exists rather than by anything new.
- `parent` — the content id of the previous revision, or absent for the first. This is the
  history.
- `author` — the NodeID. Equal to the pointer's owner for a page in its own namespace, and
  *not* equal once a page is copied from elsewhere, which is how provenance survives a copy.
- `written_at_ms` — shown to people, **never** used for ordering. Same rule as the pointer's
  `published_at_ms` (ADR-0027 §5): a mesh with no NTP cannot order by wall clock.

### Every revision is signed, not just the head

The pointer's signature vouches for the head and nothing else. History is walked by fetching
parents, and a peer that serves the chain could otherwise substitute any ancestor: the reader
would verify the head, follow an unsigned link, and display a fabricated earlier version as
the author's words. Content addressing stops the bytes being *altered* — a substituted parent
has a different id — but the id is only as good as what vouches for it, and nothing does once
you step off the head.

So each revision is signed by its author over the same domain-separated canonical encoding the
pointer uses. Verifying a page's history is then a walk, and each step is checkable on its own.

### The chain is verified in one direction

A reader walks head → parent → parent, verifying each signature and that each revision names
the page it was asked for. It does **not** trust `parent` to be older, because "older" is not
a thing the record can prove — it checks that the chain terminates and does not revisit an id
it has already seen. A cycle is a malformed page, not a hang.

## Consequences

- The wiki needs no new wire method and no new daemon. Publishing is `store.put` plus
  `pointer.publish`; reading someone else's page is `content.pointer` plus the fetch path,
  all of which exist and are verified between booted nodes.
- A page is readable with zero peers, because it is local objects and a local pointer
  (§4.1 of `DISTRIBUTED-SERVICES.md`).
- Its `Trickle`-safe representation is the body text itself; there is nothing to degrade.
- Deleting a page is the pointer's tombstone (ADR-0027 §4). The revisions stay
  content-addressed and reachable to anyone who kept an id, which the UI must not pretend
  otherwise — the same honesty ADR-0027 requires of demotion.

## What this does not decide

- **Multi-writer merge.** Two nodes cannot edit one page, so the catalogue's "last-writer-wins
  with explicit merge" does not arise yet. It becomes real the moment a page is *shared* —
  several NodeIDs contributing to one document — and that needs either a designated owner
  merging proposals sent as envelopes (ADR-0028) or a CRDT. Neither is built and neither is
  chosen here.
- **A fork inside one author's own chain.** One writer with a monotonic pointer should not
  produce two revisions with the same parent, but a restore from backup or two processes
  racing could. The reader can detect it only by walking both chains, which it has no reason
  to do. Not handled.
- **Rendering.** Wiki markup, links between pages, and `onm://` resolution in a browser are
  §3's local resolver and are not part of this.

## References

ADR-0016 (content-addressed blocks), ADR-0027 (signed mutable pointers, and the ordering rule
this copies), ADR-0019 (sealing, for a page that is not `PUBLIC`),
`docs/services/DISTRIBUTED-SERVICES.md` §2–§4.

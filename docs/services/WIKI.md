# Wiki

**Status:** `IMPLEMENTED`. The record and its rules are in `crates/otwono-wiki`; the schema is
`schemas/wiki-revision.schema.json`; the decisions are ADR-0032. `otwono-wikictl` writes,
reads and walks the history of a page over the real control plane, and a page written through
the daemons reads back as its text with its signature verifying against the node that made it.

A page also **crosses a link**: a reading node resolves the peer's `wiki/<page>` pointer,
fetches the revision and then the body, and verifies the revision against the key the
*handshake* proved. Shown between two nodes over a real Noise channel;
**not yet between booted nodes**, which is what Phase 6's first exit clause needs — see §6.

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

- **No `wiki.*` control-plane method.** `otwono-wikictl` composes the calls that already
  exist — `store.put`, `pointer.next_sequence`, `id.sign`, `pointer.publish` — rather than a
  daemon growing a wiki API. Four calls, in one command and not three, because splitting them
  would put an unsigned record on a command line between them (ADR-0027's reason for
  `pointer-publish` being one command).

  The pointer moves **last**, and that order is not incidental: it is what anyone else reads,
  so a pointer advanced before its revision was stored would name an id this node cannot
  serve, and a reader would see a page that exists and cannot be opened.
- **No page has crossed a link between *booted* nodes.** It crosses one in
  `a_wiki_page_is_readable_from_another_node`, over a real Noise channel with real daemons.
  What the three-node QEMU harness has shown is two nodes resolving `wiki/Getting-Started` as
  a *pointer to an opaque blob* — the primitive, not this service. Phase 6's first exit clause
  needs a revision and its body to make that trip.
- **No rendering, no links between pages, no `onm://` in a browser.** That is §3's local
  resolver.
- **No multi-writer merge**, per §2 above.
- **A fork inside one author's own chain** — two revisions sharing a parent, from a restore or
  two racing processes — is not detected. A reader would have to walk both chains, and it has
  no reason to know there are two.

## References

ADR-0032 (the decisions), ADR-0027 (pointers, and the ordering rule this copies), ADR-0016
(content-addressed blocks), `docs/services/DISTRIBUTED-SERVICES.md` §2–§4.

## 7. Using it

```
otwono-wikictl write Getting-Started --file page.md
otwono-wikictl read  Getting-Started --out page.md
otwono-wikictl history Getting-Started
otwono-wikictl read Getting-Started --from <NODEID> --at <ADDR> --out theirs.md
```

Reading a peer's page checks three things before a byte is written, because a file on disk is
what a person then reads and believes: the **pointer**, by `otwono-netd`, against the
handshake key and the rollback rules; the **revision's own signature**, against that same key,
since the pointer vouches for which id is current and says nothing about what that id
contains; and that the revision **names the page that was asked for**.

The key comes from `net.pointer`'s reply rather than from the record. A NodeID is a hash of
the public key, so a record cannot carry its own answer to "was this really them" — the
handshake is the only place that key is proved rather than asserted.

`history` verifies every step and prints how the walk ended, so a truncated history says so
rather than looking like a short one. It answers for revisions authored by **this** node and
refuses others: a page copied from a peer keeps its original author (ADR-0032), and there is
nowhere yet to look a peer's key up from a terminal. Refusing is the right answer — "verify it
later" is "never".

# The Neighbourhood Cache

**Status:** `SPECIFIED`. No implementation. Nothing in this document has been built, run,
or measured. Decided in **ADR-0015**; belongs to Phase 5 (the content store) and Phase 6
(peer exchange).

---

## 1. What it is for

A node contributes a bounded slice of its disk. That slice holds content-addressed chunks,
encrypted at rest, reachable only by `otwono-stored`. When the node wants something, it asks
every peer it can reach for the chunks in parallel and verifies each one by its hash.

The property this buys: **a dense neighbourhood is a fast neighbourhood.** Ten houses on a
street each fetching the same 4 GB model over their own uplinks is ten downloads. Ten houses
with overlapping caches is one download and nine LAN copies. Nothing coordinates that — it
falls out of naming chunks by their hash, which makes every holder interchangeable.

It also means the network gets *better* as it grows, which is the opposite of how most
distributed systems behave, and it is the reason to build it this way.

## 2. What it is not

It is not a blockchain, and the difference matters enough to state twice. A blockchain
settles disagreements about history between parties who distrust each other. Here there is
nothing to disagree about: a chunk either hashes to its name or it is discarded. See
ADR-0015 for the full argument, including where something ledger-like *would* be needed.

## 3. Structure

```
/var/lib/otwono/cache/          0700, encrypted at rest, otwono-stored only
  chunks/<aa>/<bb>/<blake3>     one file per chunk, name is the hash
  objects/<aa>/<hex>.json       one record per cached object
  meta.json                     size accounting, last-access, pin flags, chunk refcounts
```

**STATUS: IMPLEMENTED** — `crates/otwono-store/src/cache.rs`. Two deviations from the sketch
above, both deliberate:

- `have.idx` is not a separate file. What this node holds is the key set of `meta.json`'s
  chunk refcounts, and a second index of the same fact is a second thing to keep in step.
- `meta.db` is `meta.json`. It is read once at open and rewritten on change; a cache index a
  person can read with `cat` is worth more here than one that is fast, and it avoids a
  database dependency in the base image.

The refcounts are not an optimization. Chunks are shared between objects by design, so
evicting one object must not delete chunks another still needs — without counting, eviction
silently corrupts whatever else referenced them.

The cache is a **second `Store`**, rooted at a different directory rather than a flag on the
first. `/var/lib/otwono/store` is the user's and nothing may evict it; everything in
`/var/lib/otwono/cache` is disposable by definition. Two directories makes that structural
instead of a boolean somebody has to remember to check, and eviction has no path to the
user's data.

| Property | Value |
|---|---|
| Addressing | BLAKE3 of the chunk's plaintext |
| Chunk size | Content-defined boundaries, parameters fixed by schema (**OQ-16**) |
| Encryption | At rest, node-held key; a stolen disk is not a readable cache |
| Eviction | Least-recently-used, except pinned objects |
| Size | Operator-set, defaulted from the capability tier |

### Size by tier

The default comes from the capability policy engine and nowhere else (CLAUDE.md §2.6). An
operator may raise or lower it; no subsystem may infer it.

**STATUS: IMPLEMENTED** — `FeatureGates::neighbourhood_cache_bytes`, in the capability
profile schema at `1.1.0`. A machine whose storage axis is `Constrained` gets zero whatever
its tier says: a full disk is a broken node, so a machine with no room contributes nothing
rather than contributing until it dies.

| Tier | Default contribution | Rationale |
|---|---|---|
| T0 | 512 MiB | An 8 GB eMMC has little to spare, and a full disk is a broken node |
| T1 | 4 GiB | Enough for a model and a media manifest set |
| T2 | 32 GiB | A working set for a household |
| T3 | 128 GiB | A neighbourhood's shoulder |

**A cache that fills the disk is a fault, not a feature.** The reserve floor that protects
the audit log and the spool applies here too.

## 4. Fetching

**STATUS: IMPLEMENTED** — `otwono_netd::content::fetch_object_from_peers`, with one caveat
worth stating up front: step 2's "ask peers which chunks they hold" is not a want-list
exchange. Every peer is simply asked for chunks off a shared queue and a peer that does not
have one loses that attempt. On a LAN that is cheaper than a round of negotiation; on a
constrained link it is not, and it wants revisiting with OQ-23.

Ordering within the object is not rarest-first either — it is whatever the queue hands out.
Rarest-first matters when peers are also downloading from each other, which is a swarm and
not what this is.

1. Resolve the object to a chunk list (from its signed manifest).
2. Ask reachable peers which of those chunks they hold — LAN peers first, then the wider
   overlay, then origin.
3. Request chunks in parallel across every holder, weighted by the link's bandwidth class.
4. **Verify each chunk against its hash on arrival.** A mismatch discards the chunk and
   demerits that peer for this transfer. It never reaches the caller.
5. Assemble, verify the whole object against the manifest, hand back a path.

A peer that lies wastes our bandwidth. It cannot corrupt our data, and it does not have to
be trusted to be useful — that is the entire security argument, and it is one hash long.

**The manifest is checked before any chunk is requested.** A `ContentId` is the BLAKE3 of
the chunk list itself, so a substituted manifest is detectable immediately — and once it is
known authentic, *any* peer may serve *any* chunk and be verified against it independently.
That is what makes step 3 safe, and the first implementation got it wrong: it only compared
at reassembly, so a lying peer could make a node download the whole object before being
caught.

### Ordering that matters

**Local before uplink, always.** The point of the design is that a chunk available at LAN
speed is never fetched over a metered, slower, or duty-cycle-limited link. The router
already knows the bandwidth class of every link (`NODE-NETWORK.md` §3); this consults it.

## 5. What may be cached

**Only `PUBLIC` and `REPLICATED` objects.** `PRIVATE` and `SHARED` never enter the shared
cache, in any form, at any time. This is enforced in `otwono-stored`, not left to callers,
and it is the property that makes the whole thing safe to switch on.

Nothing is replicated without the operator's per-collection opt-in, per
`DISTRIBUTED-SERVICES.md` §5. The cache does not create an exception to that rule; it is a
consumer of it.

## 6. Two costs the operator must be told about

Stated here so they are stated in the UI:

- **Holding is publishing.** Serving a chunk tells your neighbours you have it. What a
  household reads is partly inferable from what its node serves. Restricting the cache to
  public and replicated content bounds this; it does not remove it.
- **Serving is carrying.** You will store bytes you did not choose individually. The
  per-collection opt-in is the control, and a purge must always be one action away.

## 7. Integrate, do not reinvent

Per CLAUDE.md §2.3, the mechanisms here are well-trodden and the adapter layer is our job:

| Need | Prior art to draw on |
|---|---|
| Swarm fetch, piece selection | BitTorrent — rarest-first, endgame mode |
| Content-addressed block exchange | IPFS Bitswap — want-lists and provider hints |
| Content-defined chunking | FastCDC, restic, casync |
| Local provider discovery | mDNS, already in use for peer discovery |

## 8. What must be true before this is called done

- A chunk that fails its hash never reaches a caller, proven by a test that serves a
  corrupted chunk from a peer.
- A fetch with three peers holding disjoint pieces completes and is byte-identical to
  origin.
- A `PRIVATE` object cannot be placed in the cache by any code path, proven negatively.
- The cache respects its size cap under sustained pressure and evicts rather than filling
  the disk.
- A T0 node with 512 MiB participates usefully rather than thrashing.

### Where each stands

| Criterion | State |
|---|---|
| A chunk that fails its hash never reaches a caller | **met** — `a_peer_serving_rubbish_wastes_bandwidth_and_cannot_corrupt_the_result`: a peer declaring the true chunk list and serving garbage for every chunk is demerited, dropped, and cannot affect the assembled object |
| A fetch with three peers holding disjoint pieces completes and is byte-identical to origin | **met, on real links.** Three booted VMs on one multicast segment, each having deleted the chunks where `index mod 3` is its own ordinal, so no node holds the whole object; every node completed the fetch and every node reported `large_served=2`. With disjoint shares that is forced rather than likely — a completed fetch *must* have combined two peers. Also host-side in `three_peers_holding_disjoint_pieces_complete_a_fetch` |
| A `PRIVATE` object cannot be placed in the cache by any code path | **met** — `cache::tests::private_content_cannot_enter_the_cache_by_any_path`, asserted negatively and with nothing reaching the disk |
| The cache respects its size cap under sustained pressure and evicts rather than filling the disk | **met** — twenty 64 KiB inserts into a 256 KiB budget, budget asserted after each |
| A T0 node with 512 MiB participates usefully rather than thrashing | **partly** — the 512 MiB default is asserted; "usefully rather than thrashing" needs a real T0 board |

Until each has a test and a log, this document describes an intention.

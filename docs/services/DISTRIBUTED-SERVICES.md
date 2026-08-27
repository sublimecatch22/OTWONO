# Distributed Services

**Status:** `SPECIFIED`, with the second primitive built.

Primitive 1 (content-addressed blocks) is `VERIFIED` — `otwono-store`, ADR-0016, exercised
between booted nodes. Primitive 2 (signed mutable pointers) is `IMPLEMENTED` per ADR-0027:
the record and its rollback rules in `otwono-pointer`, durable storage in
`otwono-store::PointerStore`, and `content.pointer` on the wire — a pointer crosses a real
channel and is verified against the key the Noise handshake proved. Primitive 3 (addressed
messages) does not exist, and no service in §2 is built.

**Verified between booted nodes** on 2026-08-27: two VMs each published `wiki/Getting-Started`
and each resolved the other's, then fetched the object it named. See
`docs/build/VERIFICATION-LOG.md`, which also records what that run did *not* show — no
tombstone, no restart, and one name per node.

The rollback rule is now **on the fetch path** rather than beside it (ADR-0027 §7). Until
that change every pointer that arrived from a peer was verified and accepted whatever the
reader had already seen: the defence existed, passed its tests, and was never consulted. The
reader's memory lives in `otwono-store::PointerStore` and `otwono-netd` reaches it over the
control plane, so it survives a restart and there is one writer to it.

**Verified between booted nodes** on 2026-08-27: each node took its peer's update at sequence
2 over the link, then refused the peer's rolled-back sequence-1 record. The replayed record is
the rightful owner's, genuinely signed and current for that owner; it is refused because the
reader remembers a higher number. Same log entry, which also records that no *third party*
replay has been shown on a wire.

**And it survives the machine going away**, verified 2026-08-27 by shutting both nodes down
and booting them again from the same disks: each came back as itself and still refused its
peer's rolled-back record, naming the sequence it had read off the disk. Nothing is
republished on that boot, so the refusal can only come from the persisted log. The store's
writes were made durable rather than merely atomic to make that claim honest (ADR-0027 §9);
a *hard* power cut is still untested.

## 1. Three primitives, many services

We are not building eight independent systems. Every service is composed from:

1. **Content-addressed blocks** — immutable, BLAKE3-addressed, chunked, deduplicated.
2. **Signed mutable pointers** — a NodeID-owned, monotonically-sequenced record naming a
   current CID. This is how anything changes over time. Built (ADR-0027). The sequence is
   not decoration: an old pointer is genuinely signed by its rightful owner and is simply
   out of date, so a reader that trusted the signature alone could be rolled back to any
   historical version. The defence is state the reader keeps, and a first read has none.
3. **Addressed messages** — end-to-end encrypted envelopes to a NodeID or UserID, with
   store-and-forward for offline recipients.

If a proposed service cannot be expressed in these three, that is a signal to re-examine
the design before adding a fourth primitive.

## 2. Service catalogue

| Service | Composition | Trickle-safe mode |
|---|---|---|
| **Profile site** | `PUBLIC` collection + signed site pointer + HTTP-over-ONM | Plain-text card |
| **Local website** | Same; static site generated locally | Text-only rendering |
| **Wiki** | Append-only signed page revisions; per-page last-writer-wins with explicit merge on conflict | Text diffs |
| **Forum** | Signed posts in topic collections; moderation by subscription to signed moderation lists, never global authority | Headers + bodies on demand |
| **Messaging** | Addressed encrypted envelopes; store-and-forward | Yes, native |
| **Document / image / audio / video sharing** | Chunked blobs + a manifest, streamed on demand | Metadata only; blobs need `Narrow`+ |
| **Media viewing** | Range-requested chunks, local transcode via ffmpeg/mpv | No |
| **Distributed search** | Local full-text index over local + subscribed content; fan-out to reachable peers with per-peer scope and rate limits | Query + top-N titles |
| **AI services** | `otwono-aid` exposed to authorized peers with quotas | Short prompts only |
| **Permission-controlled sharing** | Falls out of `SHARED` labels + per-peer authorization | Yes |
| **Education** | `PUBLIC`/`REPLICATED` curriculum blocks + a `PRIVATE` learner record + signed transcripts. See `EDUCATION.md` | Practice and record-keeping; no generation |
| **Finance** | Entirely `PRIVATE`, passphrase-encrypted, never replicated or cached. See `FINANCE.md` | Yes — arithmetic needs no network |

**The cluster cache is not in this table** because it is not a service — it is how the
first primitive gets from one node to another. See `CLUSTER-CACHE.md` and ADR-0015.

## 3. Addressing

```
onm://<nodeid>/<service>/<path>
onm://otw1:qm7f-2k9x-8v3t-rj5p/wiki/Getting-Started
```

Resolution is by NodeID through the overlay — never by IP, never by DNS. A local resolver
and a browser integration make these addresses work in an ordinary browser on the node.

Optional human-readable names come from **local petname assignment**, not a global
namespace. There is no registry to squat, no auction, and no authority. Users may import
name suggestions from trusted peers; suggestions are never automatic.

## 4. Universal requirements

Every service must:

1. **Work with zero peers.** Local wiki, local notes, local profile, local media, local
   search — all fully functional on one disconnected machine.
2. **Have a `Trickle`-safe representation**, or explicitly declare that it requires
   `Narrow`+ and degrade with a clear message rather than hanging.
3. **Respect visibility labels.** No service may bypass `otwono-stored`.
4. **Be tier-aware.** A T0 node hosts a text profile and relays messages; it does not run a
   full-text index over a media library.
5. **Be independently testable** with a fake network.

## 5. Moderation and abuse

An open, replicating network will carry content that node operators do not want to store or
serve. The design position:

- **Nothing is replicated without the operator's opt-in**, per collection.
- Moderation is **subscription-based**: signed block/allow lists that a node chooses to
  follow. No global authority, and no pretence that a decentralized network has one.
- Operators can always purge local content, and the UI makes that easy.
- Gateways are the abuse chokepoint and carry the strictest default policy, because a
  gateway operator carries real-world legal exposure. This must be said explicitly in the
  UI before someone enables it.

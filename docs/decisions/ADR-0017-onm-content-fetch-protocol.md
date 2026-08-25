# ADR-0017 — The ONM content-fetch protocol: ranged, object-scoped, and verified per chunk

**Status:** accepted · **Date:** 2026-08-24

## Context

`otwono-net` gives two nodes an authenticated, encrypted channel (`SecureChannel`, Noise
XX plus the session proof of ADR-0006/0010). `otwono-stored` gives one node a
content-addressed store with a visibility label at the network boundary (`store.serve`).
Nothing joins them. Today `otwono-netd` completes a handshake, exchanges `Hello`, records
the peer, and **drops the channel**. No content has ever crossed an OTWONO link.

That gap is also the last unmet exit criterion of Phase 5. `docs/services/DATA-VISIBILITY.md`
§6 asks for a demonstration that a `PRIVATE` object never appears on any link. It is
currently proven at the `store.serve` method and nowhere else, which is a proof about a
function, not about a wire.

Four forces shape the protocol.

**Noise frames are small.** A Noise transport message is at most 65535 bytes, and 16 of
those are the AEAD tag. ADR-0016 set `MAX_CHUNK` at 256 KiB. So the largest legal chunk
does not fit in one frame, before anything is said about large objects.

**Links are not comparable.** `BandwidthClass::max_reasonable_payload()` is 256 bytes on a
`Trickle` link (LoRa, AX.25) and 64 MiB on a `Wide` one. A protocol whose message size is
fixed by the content, rather than by the link, is a protocol that works on one medium.

**The peer is authenticated, not trusted.** `SECURITY-MODEL.md` puts this daemon in Z3.
Everything a peer sends is untrusted data. A receiver that cannot check a piece as it
arrives has to accept a gigabyte before discovering it was garbage.

**A refusal must not be a disclosure.** `store.serve` already collapses absent, private,
shared, and damaged into one answer. Anything built on top must not reintroduce the
distinction — and must not add a *new* oracle of its own.

## Decision

Two request types, both **ranged**, both **scoped to an object**, carried as
newline-free JSON inside `SecureChannel` frames after `Hello`.

```
content.manifest { content_id, from_chunk, max_chunks }
  -> { content_id, size_bytes, chunking, visibility, total_chunks, from_chunk, chunks[] }

content.chunk    { content_id, digest, offset, max_bytes }
  -> { content_id, digest, offset, total_length, data }
```

and exactly one error shape:

```
{ error: "not_available", content_id }
```

### The range is not an optimization

Both requests carry a caller-chosen bound and both replies may be short. The requester
loops. The *link*, through `BandwidthClass::max_reasonable_payload()`, decides how much of a
chunk moves per frame; the content never does. It also makes an interrupted transfer
resumable for free, which is the same shape `fetch.get` settled on in ADR-0014 for the same
reason.

**Amended after measuring.** The first draft of this section claimed the result was one
protocol that runs on LoRa and on Ethernet alike. That claim was wrong, and the numbers are
worth stating rather than quietly dropping:

| Message | Bytes on the wire | Fits a `Trickle` frame (256)? |
|---|---:|---|
| Noise session proof (handshake) | 447 | no |
| Manifest reply, no entries | 262 | no |
| Manifest reply, one entry | 360 | no |
| Manifest reply for a `SHARED` object, no entries | 646 | no |
| Chunk reply, empty body | 229 | yes, with 11 bytes to spare |
| Chunk request, largest | 225 | yes |

The shared row was added when ADR-0019 §4 put a sealed content key in the manifest. It is
the number `MANIFEST_ENVELOPE_RESERVE` is sized for, because a requester does not learn that
an object is shared until the first manifest arrives — sizing the window by the smaller
number would mean discovering the reply does not fit only after asking for it. On a link
that already carries a manifest at all, the cost is a few fewer entries per window.

So a `Trickle` link can carry chunk traffic — at about six bytes of payload per
transmission — but cannot carry a manifest window, and the Noise handshake does not
complete over one in the first place. `content::carries_a_manifest()` says so before a fetch
sends anything, rather than letting a `PayloadTooLarge` surface from three layers down. The
protocol works from `Narrow` upward. Making it work on a radio needs a compact encoding
(OQ-23) and a smaller handshake (OQ-24), and neither is in this ADR.

A responder is always free to send less than asked. A requester that receives zero new
bytes twice in a row must stop; without that rule a hostile or broken peer holds the
requester in a loop forever. (fetchd learned this one the expensive way — see defect 29 in
`docs/build/VERIFICATION-LOG.md`.)

### A chunk request names the object it belongs to

The obvious protocol asks for a chunk by digest alone. That is a probe oracle: the store
is content-addressed, so a peer that can guess a digest — the empty file, a common
config, a known photo — learns whether this node holds those exact bytes, whatever label
they carry. Chunks are shared across objects by design, so a private object and a public
one can contain the same chunk.

So every chunk request names a `content_id`, and the responder answers only if:

1. the object exists, **and**
2. its label permits leaving the node, **and**
3. the requested digest is in *that object's* chunk list.

All three failures produce the same `not_available`. The set of chunks a peer can ask about
is therefore exactly the set reachable from an object it is already allowed to have, and
"do you hold chunk X" is not a question the wire can express.

### The manifest is paginated by chunk index

A 1 GiB object is ~16 000 chunks at ADR-0016's 64 KiB average, and a chunk list that size
is ~800 KB of JSON — twelve times a Noise frame and 3000 times a LoRa payload. The
manifest is therefore fetched in windows, like the chunks themselves. `total_chunks` in
every reply lets the requester size the job from the first window.

### Verification is per chunk, at the receiver, before the store

Each chunk is hashed on arrival and compared to the digest the manifest gave, and the
assembled chunk list is compared to the `content_id` that was asked for. A peer cannot
substitute content, and cannot make the receiver buffer more than one chunk before the
first check fires. This is the property that pays for shipping digests in the manifest
rather than re-deriving boundaries at the end: FastCDC is deterministic, so re-chunking the
finished bytes *would* have recovered the same `ContentId`, but only after accepting all of
them.

### The label is checked twice, in two processes

`otwono-stored` decides — it owns the store and it owns the boundary. `otwono-netd` then
checks the `visibility` in the reply again before a byte reaches the link, and refuses
`Private` and `Shared` itself. `DATA-VISIBILITY.md` §4 asks for exactly this duplication,
and the two checks are in different crates, different processes, and different trust zones
on purpose: a bug in either one is not enough to leak.

`otwono-netd` holds `store.serve` and nothing else. It cannot call `store.get`, so the
network daemon has no path to a private object even if it wanted one.

### Serving is answered from `store.serve_manifest` and `store.serve_chunk`

Two new methods on `otwono-stored`, both guarded by the existing `store.serve` capability
and both using the same collapsed refusal. They exist so that `otwono-netd` never holds
more than one bounded range in memory on a peer's behalf — the whole-object `store.serve`
returns everything inline, which is fine for a local caller and wrong for a remote one.

## Consequences

**Good.** Content moves between nodes over the channel that already authenticates them,
with no new transport and no new key. A transfer resumes. A lying peer is caught at the first bad chunk. The chunk-probe oracle is
closed by construction rather than by rate limiting. Phase 5's fourth criterion becomes
testable on an actual link.

**Bad, and worth naming.**

- **JSON with base64 bodies costs a third of the payload, and the envelope costs more than
  that.** Two 64-character hex ids and their field names are most of a 229-byte chunk
  envelope. On a `Wide` link that is irrelevant; on a radio it is the whole budget. Accepted
  for now because one encoding everywhere is worth more than the bytes, and because the
  framing is versioned — a compact encoding is a later, additive change (OQ-23).
- **`Trickle` links are out of scope until OQ-23 and OQ-24 are answered.** Measured, stated
  above, and refused explicitly rather than left to fail obscurely.
- **Traffic analysis is untouched.** Sizes and timings are visible to anyone watching the
  link. Noise encrypts, it does not pad.
- **There is no request pipelining and no concurrency.** One request, one reply, in order,
  per channel. That is slow over a high-latency link and it is deliberate: a correlation id
  and out-of-order replies are a state machine, and this slice does not need one.
- **A peer can still ask for public objects it should not care about**, repeatedly. There
  is no rate limit here. That is OQ-17's territory (freeloading) and ADR-0015 already
  argues against building accounting before there is evidence of harm.
- **`SHARED` is not servable at all yet.** It needs per-recipient key wrapping against the
  identity daemon's agreement keys. Until then it fails closed, which is the right way for
  it to be missing.

## Alternatives rejected

- **Whole objects in one message.** Simplest, and impossible: 256 KiB chunks do not fit in
  a 65535-byte Noise frame.
- **Byte ranges over the object, verified by re-chunking at the end.** Genuinely simpler on
  the wire — no digests, no manifest, no pagination — and it does verify, because FastCDC
  is deterministic. Rejected because the check only fires after the last byte, which means
  a hostile peer decides how much a receiver buffers before it can be caught.
- **Chunk requests keyed on digest alone.** Smaller messages, natural deduplication across
  objects, and a disclosure oracle over a content-addressed store.
- **libp2p Bitswap.** A mature, well-understood answer to almost this question. Rejected
  because taking it means taking libp2p's transport and stream muxing, which would sit
  beside the Noise channel ADR-0006 already established rather than on top of it, and
  because Bitswap wants a want-list gossiped to many peers — a design for a public DHT,
  not for a link that may be a duty-cycle-limited radio. Revisit if the neighbourhood cache
  grows past one-hop.
- **Reusing the JSON-RPC control plane over the channel.** Tempting: the framing exists.
  Rejected because the control plane's contract is "a local caller with a capability
  token", and putting a remote, untrusted speaker on it would make every daemon's method
  table part of the wire's attack surface.

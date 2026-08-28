# ADR-0029 (DRAFT) — A compact encoding for the ONM wire

**Status: DRAFT — for founder review. Not accepted, not implemented.**
**Date:** 2026-08-27
**Phase:** 9 (mesh and radio), raised early because Phase 6 is now adding wire messages.

> This document exists because ADR-0028's implementation was told **not** to choose an
> encoding, and choosing one by accident is exactly what happens when a codec question is
> left implicit while messages are being added. Nothing here has been built. The numbers are
> measured; the recommendation is a recommendation.

## Context

`OPEN-QUESTIONS.md` carries two entries that are really one problem:

- **OQ-23.** Measured 2026-08-24: an ONM content message spends **229–360 bytes on its JSON
  envelope alone** — two 64-character hex ids and their field names — so a manifest window
  does not fit an EU868 LoRa frame at all and a chunk reply carries about six bytes of
  payload.
- **OQ-24.** The handshake does not fit either: the two Noise session-proof frames are **447
  bytes each** against a 256-byte `Trickle` payload, so ONM cannot authenticate over LoRa
  however small the content messages become.

Phase 6 has since added four more messages — `content.pointer` (ADR-0027) and
`content.relayable` / `content.addressed_to_me` (ADR-0028) — and the pattern holds. Measured
today, at the compact end (no whitespace):

| Message | Bytes |
|---|---|
| `content.relayable` request | 47 |
| `content.addressed_to_me` request | 53 |
| `carried` reply, one entry | 236 |

A carried entry is 204 bytes, of which **55 are field names**. The actual information is one
32-byte digest, one 34-byte node id, and two integers.

`DISTRIBUTED-SERVICES.md` §4.2 requires every service to have a `Trickle`-safe
representation, and messaging "natively". An envelope descriptor that cannot cross the link
messaging is supposed to work best on is a contradiction the project should not ship.

## What is *not* the problem

Worth stating, because it changes the answer. The costs are not evenly spread:

- **Field names are ~27% of a carried entry** and are pure repetition — the same fifteen
  names cross a link thousands of times in a session.
- **Hex is 2× the bytes of the digest it encodes.** Every content id on the wire is 64
  characters carrying 32 bytes.
- **Base32 node ids are ~1.6×.** A 34-byte multihash spends 59 characters.
- **The integers are already small** and gain almost nothing from a different codec.

So roughly **half of a typical message is encoding overhead**, and the two biggest
contributors are structural (names, and text-encoded binary) rather than anything a
general-purpose compressor would be needed for.

## Candidates

### 1. CBOR (RFC 8949)
Binary, self-describing, schema-free — a direct swap for JSON. Digests and keys become byte
strings at their natural size; integers are compact. Field names remain, though CBOR's
canonical form can be paired with integer keys.

*Estimated for a carried entry:* roughly 100 bytes against 204, and about 75 with integer
keys.

**For:** mature, widely implemented, a mechanical translation of the current model. Keeps a
self-describing wire, so `socat` inspection degrades to "needs a decoder" rather than
becoming impossible. **Against:** loses the property that made JSON right everywhere else —
a person can read a frame with no tooling.

### 2. Session-scoped short handles for ids
Not an encoding at all: the first mention of a content id or node id in a session is full,
and later mentions are a small integer. Composes with any codec.

*Estimated:* dramatic on repetitive exchanges — a chunk conversation naming one object
hundreds of times — and nil on a single-shot message.

**For:** attacks the biggest single cost without changing the wire format. **Against:** adds
per-session state on both ends, and state is where protocol bugs live. A handle table that
disagrees between the two ends is a class of failure the current stateless messages cannot
have.

### 3. Shorter field names
`eid`/`rcpt`/`sz`/`exp` instead of the current names. Cheap and unglamorous.

*Estimated:* about 35 bytes off a carried entry, ~17%.

**For:** trivially implementable, no new dependency, no state. **Against:** buys the least,
and spends the readability the project chose deliberately for the smallest return.

### 4. Do nothing on the codec; fragment at the link layer
OQ-24's note already observes that link-layer fragmentation is the more general answer, since
the handshake cannot be shrunk enough by any codec.

**For:** solves the handshake, which no codec choice does. **Against:** a new failure mode on
a lossy medium, and it does not reduce the bytes actually sent — on a duty-cycle-limited
radio the airtime is the cost, not the frame count.

## Recommendation, for review

**CBOR with integer keys, plus link-layer fragmentation, and short handles only if measurement
later shows they are needed.**

Reasoning:

1. **Fragmentation is not optional.** No codec makes a 447-byte handshake frame fit 256
   bytes, so OQ-24 needs it regardless. Deciding the codec without it solves half a problem.
2. **CBOR is the largest single win** available without introducing state, and integer keys
   take the field-name cost to near zero. It is a mechanical change: the schemas stay the
   contract, and the JSON Schema documents describe the *model* rather than the bytes.
3. **Short handles should wait.** They are the only candidate that adds cross-message state,
   and the case for them is strongest exactly where measurement is cheapest to do later — a
   real chunk conversation over a real radio. Adding state before that measurement would be
   buying a bug class on an estimate.
4. **Keep JSON as an alternative representation**, selectable per link, rather than deleting
   it. The "readable with `socat`" property is genuinely load-bearing for development and for
   a second implementation reading the schemas; losing it entirely to save bytes on a link
   most nodes never use would be a poor trade.

## What this would cost

- Every wire type gains a codec-agnostic representation. ADR-0028's implementation already
  separates the envelope and relay *types* from their serialization for this reason, so the
  new messages would not need reshaping — but the older ones would.
- The schemas stay authoritative and gain a note that they describe a model with two
  encodings, not a byte layout.
- A negotiation step: which encoding this link speaks. That is new protocol, and it is where
  the risk sits.

## What this draft does not decide

Everything. It is a draft. In particular it does not decide the negotiation mechanism, whether
the integer-key mapping is per-message or global, or whether JSON survives at all in the long
run.

## References

`OPEN-QUESTIONS.md` OQ-23 and OQ-24, ADR-0017 (the protocol shape being encoded), ADR-0028 §9
(the newest messages measured here), `docs/services/DISTRIBUTED-SERVICES.md` §4.2 (the
`Trickle`-safe requirement).

# ADR-0016 — Content-defined chunking with FastCDC at 16/64/256 KiB

**Status:** accepted · **Date:** 2026-08-24 · Resolves **OQ-16**

## Context

Every object in the content store is split into chunks addressed by their BLAKE3 digest.
ADR-0015 named the consequence of getting the split wrong:

> Chunk size and boundary algorithm determine whether two nodes that fetched the same file
> independently can serve each other. Get it wrong and the swarm silently does not form.

That makes this a **network-wide compatibility constant**, not a tuning knob. Two nodes that
chunk the same bytes differently produce different digests for the same data and cannot help
each other — and nothing reports an error, they simply never match. It is fixed by schema,
versioned, and changing it splits the network. Hence an ADR before the code, and one backed
by measurement.

Three things are actually in tension: how well chunks survive an edit, what the chunk index
costs on a small board, and how big a unit of transfer should be on a slow link.

## Measurements

Taken 2026-08-24 on this x86_64 development machine (4 cores), `fastcdc` 5.0.0 (FastCDC
v2020) with BLAKE3, over three inputs chosen to match what a node actually stores: 256 MiB
of high-entropy data standing in for quantized weights, a 57 MiB rootfs tar (binaries,
config, real internal duplication), and 117 MiB of source and documentation text.

**"after-insert"** is the fraction of chunks still shared after 64 bytes are inserted near
the front of the file — the case that separates content-defined chunking from fixed blocks.
**"fixed"** is the same measurement with fixed-size blocks of the same nominal size.

| params | chunks (256 MiB) | mean | index/GiB | after-insert | fixed |
|---|---|---|---|---|---|
| 2/8/16 KiB | 27,835 | 9.4 KiB | 5.1M | 100.0% | 0.0% |
| 4/16/64 KiB | 13,284 | 19.7 KiB | 2.4M | 100.0% | 0.0% |
| 8/32/128 KiB | 6,661 | 39.4 KiB | 1.2M | 100.0% | 0.0% |
| **16/64/256 KiB** | **3,340** | **78.5 KiB** | **0.6M** | **99.9%** | **0.0%** |
| 32/128/512 KiB | 1,674 | 156.6 KiB | 0.3M | 99.9% | 0.0% |
| 64/256/1024 KiB | 831 | 315.5 KiB | 0.2M | 99.9% | 0.0% |

The rootfs and text inputs produce the same shape; the full output is in
`docs/build/VERIFICATION-LOG.md`.

Throughput, same machine, 256 MiB:

| params | chunk MiB/s | chunk + hash MiB/s |
|---|---|---|
| 4/16/64 KiB | 1,640 | 1,035 |
| 8/32/128 KiB | 1,763 | 1,146 |
| 16/64/256 KiB | 1,750 | 1,191 |
| 64/256/1024 KiB | 1,775 | 1,252 |

### What the numbers decide, and what they do not

- **Content-defined chunking is not optional.** After a 64-byte insertion, CDC keeps
  99.5–100% of its chunks and fixed-size blocking keeps **0–2.4%**. That is the entire
  justification for the extra complexity, in one column.
- **Boundary stability does not choose the parameters.** Every set holds ≥99.5%.
- **Throughput does not choose them either.** It varies by under 20% across a 32× range of
  chunk sizes, and hashing dominates chunking in every case.
- **Index cost does.** It spans 25× across the table, and it is the number that bites on a
  small board.

So the decision is index cost against transfer granularity, and nothing else.

## Decision

**FastCDC v2020, min 16 KiB / average 64 KiB / max 256 KiB, one parameter set for the whole
network.**

| | |
|---|---|
| Algorithm | FastCDC v2020, via the `fastcdc` crate (7 crates in the tree, no async runtime) |
| Min / avg / max | 16 KiB / 64 KiB / 256 KiB |
| Digest | BLAKE3, as everywhere else in the system |
| Versioned in | the object schema, as a required field |

**Not tunable per node, per object class, or per tier.** A node that chunks text finely and
models coarsely would produce digests no other node shares, which defeats the point. The
parameters are the same on a Pi Zero and a workstation.

### Why 16/64/256

Index cost at 64 KiB average is modest at every tier the capability policy defines:

| Tier | Cache contribution (ADR-0015) | Chunks | Index |
|---|---|---|---|
| T0 | 512 MiB | ~8,000 | ~0.4 MiB |
| T1 | 4 GiB | ~65,000 | ~3 MiB |
| T2 | 32 GiB | ~525,000 | ~24 MiB |
| T3 | 128 GiB | ~2.1M | ~96 MiB |

At 16 KiB average the T3 figure is roughly 400 MiB of index, which is a real cost for no
measured benefit in stability. At 256 KiB average the index is cheaper still, but a 256 KiB
average implies a 1 MiB maximum chunk, and a single chunk is the smallest thing a peer can
usefully serve — on a `Narrow` link that is minutes, and it wastes a whole chunk when a
transfer fails partway.

64 KiB sits where the index is cheap on the smallest board this OS targets and the transfer
unit is still small enough to spread across peers and to retry cheaply.

## Consequences

**Good.** One constant, decided once, with the arithmetic written down. A file edited
anywhere still shares essentially all of its chunks, so incremental sync and cluster
serving both work on changed data rather than only on identical copies. The index fits
comfortably in the memory of the smallest supported node. `fastcdc` is a small, focused
dependency with no runtime, matching the workspace's existing shape.

**Bad, and worth naming.**

- **This is a commitment.** Changing these numbers later partitions the network into nodes
  that can serve each other and nodes that cannot, silently. The schema carries the
  parameters so a future version can be *detected*, but a mixed network still degrades to
  no sharing between the two halves.
- **Not measured on ARM.** Throughput here is x86_64 with SIMD BLAKE3. A Cortex-A72 will be
  several times slower, and a 4 GB model must be chunked and hashed before it can be stored
  or served. The number is unknown and should be measured on real hardware before anyone
  promises a time. The parameter choice does not depend on it — throughput barely varies
  across the table — but the user-facing wait does.
- **High-entropy data does not dedup, and nothing changes that.** Quantized model weights
  are near-incompressible and near-unique; chunking buys resumable, parallel, verifiable
  *transfer* for them, not storage savings. Only the transfer property is claimed.
- **Objects smaller than 16 KiB are a single chunk.** That is most wiki pages, manifests and
  lesson files, and it is correct — but it means per-object overhead, not per-chunk
  overhead, dominates for a store full of small documents.

## Alternatives rejected

- **Fixed-size blocks.** Simpler and faster, and the measurement kills it: 0–2.4% of chunks
  survive an insertion, against ~100% for CDC. A store that only dedups byte-identical files
  is a store that dedups almost nothing real.
- **Rabin fingerprinting**, as in the original LBFS work and older restic. Well understood
  and slower, with no compensating advantage — FastCDC exists because it is the same idea
  done faster.
- **Per-tier or per-content-type parameters.** Superficially attractive: fine chunks for
  text, coarse for models. It breaks the one property the whole design rests on, since two
  nodes must chunk identical bytes identically.
- **Deferring the choice and making it configurable.** The most expensive option. A
  configurable network-wide constant is a network that has quietly partitioned by the time
  anyone notices.

## References

- ADR-0015 (the cluster cache, which this makes possible), ADR-0007 (labels — what may
  be cached at all), ADR-0004 (tiers — where the index budget comes from).
- `docs/services/CLUSTER-CACHE.md`, `docs/build/VERIFICATION-LOG.md`.

# ADR-0018 — Large content moves as files handed to the caller's uid, not as bytes on the control plane

**Status:** accepted · **Date:** 2026-08-24

## Context

Everything in the content path — `store.put`, `store.get`, `store.serve`, `cache.put`,
`cache.get`, `net.fetch` — carries its object base64-encoded inside one JSON-RPC line. The
control plane is newline-delimited JSON with a 1 MiB line cap (`MAX_LINE_BYTES`), and base64
costs four characters per three bytes, so the real ceiling is **640 KiB** per object.

That number was measured, not chosen. `MAX_INLINE_BYTES` claimed 32 MiB until 2026-08-24,
when a 900 KiB test fixture could not be stored and the caller got `BrokenPipe` — the
server's line reader closes the connection rather than replying (defect 35,
`docs/build/VERIFICATION-LOG.md`).

**Amended while implementing.** The cap was, at that point, only enforced on *requests*. The
client's `read_line` had no bound at all, so `store.get` cheerfully returned an 8 MiB reply
and the caller allocated it — which means the "ceiling" bit on the way in and not on the way
out. That asymmetry is now closed in both directions (defect 36): the client refuses a reply
over `MAX_LINE_BYTES`, and `store.get`, `cache.get` and the inline `store.serve` check the
object's size *before* reading it, so a caller gets a sentence naming `store.export` rather
than a transport error after the daemon has already encoded eight megabytes.

640 KiB is not a media ceiling. It is barely a photograph. `NEIGHBOURHOOD-CACHE.md` is
written about a household's media, `PORTABLE-APPS.md` about signed application bundles, and
`AI-RUNTIME.md` about model weights measured in gigabytes. The subsystem that exists cannot
carry any of them.

## Decision

**Objects above the inline cap move as files on the local filesystem. The control plane
carries the metadata, the capability decision, and a path — never the bytes.**

This is not a new idea in this repository. `otwono-fetchd` already does exactly this: a
download lands in `/var/lib/otwono/fetch/<id>.blob` and `fetch.get` returns a `blob_path`,
because a 4 GiB model was never going to fit in a JSON-RPC reply either (ADR-0014). The
decision here is to stop treating that as a special case of egress and make it the way large
content moves generally.

### Export: the daemon writes, then gives the file away

`store.export` writes an object's plaintext into a daemon-owned directory
(`/var/lib/otwono/export`, 0700), then **chowns it to the calling uid** and returns the path.
The caller reads it and deletes it; a reaper removes what callers abandon.

The calling uid is not something the caller asserts. It comes from `SO_PEERCRED` on the Unix
socket, which the kernel fills in — the same `PeerIdentity` every policy decision and audit
record in this project already rests on. A caller cannot ask for a file to be handed to
somebody else.

### Import: the daemon reads a file the caller already owns

`store.import` takes a path the caller names. The daemon opens it with `O_NOFOLLOW`, then
`fstat`s the descriptor it actually got and refuses unless `st_uid` is the calling uid and it
is a regular file. Every subsequent operation uses that descriptor, never the path again.

That ordering is the whole of the safety argument, and it is worth being explicit about why:
this daemon runs as root. Checking a *path* and then opening it is the classic
time-of-check-to-time-of-use bug — the caller swaps the file for a symlink to
`/etc/shadow` between the two. Checking the *descriptor* after opening it cannot be raced,
because the descriptor already refers to a particular inode.

`O_NOFOLLOW` refuses a symlink at the final component. The uid check refuses a file the
caller does not own, which is what stops "import `/etc/shadow`" from being a way to read it.

### The inline methods stay

`store.put` and `store.get` keep their 640 KiB cap and their inline bytes. Most objects in
this system are small — a note, a manifest, a learner's record, a signed peer list — and
making every one of them a file to open, read and unlink would be worse in every way that
matters. Two paths, chosen by size, with the boundary documented and asserted.

## Consequences

**Good.** The content path can carry the things the specifications describe. It reuses the
pattern `otwono-fetchd` already established rather than inventing a second one. The bytes
never traverse the control plane, so the 1 MiB line cap can stay small and hostile — an
unbounded line is an unbounded allocation from an untrusted local caller, and raising it
would have been the tempting wrong answer.

**Bad, and worth naming.**

- **Exported plaintext sits on disk.** The store is encrypted at rest; an export is not. It
  lives in a 0700 directory, is 0600 and owned by the caller, and is meant to be short-lived
  — but between the write and the caller's `unlink` the plaintext of a `PRIVATE` object is a
  file. That is what "export" means and there is no version of handing a user their own data
  that avoids it, but a UI must not present it as free.
- **An abandoned export is a leak that grows.** A caller that crashes between `store.export`
  and its `unlink` leaves plaintext behind. Hence the reaper, and hence a bounded export
  directory — but a reaper is a thing that can fail silently, and this is the first place in
  the project where correctness depends on cleanup happening later.
- **`net.fetch` still assembles in memory.** Fan-out holds the whole object in RAM before
  writing it anywhere. A file-based export does not fix that; a T0 board with 512 MiB of RAM
  cannot fetch a 2 GiB object however the result is delivered. Streaming a fan-out fetch
  directly to a file is a further change and is **not** in this ADR (OQ-25).
- **Two code paths for the same operation.** Small objects go inline and large ones go
  through a file, so every consumer has to handle both, and the seam is a place bugs will
  live. Mitigated by making the boundary explicit and asserted rather than implicit.
- **The export directory is a new thing to size.** It is not covered by the neighbourhood
  cache's budget and nothing evicts from it under pressure; it is bounded by the reaper and
  by a free-space floor, which is weaker.

## Alternatives rejected

- **Raise `MAX_LINE_BYTES`.** One constant, and it moves the problem rather than solving it:
  a line cap is an allocation an untrusted local caller chooses, and no value both fits a
  video and is safe to allocate per connection. It would also have to be raised again.
- **Pass a file descriptor with `SCM_RIGHTS`.** Genuinely the most elegant answer: no
  temporary file, no plaintext at rest, no reaper, and no path to validate. Rejected here
  because the store is chunked and encrypted — there is no single file to hand over, so the
  daemon would have to materialise a decrypted temporary file *anyway* and pass a descriptor
  to that, which has the same plaintext-on-disk cost plus ancillary-data plumbing the JSON
  line framing does not carry. Worth revisiting if the store ever gains a whole-object
  representation.
- **A chunk-at-a-time method on the existing socket.** `store.read_chunk { content_id,
  index }`, looped. No new mechanism at all, which is attractive. Rejected on arithmetic: a
  4 GiB object is ~65 000 round trips *and* base64's 33% tax on 4 GiB of data, and the
  base64 is the part that hurts. Left as the fallback if the file path proves worse in
  practice than it looks.
- **Writing to a caller-named path.** Simpler, and a root daemon writing wherever a caller
  points it is a privilege escalation with extra steps.
- **A second socket that speaks binary.** A real answer for streaming, and a second protocol
  to specify, version, test and secure. Not worth it for moving files around one machine.

## References

- ADR-0014 (`otwono-fetchd`'s spool — the pattern this generalises), ADR-0015 (the
  neighbourhood cache, whose content this unblocks), ADR-0017 (the ONM content-fetch
  protocol, which has its own separate framing and is unaffected).
- **OQ-25** — streaming a fan-out fetch directly to a file, so a small board can fetch an
  object larger than its RAM.

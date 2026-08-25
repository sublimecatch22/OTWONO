# ADR-0019 — `SHARED` objects: encrypt before chunking, wrap per recipient, and put the unwrapping key in `otwono-idd`

**Status:** accepted · **Date:** 2026-08-25 · **STATUS: partly IMPLEMENTED**

- **IMPLEMENTED (unit and integration tested, never booted):** §2's sealed box
  (`seal_to`/`open_with`), and §3's third key — generated at startup by `otwono-idd`,
  vouched for under its own signing domain, recorded in `node.key`, published in `node.pub`
  and by the open `id.sharing_binding` method.
  `id.unwrap_shared` exists behind its own capability, which no shipped policy grants
  because nothing calls it yet.
- **SPECIFIED, no code:** §1 (encrypt then chunk), §4 (authorized serving and the
  confirmation for `SHARED` egress), and §5 (adding and removing recipients). Until those
  exist `SHARED` still fails closed everywhere, exactly as described under Context.

## Context

`SHARED` is the one visibility label that does not work. `DATA-VISIBILITY.md` §1 defines it
as "available only to explicitly authorized users or nodes", §5 says it is "a per-object
content key, wrapped once per authorized recipient with an X25519 key agreement", and §3
gives the object record an `authorized` field. None of that is built.

What exists instead is a refusal. `Visibility::may_leave_the_node_unattended()` returns
false for `Shared`, so a `SHARED` object is treated exactly like a `PRIVATE` one at every
boundary. That is the correct way for a missing feature to be missing — it fails closed —
and it has been carried as a caveat in every verification-log entry since Phase 5 slice 3.

Three questions have to be answered before any of it can be written, and each has a wrong
answer that would look reasonable.

## Decision

### 1. Encrypt the object, then chunk the ciphertext

A `SHARED` object is encrypted **as a whole, before chunking**, with a fresh per-object
content key. Its chunks are ciphertext, its chunk digests are over ciphertext, and its
`ContentId` is therefore over ciphertext.

The alternative — chunk the plaintext, then encrypt each chunk — keeps digests over
plaintext and is wrong twice over. A holder who *guesses* the plaintext can confirm the
guess against the digest, which for a document whose contents are largely predictable
(a form, a template, a known file) is a real disclosure. And the object's id would be
**identical** to the id of the same file stored `PUBLIC`, so merely holding a `SHARED`
object would tell its holder which known file it is.

**Framed, in implementation.** "As a whole" means the chunking sees only ciphertext; it
does not mean one AEAD invocation over the object. A single invocation would require
holding the object in memory to seal it and again to open it, which is what ADR-0018 exists
to avoid — a `SHARED` video is no smaller than a `PUBLIC` one. The plaintext is therefore
sealed in 1 MiB frames using the STREAM construction, and the concatenated frames are what
gets chunked. Every property this section asks for survives: boundaries fall on ciphertext,
digests are over ciphertext, the id is over ciphertext. STREAM rather than independently
nonced frames because it is what makes truncation and reordering fail to decrypt instead of
yielding a plausible shorter object.

The cost is real and worth naming: a `SHARED` object does not deduplicate against its own
plaintext, and sharing one file with two different recipient sets produces two unrelated
ids. Both follow from the encryption being meaningful, and the second is arguably a feature
— two shares of the same document are not linkable by a holder.

### 2. Wrap with an ephemeral sender key, not a long-term one

Each recipient's copy of the content key is sealed to that recipient's X25519 public key
using a **fresh ephemeral keypair per object**, libsodium-sealed-box style. The wrapping
node needs no long-term key of its own to share something.

That matters because the daemon doing the wrapping is `otwono-stored`, which today holds
exactly one secret — the storage key — and has no identity key at all. Requiring it to hold
one would be a new trust boundary for no gain: a static-static agreement would let a
recipient prove *who* shared with them, which is not a property `DATA-VISIBILITY.md` asks
for and which the object's `owner` field already records under the node's signature.

### 3. The unwrapping key is a **third** node key, held by `otwono-idd`

To read a `SHARED` object a node must hold the X25519 secret its copy was sealed to. There
is already an X25519 key on a node — the agreement key — but it lives in `otwono-netd`
(ADR-0010), which is the Z3 process that parses input from the network.

Unwrapping there would mean a content key existing in the hostile-input daemon's memory and
then crossing the control plane to `otwono-stored`. ADR-0010's whole point is that
`otwono-netd` holds only what Noise needs, and a sharing key is not what Noise needs.

So a node gets a **sharing key**: X25519, generated alongside the signing key, held by
`otwono-idd`, vouched for by the node's Ed25519 key exactly as the agreement key is, and
published in `PublicIdentity`. `otwono-stored` asks `otwono-idd` to unwrap
(`id.unwrap_shared`), guarded by its own capability. The content key returns to the daemon
that already holds the storage key, so no new boundary is crossed.

Three keys per node, each with one job:

| Key | Held by | For |
|---|---|---|
| Ed25519 signing | `otwono-idd` | the node's name and every signature |
| X25519 agreement | `otwono-netd` | the Noise handshake, and nothing else |
| X25519 sharing | `otwono-idd` | unwrapping `SHARED` content keys |

### 4. Serving stays uniform, and the refusal stays uniform

`store.serve_*` will accept `Shared` **only** when the asking peer's NodeID is in the
object's `authorized.nodes`. Every other case — not authorized, not present, damaged,
private — returns the same `not_available` it returns today. A peer that could distinguish
"you are not on the list" from "that is not here" could enumerate both what a node holds and
who it shares with.

Egress of `SHARED` requires confirmation at `otwono-permd`, per `DATA-VISIBILITY.md` §4.
That is a separate capability from `store.serve`, because serving public content unattended
and serving shared content are different decisions.

### 5. Removing a recipient does not un-share

Adding a recipient re-wraps the content key for them. Removing one deletes their wrapped
copy and nothing else: they may already hold the ciphertext and their key. The API says so
in its reply, the way `store.demote` already does, and a UI that implies otherwise is lying.
Genuinely revoking access means re-encrypting under a new content key and re-sharing — a
different, more expensive operation, and not this one.

## Consequences

**Good.** The last visibility label becomes real. The wrapping needs no new long-term secret
in the daemon that does it. The unwrapping key is in the daemon that already holds keys,
rather than in the one that faces the network. A holder of a `SHARED` object learns nothing
from its id, and cannot confirm a guess about its contents.

**Bad, and worth naming.**

- **A third key is a third thing to lose, back up, and rotate.** Losing the sharing key makes
  every object shared *to* this node unreadable, while leaving objects shared *by* it fine.
  That asymmetry will surprise someone.
- **Rotation is not designed here.** `id.rotate` replaces the signing key and endorses the
  new one; what happens to already-wrapped content keys when a sharing key changes is
  unanswered (**OQ-27**).
- **No forward secrecy.** A recipient's sharing key compromised tomorrow reads everything
  ever shared to it. Per-object ephemeral senders give sender-side unlinkability, not
  recipient-side forward secrecy, and getting that would need key rotation with re-wrapping.
- **`SHARED` cannot use the neighbourhood cache**, by construction and by rule. It also
  cannot dedup. A household sharing a large video with three neighbours stores it four
  times.
- **The authorized list is a privacy object in itself.** It names who a node shares with,
  and it lives in the object record. A node that holds a `SHARED` object it cannot read can
  still read the list of who can. That is a real leak and it is not fixed here (**OQ-28**).

## Alternatives rejected

- **Chunk the plaintext and encrypt each chunk.** Keeps dedup and the familiar id, and lets
  a holder confirm a guessed plaintext by its digest. Rejected on that alone.
- **Reuse the agreement key in `otwono-netd`.** One fewer key, at the cost of putting content
  keys in the process that parses hostile input. This is exactly the trade ADR-0010 refused.
- **A static-static agreement using an identity key held by `otwono-stored`.** Gives sender
  authentication that nothing asks for, and adds an identity secret to a daemon that has none.
- **Symmetric per-recipient pre-shared keys.** No public-key operations, and every pair of
  nodes needs an out-of-band exchange first — which is the problem the node identity already
  solves.
- **Leave `SHARED` refusing, and tell people to use `PUBLIC` with obscurity.** Named because
  it is what happens by default if this is deferred again, and because it is how a
  local-first system quietly becomes one where the only real choice is "everyone" or "nobody".

## References

- ADR-0006 (Ed25519 node identity), ADR-0010 (the two-key split this adds a third to),
  ADR-0007 (visibility labels), ADR-0017 (the serve path this extends).
- `docs/security/DATA-VISIBILITY.md` §3 (`authorized`), §4 (confirmation for `SHARED`
  egress), §5 (per-recipient wrapping).
- **OQ-27** sharing-key rotation, **OQ-28** the authorized list as metadata.

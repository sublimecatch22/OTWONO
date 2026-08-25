# ADR-0024 — The confirmation channel: asynchronous, bound to one request, and refused to the subject that asked

**Status:** accepted · **Date:** 2026-08-25 · **STATUS: SPECIFIED** — no code exists yet.

## Context

`policy.rs` turns `Allow` into `Ask` for any `always_confirm` action, and `service.rs` then
returns `confirmation_required`, because nothing can ask a person. That is the correct
fail-closed answer to a missing feature, and it has been carried as a caveat since Phase 4.

It is now blocking four capabilities, not one: `fs.delete`, `net.egress`, and — since
ADR-0022 and ADR-0023 — `wallet.create`, `wallet.sign` and `wallet.export_seed`. The wallet
is the case that forces the issue, because a wallet that cannot be created is a subsystem
that cannot be used at all, and `SECURITY-MODEL.md` §2 lists eight categories of action that
must confirm "regardless of policy or model confidence". None of them can happen today.

The question this settles is not "how do we show a dialog". There is no desktop, T0 nodes are
headless, and a design that assumes a screen would exclude the hardware this OS targets. The
question is **what makes a confirmation mean anything**, and the answer has to survive the
fact that the thing wanting the permission is a program running on the same machine as the
thing granting it.

## Decision

### 1. Asynchronous. `perm.request` never blocks on a person

An `Ask` decision creates a **pending confirmation** — a record with an id — and returns
`confirmation_required` carrying that id. The caller is not parked.

Blocking is the obvious design and it is wrong twice:

- **A daemon would hold a thread waiting on a human who may be asleep.** `otwono-stored` and
  `otwono-netd` serve other callers; a wallet confirmation must not degrade the mesh.
- **It hands an agent a denial-of-service.** A caller that can make a privileged daemon block
  can make it block repeatedly. Prime directive 2 — *works offline* — extends to *works while
  nobody is home*: an unanswered confirmation must cost nothing.

The requester later calls `perm.claim` with the id. Approved yields the token; still pending
yields `confirmation_required` again; denied or expired yields `forbidden`.

### 2. One confirmation authorises exactly one request

The pending record stores the **subject, action, resource and a nonce**, and approving it
authorises that tuple and nothing else. It is consumed on claim.

This is the difference between a confirmation and a policy change. "Yes, delete
`/home/u/tax-2025.ods`" must never become "yes, `fs.delete`". A person shown one sentence has
consented to one sentence, and an approval that widened beyond it would be a lie about what
they agreed to. A caller that wants the same action again asks again.

The resource is part of the identity for the same reason: *delete a file* and *delete which
file* are not the same question, and a confirmation that omitted the answer would be
unreadable.

### 3. The subject that asked may not confirm — and this is the whole design

Confirmation arrives on a **separate socket**, `/run/otwono/confirm.sock`, and
`otwono-permd` checks peer credentials on it. **An approval whose uid matches the requesting
subject's uid is refused**, recorded as refused, and the pending record stays pending.

A subject confirming its own request is not a confirmation; it is a program agreeing with
itself. Every other rule here is bookkeeping, and this one is the security property.

The socket is separate rather than a method on the existing one so that its permissions can
differ from the control plane's — a control-plane socket every daemon must reach cannot also
be the socket only a person may reach.

### 3a. Correction: a confirmer set, not "somebody else"

**Amended 2026-08-25, the same day, on tracing who actually calls `perm.request`.** §3 above
is wrong, and the way it is wrong would have surfaced exactly when the uid split made it
live — which is the worst time to find it.

The askers are CLIs. `otwono-hwctl`, `otwono-aictl`, `otwono-storectl` all call
`perm.request`, and their subject is **the uid of the person who ran them**. Under §3, a
person who runs `otwono-storectl` and is then shown "delete `/home/u/tax-2025.ods`?" would be
**refused their own approval**, because the asker's uid is theirs. On a household node with
one person, that refuses every confirmation there is.

The mistake was the invariant. §3 optimised for *two parties*, and the property that actually
matters is **a human was shown the consequence and assented**. For a person at a terminal,
asking and approving are two genuine acts by one party, and the second is the one where they
see the resource and the blast radius. That is the value; requiring a second human to approve
a household deleting its own file is absurd.

**The rule is therefore membership, not difference:** only a subject in a configured
**confirmer set** may approve. The set defaults to **empty**, so an unconfigured node
confirms nothing — the same fail-closed state as before, reached honestly.

This keeps everything §3 was protecting:

- **An agent cannot approve its own request**, because an agent's uid is not in the set. That
  was the real threat, and it is still refused.
- **An agent cannot approve anything at all**, which is stronger than §3: under the old rule
  an agent could have approved a *different* subject's request.
- **A person may confirm their own request**, which is the normal flow and which §3 broke.

It follows the precedent this project already prefers (CLAUDE.md §2.3): `sudo`'s wheel and
polkit's admin identities both work this way, and for the same reason.

**What did not survive:** the claim that a confirmation always involves two parties. It does
not, and a design document that said so would be describing something nobody would ship.

Group membership (`getgrouplist`) would be the idiomatic Unix expression of a confirmer set
and is deliberately not built here: `PeerIdentity` carries a uid and a gid, not a group list,
and widening it is its own change. Explicit subjects first.

### 4. What this does not achieve, stated before it is built

**It cannot prove a human.** It proves a *different uid on a different connection*. Nothing
available to a Unix daemon distinguishes a person at a keyboard from a program running as
another user, and any claim otherwise would be false.

**And on a node where the agent runs as root, it is theatre.** Root can connect to any
socket, read any key, and edit the policy. The current image runs everything as `uid:0` and
its shipped policy grants to `subjects = ["uid:0"]`, so shipping this channel onto that image
today would add a ceremony that stops nothing.

So this ADR imposes a prerequisite on another piece of work rather than pretending to be
complete: **the agent must run under its own uid, distinct from the confirming user's,
before any confirmation on this channel means anything.** Until then the mechanism is
correct, tested, and inert. That is worth building in this order — the alternative is to
discover the uid split is needed while also debugging a new protocol — but it must not be
described as "confirmation works" in the meantime.

### 5. Pending confirmations expire, and expiry is a denial

A pending record has a lifetime (default 300 s). Expiry is not a soft state: a claim against
an expired record is `forbidden`, and the record is kept, marked expired, in the audit log.

Nothing may sit pending indefinitely. A queue of stale requests is both a memory cost a T0
board should not carry and, worse, a trap: an approval given hours later would authorise an
action whose context has gone. The person who says yes should be saying yes to something
that is still happening.

The number of simultaneously pending confirmations is bounded, and a request that would
exceed the bound is refused rather than queued — an unbounded pending list is a way for a
caller to spend a small board's memory by asking for things nobody will answer.

### 6. The confirmer is shown enough to decide, and no more

`confirm.list` returns, per pending record: the id, the subject, the action, its registered
summary and blast radius, the resource, the caller's stated reason if any, and the age.

The blast radius is included because "irreversible" is the word that changes an answer. The
caller's reason is shown as **the caller's claim**, never as fact — it is a string chosen by
the thing asking for the permission, which under `SECURITY-MODEL.md` §3 may be an agent
acting on untrusted content. A UI that renders it as an explanation rather than as an
assertion has been enlisted by whatever wrote it.

What is *not* included: any payload. A confirmation names an action and a resource; it does
not hand the confirming socket the bytes about to be signed or deleted.

### 7. Every step is audited, including the refusals

Pending creation, approval (with the confirmer's uid), refusal for self-confirmation,
denial, expiry, and the eventual token issue. The audit chain already exists and is
hash-chained; a confirmation flow that left no trace of *who* approved would remove the only
evidence that the human step happened at all.

## Consequences

**Good.** The eight categories in `SECURITY-MODEL.md` §2 become expressible. Nothing blocks,
so an unanswered confirmation costs a caller nothing and a node with nobody home behaves
exactly as it does now. The approval is scoped so tightly that it cannot be mistaken for a
policy change. The wallet — and `fs.delete` and `net.egress` — stop being unreachable by
construction.

**Bad, and worth naming.**

- **It is inert until the agent has its own uid** (§4), and the temptation to describe it as
  finished before then will be strong.
- **A second socket is a second attack surface** on the most security-critical daemon, added
  for a feature that daemon did not otherwise need.
- **Two round trips** where there was one, plus a claim the caller must remember to make.
  Callers that forget will look like they were denied.
- **It cannot prove a human**, and no version of it will.
- **A person who approves quickly and often stops reading.** This design cannot fix
  confirmation fatigue; it can only avoid causing it, which is why §2 refuses to let one
  approval widen and why the action registry keeps `always_confirm` rare.

## Alternatives rejected

- **Block `perm.request` until answered.** One round trip, no ids, no claim step. Parks a
  thread in a privileged daemon on human latency and hands a caller a denial of service. §1.
- **Approve on the same control-plane socket.** No new socket, no new surface. The socket
  every daemon must reach cannot be the socket only a person may reach, so the
  self-confirmation rule would have nothing to stand on. §3.
- **Let an approval grant the action for a while** ("yes, for the next ten minutes"). Fewer
  prompts, and it converts a confirmation into a temporary policy change the person did not
  read as one. §2.
- **Prove a human with a TTY check or a physical-presence signal.** Neither is available on
  every target, both are forgeable by a process that can allocate a pty, and either would
  license the claim "a human confirmed" that §4 refuses to make.
- **Ship it before the uid split and grant to `uid:0` anyway**, so the wallet works sooner.
  That is a ceremony that stops nothing, described as a security control. It is the single
  worst option here, because it would make the system *look* protected.
- **Queue pending confirmations without expiry**, so nothing is lost while a person is away.
  Unbounded memory on a T0 board, and an approval given hours later authorises something
  whose context has gone. §5.

## What is deliberately not decided

- **The agent's uid split** — required by §4, and its own piece of work.
- **How a person is reached** (console, companion client per `PORTABLE-APPS.md`, a desktop
  surface). This ADR defines the channel; the notification is a separate design and may be
  several.
- **Whether some actions should require re-entering a passphrase** rather than an approval
  click. ADR-0022 §3 already requires it for `wallet.export_seed`; making that general is not
  answered here.
- **Rate limiting approvals**, and what to do about confirmation fatigue beyond keeping
  `always_confirm` rare.

## References

- `docs/security/SECURITY-MODEL.md` §2 (mandatory user confirmation, the audit log),
  §3 (agent-specific threats — why a caller's stated reason is a claim, not a fact).
- ADR-0022 §3 and ADR-0023 §4 (the wallet actions this unblocks), ADR-0014 (why `net.fetch`
  is deliberately not one of them).
- `crates/otwono-permd/src/policy.rs` (the `Allow` → `Ask` conversion),
  `crates/otwono-permd/src/service.rs` (the `confirmation_required` this replaces),
  `crates/otwono-proto/src/server.rs` — `PeerIdentity`, the `SO_PEERCRED` uid §3 rests on.

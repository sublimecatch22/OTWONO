# ADR-0010 — Split the node's two keys across two daemons

**Status:** accepted · **Date:** 2026-08-22

## Context

ADR-0006 gave each node two keys: a long-term **Ed25519** signing key, whose hash *is* the
NodeID, and a separate **X25519** agreement key for Noise. Phase 3 stored both seeds in one
file, `node.key`, and both `otwono-idd` and `otwono-netd` opened it.

That put the node's signing key inside `otwono-netd` — the daemon that parses input from
the network, the Z3 process in `docs/security/SECURITY-MODEL.md`. The two keys have very
different consequences on loss:

| Key | If it leaks |
|---|---|
| X25519 agreement | Current sessions are compromised. Generate a new one, re-bind it, carry on. |
| Ed25519 signing | The node's **name** is compromised, permanently. A NodeID cannot be re-earned, only succeeded, and every peer that trusted the old one has to be told. |

Holding the recoverable key and the unrecoverable one in the same process gives the whole
node the risk profile of its most exposed component. `otwono-netd/src/lib.rs` carried this
as a documented deviation from the intended design since Phase 3.

The obvious fix — move the handshake into `otwono-idd` — does not work: `snow` drives the
key agreement itself and needs the X25519 secret in the process holding the socket.

## Decision

**Split the keystore by holder, and broker the two signatures the handshake needs.**

```text
/var/lib/otwono/identity/
  node.key       0600  Ed25519 seed + the agreement PUBLIC key it vouches for   otwono-idd
  agreement.key  0600  X25519 secret                                            otwono-netd
```

- `otwono-idd` holds `SigningIdentity`. It is the only process that opens `node.key`.
- `otwono-netd` holds `AgreementKey`. It has no code path that opens `node.key`.
- The seam is the `SessionSigner` trait in `otwono-identity`. `otwono-net` handshakes
  against the trait, so it does not care where the signing key is.
- At startup `otwono-netd` calls **`id.bind_agreement`**, handing over its agreement
  *public* key and taking back a signed `AgreementBinding`. This is also how the node's
  name reaches `otwono-netd`: it cannot derive a NodeID, because it cannot see the key a
  NodeID is the hash of.
- Per handshake, `otwono-netd` calls **`id.sign_session`** with the Noise handshake hash.

Both calls are brokered by `otwono-permd` against new registered actions
(`id.bind_agreement`, `id.sign_session`), so they are policy-gated and audited like every
other privileged operation.

`NodeIdentity` — both halves in one process — is kept, but no daemon uses it. It is what
tests and single-process tools hold, and it implements `SessionSigner` by signing locally.

## Why the two halves are genuinely both needed

`id.sign_session` is a signing oracle, so the split only means something if holding it is
insufficient. It is:

- A caller with `id.sign_session` but **no agreement secret** cannot complete a handshake.
  The binding it would present names an X25519 key it does not hold, and the responder
  rejects the proof with `BindingDoesNotMatchHandshake`.
- A caller with the **agreement secret but no `id.sign_session`** cannot complete one
  either. It cannot sign the current handshake hash, and a proof from any other key fails
  with `StaleOrForgedSessionProof`.

The oracle is bounded on purpose: `id.sign_session` signs exactly 32 bytes under a fixed
domain prefix. A caller cannot steer it into signing anything of its choosing.

## Consequences

**Good:** compromising the network-facing daemon costs the node its sessions and its
agreement key — both replaceable — and not its name. Key use is concentrated in one small
daemon, which is what `docs/security/SECURITY-MODEL.md` always claimed. Both handshake
capabilities are now visible in policy and in the audit log, so an operator can see how
often the node authenticates and can revoke the ability without stopping the daemon.

**Bad, and accepted:**

- **A handshake now depends on two more processes.** `otwono-idd` and `otwono-permd` must
  both be up. This is fail-closed — a node that cannot prove who it is must not pretend —
  but it is a real new failure mode. `otwono-netd` gains `Requires=` on both.
- **One control-plane round trip per handshake.** Local `AF_UNIX`, and handshakes are rare
  compared to traffic; the cost is not on the data path. If it ever matters, the fix is to
  batch or pre-sign, not to move the key back.
- **A capability token to manage.** `otwono-netd` caches its `id.sign_session` token and
  re-requests once on expiry.
- **Rotation now has a second step.** A new signing key has vouched for nothing, so
  `id.rotate` clears the binding and reports `agreement_rebind_required`. Until
  `otwono-netd` re-binds, the node cannot handshake. That is correct — presenting a binding
  the current key never made would be a lie — but it is a sharper edge than before.
- **`id.node` can now fail.** A node whose agreement key has never been bound has nothing
  publishable: `PublicIdentity` carries an agreement key, and inventing one would be a lie.
  `id.node` returns `unavailable` in that window. `id.fingerprint` still answers, so the
  node's name is always askable — the two questions are now genuinely different.
- **Still one uid.** Both daemons run as root, so the separation is by process and code
  path, not yet by kernel-enforced access control. `node.key` at 0600 stops another *user*,
  not another root process. Finishing this needs the Z2/Z3 user separation and Landlock
  work, which is not done. Stated plainly here so the current guarantee is not overstated.

## Migration

A pre-split `node.key` holding both seeds is split in place by `otwono-idd` at startup,
preserving the existing agreement key so the node's published `node.pub` does not change.
The migration is idempotent and refuses to overwrite an `agreement.key` that already
exists.

## Alternatives rejected

- **Move the whole handshake into `otwono-idd`.** Would put socket handling and hostile
  frame parsing inside the process holding the node key — the opposite of the goal.
- **Derive X25519 from the Ed25519 seed** (the birational map). One fewer file, but it
  re-couples the two keys: the agreement key could not be rotated independently, which is
  the property ADR-0006 kept them separate for.
- **Ship the signing key to `otwono-netd` over the socket.** Solves nothing; the key ends
  up in the exposed process anyway.
- **Leave it as one file and rely on future uid separation.** Kernel enforcement is the
  right endpoint, but it does not remove the key from the process's address space. A
  memory-disclosure bug in `otwono-netd` would still leak the node's name.

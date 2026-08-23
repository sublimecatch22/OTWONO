# ADR-0012 — Confine the inference engine with Landlock, applied by the adapter to itself

**Status:** accepted · **Date:** 2026-08-23

## Context

ADR-0011 put llama.cpp behind an adapter process. That fixed the blast radius of a *crash*
— the daemon survives one — but not the blast radius of a *compromise*. `llama-server` is a
large C++ program whose entire job is parsing binary files that came from somewhere else,
and it runs in the adapter's process tree with the adapter's privileges. Today the only
model on any OTWONO node is one we generated. `ai.models.pull` will change that, and it is
the wrong order to build the intake path before the containment.

What is at stake is specific, not abstract. The daemon's tree can reach
`/var/lib/otwono/identity` — the node's Ed25519 private key — plus the audit log and the
policy store. A malicious GGUF that achieves code execution inside the engine currently
gets all of it.

The candidates were Landlock, bubblewrap, and a separate systemd unit per engine.

## Decision

**The adapter restricts itself with Landlock at startup, before it ever spawns an engine.**
Landlock is inherited by descendants and cannot be undone, so the engine is confined by
construction rather than by remembering to confine it.

The policy is built from the three paths the adapter is told about — the engine binary's
directory, a model directory, and the runtime directory — plus the read-only system
directories any dynamically linked program needs. The runtime directory is the only
writable path. `--model-dir` is required rather than defaulted, because it is the boundary
of what the engine may ever read and a default would be a boundary nobody chose.

**It fails closed.** On a kernel that does not enforce Landlock the adapter refuses to
start; `--allow-unconfined` overrides that, explicitly and logged on every start.

### Why the adapter restricts *itself*, rather than the child

The obvious place is between fork and exec, which in Rust is `Command::pre_exec` — and
that is `unsafe`. Adding `unsafe` to the process that handles untrusted model files, in
order to make it safer, is a poor trade. Restricting at startup avoids it, and confines the
adapter too, which is strictly better: the adapter has no more business reading the node's
private key than the engine does.

The cost is that the policy must be known before any `backend.load` names a model. Hence a
model *directory* rather than arbitrary paths.

### Why Landlock and not bubblewrap

| | Landlock | bubblewrap |
|---|---|---|
| New package in a minimal image | none | `bubblewrap` |
| Applied from safe Rust | yes, one syscall | via exec, needs the binary present |
| Process tree | unchanged — ADR-0011's shape survives | inserts another process |
| Coverage | filesystem (and network on newer ABIs) | full namespaces: PID, mount, IPC |

bubblewrap confines more. Landlock confines the thing that matters here — access to files —
without adding a dependency to the base image or a fourth process to the chain, and it
composes with the systemd hardening already on `otwono-aid.service` rather than fighting
it. A separate systemd unit per engine was rejected outright: it would take ownership of
the engine process away from the adapter that supervises it, undoing ADR-0011.

## Consequences

**Good.** A compromised engine cannot read the node identity key, the audit log, the policy
store, or the user's files. The model store is readable and *not* writable, so an engine
cannot replace a verified blob with its own for the next load. Nothing is added to the
image. The adapter refuses a model outside the store before starting anything, so the
common misconfiguration produces a sentence instead of a permission error from inside C++.

**Bad, and worth naming.** `/proc` and `/sys` are readable, because ggml's CPU detection
needs them; procfs exposes a great deal and this is a real widening. There is no PID or
mount namespace and no seccomp filter, so a compromised engine can still exhaust CPU and
see what any process can see. Landlock governs new opens, not descriptors already open at
the time it is applied. And fail-closed means a node on a kernel without Landlock loses
local inference until an operator opts out deliberately — which is the intended trade, but
it is a real one.

**Untested where it was written.** The OTWONO dev environment's kernel returns `ENOSYS` for
`landlock_create_ruleset`, so enforcement cannot be demonstrated there. The policy is unit
tested and the *kernel* behaviour is verified on a booted image, whose kernel is its own.
See `docs/build/VERIFICATION-LOG.md`.

## Alternatives rejected

- **`Command::pre_exec` to confine the child** — `unsafe` in the one process that most
  needs to be boring.
- **bubblewrap** — more coverage, at the cost of a base-image dependency and a fourth
  process; revisit if PID and mount isolation become necessary.
- **A systemd unit per engine** — breaks the supervision chain ADR-0011 established.
- **Nothing, and rely on `otwono-aid.service`'s hardening** — that hardening is written for
  a daemon that reads a catalog, and it does not stop the engine reading the node key.

# ADR-0011 — Drive llama.cpp as a supervised adapter process, not a linked library

**Status:** accepted · **Date:** 2026-08-23

## Context

ADR-0005 committed to integrating existing inference engines rather than writing one. This
decides *how* the first of them, llama.cpp, is actually attached — a question with several
plausible answers and materially different consequences.

llama.cpp offers three ways in:

1. `libllama` via FFI, linked into a Rust process.
2. `llama-cli`, driven as a subprocess through its interactive text mode.
3. `llama-server`, an HTTP service that already does model loading, KV-cache reuse across
   requests, slot management and sampling.

And two places the translation could live: inside `otwono-aid`, or in a separate process.

## Decision

**A separate adapter binary, `otwono-llama-backend`, drives `llama-server` over a Unix
socket.** Three processes:

```
otwono-aid  ──NDJSON JSON-RPC on stdio──▶  otwono-llama-backend  ──HTTP over a
 (daemon)      (otwono_ai::supervisor)         (otwono-llama)        Unix socket──▶  llama-server
```

Specifically:

- **`llama-server`, not `libllama` and not `llama-cli`.** FFI would put `unsafe` in a
  privileged daemon and make `cargo test --workspace` depend on a C++ toolchain and an
  engine build on every machine — the whole workspace behind the slowest dependency in it.
  `llama-cli` would mean parsing an interface designed for humans. `llama-server` is the
  interface upstream maintains for programs, and it already solves KV-cache reuse, which is
  most of the difference between a usable and an unusable assistant on small hardware.
- **A Unix socket, not a loopback TCP port.** A port on `127.0.0.1` is reachable by every
  local user; on a shared machine that would let any account drive the inference engine,
  read what is in flight, and inject prompts. A socket in a `0700` directory is protected
  by the filesystem, which is the boundary the rest of the control plane already uses.
- **An adapter process, not HTTP inside the daemon.** llama.cpp is one backend of several,
  and the others look nothing like it: whisper.cpp has no server, Piper reads text on
  stdin, ONNX Runtime is a library. The point of the supervisor protocol is that
  `otwono-aid` learns exactly one dialect. Put llama.cpp's HTTP in the daemon and the
  daemon grows a second dialect for the next backend and a third after that.
- **Availability is discovered on disk, not decided at compile time.** A backend exists
  when its adapter and its engine are both present and executable under
  `/usr/libexec/otwono/ai-backends` and `/usr/lib/otwono/ai`. One OTWONO build therefore
  serves a CPU-only Pi and a CUDA workstation, and `ai.capabilities` describes the machine
  rather than the build.
- **The engine does not get its own process group.** It stays in the adapter's, so the
  supervisor's group kill reaches it.

## Consequences

**Good.** The daemon links no engine and keeps answering `ai.capabilities` when a model
load segfaults. The workspace builds and tests with no C++ toolchain, no engine and no
model anywhere on the machine. Engine upgrades are a file swap. A GPU build is a second
directory, not a second daemon. And the awkward paths — a hung engine, a corrupt model, a
killed adapter — are ordinary tests, because they happen at a process boundary we control.

**Bad.** Three processes to reason about instead of one, and two hops of serialization per
request: JSON to the adapter, JSON again to the engine. On a 12-token completion that
overhead is visible in a profile; against a real model's generation time it is noise, and
the KV-cache reuse `llama-server` provides swamps it in the other direction. We also
inherit upstream's HTTP response shape, which changes — `stop_type` replaced three boolean
fields between releases, and the adapter now reads both.

**Also bad, and worth naming.** The engine is a large C++ program parsing untrusted model
files, and it runs with the adapter's privileges. Nothing *in this ADR* sandboxes it beyond
what `otwono-aid.service` already imposes on the tree. That gap is closed by ADR-0012,
which has the adapter confine itself with Landlock before starting an engine.

## Alternatives rejected

- **`libllama` via FFI** (`llama-cpp-2` and similar) — `unsafe` in a privileged daemon, a
  C++ build dependency for the entire workspace, and a segfault in a model loader taking
  the control plane with it.
- **`llama-cli` as a subprocess** — parsing an interface intended for a terminal, with no
  stable contract and no KV-cache reuse between requests.
- **HTTP from `otwono-aid` directly to `llama-server`** — makes the daemon learn one
  dialect per backend, and puts engine lifecycle management in the process that must stay
  up when the engine dies.
- **A loopback TCP port** — trades a real security boundary on multi-user machines for the
  convenience of an off-the-shelf HTTP client.
- **Vendoring llama.cpp into the Cargo build** — ties our release cadence to theirs and
  makes every `cargo build` a C++ build.

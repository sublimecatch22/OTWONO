# ADR-0001 — Rust for privileged system daemons

**Status:** accepted · **Date:** 2026-08-22

## Context

The privileged daemons parse hostile input (`otwono-netd`), hold key material
(`otwono-idd`), and make security decisions (`otwono-permd`). They must cross-compile
cleanly to arm64, run in a few megabytes on an SBC, and be auditable.

Candidates: C, C++, Go, Rust, Python.

## Decision

**Rust (stable) for all privileged daemons and all code that parses network input.**
`#![forbid(unsafe_code)]` by default; any exception needs an ADR and a `// SAFETY:` comment.

Python is permitted for agent-layer glue where a library clearly dominates, never in a
privileged daemon. Shell is permitted for the build system.

## Consequences

**Good:** memory safety exactly where a bug is a remote code execution; excellent
cross-compilation (`aarch64-unknown-linux-{gnu,musl}` confirmed working in this
environment); static musl binaries with no runtime dependency, which matters on a minimal
SBC image; mature `rust-libp2p`, `snow`, `ed25519-dalek`, `blake3`.

**Bad:** slower compilation than Go, especially on an SBC; a steeper contributor ramp;
some vendor NPU SDKs are C/C++ only and will need FFI shims — those shims are the one place
`unsafe` is expected, and they must be isolated in dedicated crates.

## Alternatives rejected

- **Go** — attractive for `libp2p-go` and fast builds, but a GC and a multi-megabyte
  runtime in every daemon is a poor fit for T0/T1, and it lacks Rust's compile-time
  guarantees at the hostile-input boundary. Rust's libp2p is equally mature.
- **C/C++** — the memory-safety risk in exactly the wrong place. Rejected.
- **Python** — unacceptable startup cost and memory footprint for a boot-critical daemon on
  an SBC, and no static distribution story.

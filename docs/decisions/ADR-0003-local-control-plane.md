# ADR-0003 — JSON-RPC 2.0 over Unix sockets as the Local Control Plane

**Status:** accepted · **Date:** 2026-08-22

## Context

Seven or more daemons plus an agent layer plus CLIs need one IPC mechanism. It must be
language-neutral, testable without booting the OS, cheap on a T0 board, and debuggable by a
human at 2 a.m.

## Decision

**JSON-RPC 2.0, newline-delimited, over Unix domain sockets** at `/run/otwono/<service>.sock`.
Peer identity from `SO_PEERCRED`; authorization from a capability token in `params._cap`.
Every service implements an unauthenticated `describe`. Schemas in `schemas/` are the
contract, versioned with `schema_version`.

## Consequences

**Good:** any language can participate; a test can point a client at a temp directory and
run the whole control plane without root or a VM; `socat` is a debugger; no D-Bus
dependency in a minimal headless image; `SO_PEERCRED` gives real caller identity for free.

**Bad:** JSON is verbose and slower to parse than a binary codec — irrelevant at control-plane
message rates, but it means bulk data (model weights, media blobs) must move over separate
channels (fd passing or a side stream), which is an explicit design constraint, not an
oversight. No built-in schema enforcement, so schema validation must be a test.

## Alternatives rejected

- **D-Bus** — ubiquitous on the desktop, but a heavy dependency for a headless SBC image,
  awkward to test, and its type system fights JSON Schema. A D-Bus *bridge* for desktop
  integration remains allowed; it is never the source of truth.
- **gRPC/protobuf** — good performance and codegen, but heavyweight for a small system, and
  it makes shell-based debugging and polyglot scripting materially harder.
- **Bespoke binary protocol** — no. We would spend the project's early velocity on framing
  bugs.

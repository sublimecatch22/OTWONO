# ADR-0007 — Fail-closed data visibility labels with provenance propagation

**Status:** accepted · **Date:** 2026-08-22

## Context

An AI agent with filesystem access, on a node that replicates content to peers, is an
exfiltration engine unless data exposure is a structural property rather than a matter of
the agent behaving well.

## Decision

Four labels — `PRIVATE`, `SHARED`, `PUBLIC`, `REPLICATED` — on every stored object,
enforced in `otwono-stored` **and** independently at the `otwono-netd` egress path.

- Default and fallback is `PRIVATE`; unparseable means `PRIVATE`. Fail closed.
- Promotion is an explicit, logged user action. The agent may propose, never promote.
- **Provenance propagates:** derived content inherits the most restrictive input label.
- Prompts sent to remote inference are egress and are subject to labels.

## Consequences

**Good:** exfiltration requires defeating two independent enforcement points; the model is
simple enough that a user can hold it in their head; provenance propagation closes the
"summarize the private file, then post the summary" laundering path, which is the realistic
attack rather than a theoretical one.

**Bad:** provenance tracking must be threaded through every derivation path or it silently
under-labels — this needs property tests, not spot checks; over-restrictive inheritance will
annoy users (a summary of a private doc being private may surprise them), so the UI must
make promotion easy *and* explicit; four labels cannot express every real-world policy, and
resisting a fifth will take discipline.

## Alternatives rejected

- **POSIX permissions alone** — no network semantics, no provenance, no replication policy.
- **Agent-side policy only** — a prompt injection defeats it entirely.
- **A full MAC/lattice model (SELinux-style)** — far more expressive, far beyond what a user
  can reason about, and historically ends up disabled.

# ADR-0004 — Capability vector with tier composition, not a single score

**Status:** accepted · **Date:** 2026-08-22

## Context

Feature availability must adapt from a Pi Zero to a dual-GPU workstation. The naive design
is a single benchmark score mapped to a tier. That design fails on real hardware: a Pi 5
with 16 GB and no GPU, and a 4-core laptop with an 8 GB RTX 4060, are not comparable on one
axis, and a scalar hides exactly the bottleneck that will break the system.

## Decision

`otwono-hal` produces a **capability vector** across six independent axes — `compute`,
`memory`, `accelerator`, `storage`, `network`, `power`. `otwono-capability` classifies each
axis, then composes an overall tier as **the highest tier whose every requirement is met**
(the weakest binding axis wins). Both the vector and the tier are published in the profile.
Subsystems are encouraged to read the axis they actually care about rather than the tier.

Probes are **injectable**: every probe reads from a root path, so a fixture directory
substitutes for `/`.

## Consequences

**Good:** bottlenecks are explicit and explainable to the user ("T2, limited by accelerator:
none"); tiering is unit-testable on hardware you do not own; new axes can be added without
invalidating existing rules; overrides are natural.

**Bad:** more thresholds to maintain and defend; the composed tier is still coarse for
unusual machines (documented in `docs/hardware/CAPABILITY-TIERS.md` §4), which is why the
vector is published alongside it.

## Alternatives rejected

- **Single benchmark score** — hides bottlenecks, unstable across kernels and thermal
  states, and impossible to explain to a user.
- **Runtime probing only ("just try it and see")** — the failure mode is the OOM killer
  during the user's first interaction. Unacceptable as a first impression, and worse on a
  headless node.
- **Per-subsystem ad-hoc checks** — guarantees inconsistency, and there would be no single
  place to override.

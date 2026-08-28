# ADR-0002 — Debian-derived base with a staged image builder

**Status:** accepted · **Date:** 2026-08-22

## Context

OTWONO must run mature desktop applications (LibreOffice, GIMP, Kdenlive, Firefox) on both
amd64 and arm64, boot on SBCs, and be reproducible enough to audit. The base choice
constrains everything downstream and is expensive to reverse.

## Decision

**Debian as the canonical base**, assembled by numbered, idempotent stages driven by a
declarative recipe. Suite, mirror, and architecture are recipe parameters, never hardcoded.
An `ubuntu-noble` recipe is maintained alongside.

## Consequences

**Good:** first-class amd64 and arm64; an enormous, maintained application set we do not
have to package; `snapshot.debian.org` gives time-pinned reproducible sources; well-trodden
`debootstrap` → customize → image pipeline; no vendor lock-in.

**Bad:** we inherit Debian's release cadence and, on the stable train, older application
versions — mitigated with Flatpak for user applications; the base image is larger than a
Buildroot appliance; glibc rather than musl.

**Environmental note:** the current OTWONO Cloud dev environment's egress proxy rejects
Debian mirrors (403 on CONNECT) and permits `archive.ubuntu.com` / `ports.ubuntu.com`. This
is a property of the sandbox, not of the architecture, and is precisely why the mirror is a
parameter. CI in this environment exercises the Ubuntu path; both must keep working.

## Alternatives rejected

- **Yocto/OpenEmbedded** — the right answer for a fixed-function embedded product, the wrong
  answer when we must ship a desktop application stack on two architectures. Maintaining
  LibreOffice and Kdenlive recipes is a multi-person-year commitment.
- **Buildroot** — no on-device package manager, no desktop story.
- **NixOS** — genuinely tempting for reproducibility and atomic updates. Rejected for 0.x
  because ARM SBC enablement (vendor kernels, device trees, non-mainline BSPs) is
  substantially harder there and it commits the whole project to one paradigm before we
  have learned anything. Revisit at 1.x — see OQ-2.
- **Alpine** — musl breaks too much GPU/NPU vendor tooling.
- **Arch** — no stable release train suitable for an appliance-style A/B update model.

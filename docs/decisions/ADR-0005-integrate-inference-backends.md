# ADR-0005 — Integrate inference backends; never write an engine

**Status:** accepted · **Date:** 2026-08-22

## Context

OTWONO needs local inference across CPU (x86 and ARM), Vulkan, CUDA, ROCm, and several
vendor NPUs. Writing an inference engine is a multi-year, full-time effort that would
consume the entire project.

## Decision

`otwono-aid` defines a backend interface and **integrates existing engines**: llama.cpp
(CPU/Vulkan/CUDA/ROCm), ONNX Runtime with vendor execution providers (RKNN, Hailo, Coral,
Intel NPU, AMD XDNA), whisper.cpp, Piper, vLLM at T4, and remote peers over ONM. Backends
run out of process and are supervised.

## Consequences

**Good:** we inherit years of kernel optimization and hardware support for free; new
hardware arrives as a new backend rather than a rewrite; a backend crash is a typed error,
not a dead daemon.

**Bad:** we inherit upstream bugs and release cadences; packaging several native backends
for two architectures is real work; the abstraction must be wide enough to be useful and
narrow enough to implement — the lowest-common-denominator risk is real and is mitigated by
allowing backend-specific options to pass through a typed escape hatch.

## Alternatives rejected

- **Write our own engine** — the project would become an inference-engine project.
- **Support exactly one backend** — llama.cpp alone would exclude NPUs entirely, which
  removes the whole point on Rockchip-class SBCs.
- **Always call a cloud API** — violates the local-first premise.

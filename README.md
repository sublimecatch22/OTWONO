# OTWONO AI OS

An AI-native, hardware-adaptive Linux distribution with decentralized peer-to-peer
networking, local AI, distributed services, and offline-first capabilities.

Runs on **x86_64/AMD64** computers and **ARM64** single-board computers. Same operating
system, same subsystems, same interfaces — the capabilities scale to the hardware.

> **Project status: Phase 0 — foundation.** The architecture, the build skeleton, and
> hardware detection exist and are tested. There is no bootable image yet. Every document
> and subsystem carries an explicit status (`SPECIFIED` / `IMPLEMENTED` / `VERIFIED`), and
> nothing here claims to work that has not been run. See
> [`docs/roadmap/ROADMAP.md`](docs/roadmap/ROADMAP.md).

---

## What it is

Three properties are non-negotiable:

1. **Local-first.** The machine in front of you is the primary computer. Cloud is optional.
2. **Works offline.** Every subsystem degrades to something useful with no Internet.
3. **You own the keys.** Identity, data, and models live on your disk, under explicit
   visibility labels.

Four things make it different from a desktop Linux with a chatbot bolted on:

| | |
|---|---|
| **Hardware-adaptive** | One capability profile decides what this machine may do. A Pi Zero gets a command-grammar assistant and a relay node; a workstation gets 70B-class models, multiple agents, and serves AI to authorized peers. Nothing else in the system re-derives "is this machine big enough". |
| **AI as an OS subsystem** | The assistant drives ordinary open-source applications — LibreOffice, GIMP, ffmpeg, Kdenlive — through a typed, permission-brokered adapter layer. It has no ambient privilege, ever. |
| **Decentralized by default** | Every installation has a cryptographic node identity and can join a peer-to-peer overlay over Ethernet, Wi-Fi, or radio. Wiki, forums, messaging, profiles, and media sharing keep working when the Internet does not. |
| **Data has a policy** | Every object is `PRIVATE`, `SHARED`, `PUBLIC`, or `REPLICATED`. The default is `PRIVATE`, unparseable means `PRIVATE`, and derived content inherits the most restrictive label of its inputs. Enforced in the store *and* independently at network egress. |

## Documentation

| Area | Start here |
|---|---|
| Architecture | [`docs/architecture/OTWONO-ARCHITECTURE.md`](docs/architecture/OTWONO-ARCHITECTURE.md) |
| Roadmap | [`docs/roadmap/ROADMAP.md`](docs/roadmap/ROADMAP.md) |
| Hardware tiers | [`docs/hardware/CAPABILITY-TIERS.md`](docs/hardware/CAPABILITY-TIERS.md) |
| Node network | [`docs/network/NODE-NETWORK.md`](docs/network/NODE-NETWORK.md) · [node identity](docs/network/NODE-IDENTITY.md) |
| AI runtime | [`docs/ai/AI-RUNTIME.md`](docs/ai/AI-RUNTIME.md) · [app integration](docs/ai/APP-INTEGRATION.md) |
| Security | [`docs/security/SECURITY-MODEL.md`](docs/security/SECURITY-MODEL.md) · [data visibility](docs/security/DATA-VISIBILITY.md) · [threat model](docs/security/THREAT-MODEL.md) |
| Build | [`docs/build/BUILD-SYSTEM.md`](docs/build/BUILD-SYSTEM.md) · [updates](docs/build/UPDATE-ARCHITECTURE.md) |
| Services | [`docs/services/DISTRIBUTED-SERVICES.md`](docs/services/DISTRIBUTED-SERVICES.md) |
| Decisions | [`docs/decisions/`](docs/decisions/) — ADRs and [open questions](docs/decisions/OPEN-QUESTIONS.md) |
| Engineering rules | [`CLAUDE.md`](CLAUDE.md) — normative for every contributor |

## Quick start

```bash
# What can this development host actually do?
tools/probe-env.sh

# Build and test the Rust workspace
cargo test --workspace

# What tier is this machine, and why?
cargo run -p otwono-hwctl -- profile

# The machine-readable contract other subsystems consume
cargo run -p otwono-hwctl -- profile --json

# Classify a captured fixture instead of the live machine
cargo run -p otwono-hwctl -- profile --root crates/otwono-hal/tests/fixtures/aarch64-rpi5-8gb-synthetic

# Capture a fixture from real hardware and commit it
tools/capture-hw-fixture.sh crates/otwono-hal/tests/fixtures/my-board --label "Orange Pi 5 16GB"
```

Image builds:

```bash
make -C build list-targets
make -C build TARGET=amd64-qemu-ubuntu rootfs      # stages 05..30 (works today)
make -C build TARGET=arm64-qemu-ubuntu rootfs      # cross-bootstrap via binfmt + qemu-user
make -C build TARGET=amd64-qemu image              # stages 40..50 — Phase 1, not implemented
```

## Repository layout

```
CLAUDE.md      permanent engineering instructions (normative)
docs/          architecture, network, ai, security, hardware, build, services, roadmap, decisions
crates/        Rust workspace — otwono-hal, otwono-capability, otwono-hwctl
schemas/       JSON Schemas — the cross-language contracts
build/         Makefile, recipes, numbered stages, QEMU harnesses
tools/         environment probe, hardware fixture capture
tests/         cross-component and contract tests
```

## Contributing

Read [`CLAUDE.md`](CLAUDE.md) first. Two rules matter more than the rest:

- **Do not claim untested functionality works.** Every claim of function must trace to a
  test run, a build log, or a boot log.
- **Integrate mature open source; do not rewrite it.** Replacing an existing project
  requires an ADR with a technical justification.

## Licence

Apache-2.0.

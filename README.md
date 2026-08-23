# OTWONO AI OS

An AI-native, hardware-adaptive Linux distribution with decentralized peer-to-peer
networking, local AI, distributed services, and offline-first capabilities.

Runs on **x86_64/AMD64** computers and **ARM64** single-board computers. Same operating
system, same subsystems, same interfaces — the capabilities scale to the hardware.

> **Project status: Phase 3 complete; Phase 4 started.** Both images boot under QEMU, and the
> **permission broker and hardware daemon now run on both booted systems**: at first boot the
> guest requests a capability, calls the hardware daemon with it, and the broker writes a
> hash-chained audit record — all recovered from the image and verified on the host
> afterwards. **Two nodes form a mesh**: booted from one image onto a segment with no
> DHCP server, they discover each other over mDNS and each authenticates the other's
> cryptographic NodeID — and since ADR-0010 the daemon that talks to the network no longer
> holds the key its NodeID names. Nothing has run on real hardware yet; there is no radio
> link and no routing. **Local inference now works**: llama.cpp is integrated as a
> supervised adapter process (ADR-0011), and a prompt travels from a control-plane client
> through the permission broker, admission control, the supervisor and `llama-server` into
> a GGUF model and back as generated tokens. It is off by default — the engine is 17 MiB of
> third-party C++ per architecture, so images opt in with `AI_ENGINE=llama.cpp` — and there
> is no streaming, no sandbox around the engine, and no model ships with the image.
> The daemons still run as root pending user separation.
>
> Every document and subsystem carries an explicit status (`SPECIFIED` / `IMPLEMENTED` /
> `VERIFIED`), and nothing here claims to work that has not been run — see
> [`docs/build/VERIFICATION-LOG.md`](docs/build/VERIFICATION-LOG.md) for the evidence and
> [`docs/roadmap/ROADMAP.md`](docs/roadmap/ROADMAP.md) for what comes next.

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
| **AI as an OS subsystem** | The assistant drives ordinary open-source applications — LibreOffice, GIMP, ffmpeg, Kdenlive — through a typed, permission-brokered adapter layer. It has no ambient privilege, ever. The broker exists today: every privileged call needs a scoped, time-limited capability token, and a policy saying `allow` on a destructive action still yields "confirmation required". |
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

# Ask the running daemons instead of probing locally
cargo run -p otwono-hwctl -- remote

# Validate a policy file, or check an audit log's hash chain
cargo run -p otwono-permd -- --check --policy-dir /etc/otwono/policy.d
cargo run -p otwono-permd -- --verify-audit /var/log/otwono/audit.jsonl

# The machine-readable contract other subsystems consume
cargo run -p otwono-hwctl -- profile --json

# Classify a captured fixture instead of the live machine
cargo run -p otwono-hwctl -- profile --root crates/otwono-hal/tests/fixtures/aarch64-rpi5-8gb-synthetic

# Capture a fixture from real hardware and commit it
tools/capture-hw-fixture.sh crates/otwono-hal/tests/fixtures/my-board --label "Orange Pi 5 16GB"
```

Image builds:

```bash
tools/probe-env.sh                                 # can this host build an image at all?
make -C build list-targets

# Build and boot an amd64 image. Verified: reaches a login prompt under QEMU and
# recovers a capability profile from inside the VM.
make -C build TARGET=amd64-qemu-ubuntu boot-test

# arm64 cross-builds via binfmt_misc + qemu-user, and boots the same way.
make -C build TARGET=arm64-qemu-ubuntu boot-test
```

Artifacts land in `out/<target>/`: the image, `boot.log`, `capability-profile.json`
recovered from the booted guest, `manifest.tsv`, and `SHA256SUMS`.

The Debian recipes (`amd64-qemu`, `arm64-qemu`) are the canonical product base but cannot
be bootstrapped in the OTWONO Cloud environment, whose proxy rejects Debian mirrors.

## Repository layout

```
CLAUDE.md      permanent engineering instructions (normative)
docs/          architecture, network, ai, security, hardware, build, services, roadmap, decisions
crates/
  otwono-hal          injectable hardware probes
  otwono-capability   axis classification, tiers, feature gates
  otwono-proto        Local Control Plane: JSON-RPC 2.0 over Unix sockets
  otwono-permd        permission broker: actions, policy, tokens, audit log
  otwono-hwd          hardware daemon (first guarded service)
  otwono-hwctl        inspection CLI, local and over the control plane
schemas/       JSON Schemas — the cross-language contracts
build/         Makefile, recipes, numbered stages, QEMU harnesses, installed files
tools/         environment probe, hardware fixture capture
tests/         contract tests and control-plane integration tests
```

## Contributing

Read [`CLAUDE.md`](CLAUDE.md) first. Two rules matter more than the rest:

- **Do not claim untested functionality works.** Every claim of function must trace to a
  test run, a build log, or a boot log.
- **Integrate mature open source; do not rewrite it.** Replacing an existing project
  requires an ADR with a technical justification.

## Licence

Apache-2.0.

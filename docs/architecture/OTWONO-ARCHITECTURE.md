# OTWONO AI OS — System Architecture

**Document status:** `SPECIFIED` — this is the architecture proposal for the project.
Implementation status of each subsystem is tracked in `docs/roadmap/ROADMAP.md` and in the
per-subsystem documents. Nothing in this document should be read as a description of
running software unless a section says `IMPLEMENTED` or `VERIFIED`.

**Version:** 0.1.0 · **Applies to:** OTWONO AI OS 0.x

---

## Table of contents

1. [Design goals and non-goals](#1-design-goals-and-non-goals)
2. [Overall OS architecture](#2-overall-os-architecture)
3. [Build-system strategy](#3-build-system-strategy)
4. [AMD64 and ARM64 support strategy](#4-amd64-and-arm64-support-strategy)
5. [Hardware capability-tier system](#5-hardware-capability-tier-system)
6. [AI runtime abstraction](#6-ai-runtime-abstraction)
7. [Decentralized node architecture](#7-decentralized-node-architecture)
8. [Node identity](#8-node-identity)
9. [Networking layers](#9-networking-layers)
10. [Data visibility model](#10-data-visibility-model)
11. [Distributed service architecture](#11-distributed-service-architecture)
12. [Security boundaries](#12-security-boundaries)
13. [Update architecture](#13-update-architecture)
14. [Application integration strategy](#14-application-integration-strategy)
15. [Testing strategy](#15-testing-strategy)

---

## 1. Design goals and non-goals

### Goals

| # | Goal | Consequence for the architecture |
|---|---|---|
| G1 | Local-first | No subsystem may require a cloud service to start or to serve its primary function. |
| G2 | Offline-capable | Every subsystem has a defined degraded mode with no Internet. |
| G3 | Hardware-adaptive | Feature availability is derived from a single machine-readable capability profile. |
| G4 | AI-native | The assistant is an OS subsystem with a brokered privilege model, not an app. |
| G5 | Decentralized by default | Node identity, discovery, and transport ship in the base image. |
| G6 | Modular | Independently buildable, testable, replaceable subsystems behind stable interfaces. |
| G7 | Integrate, don't rewrite | Mature open-source applications are driven, not replaced. |
| G8 | User sovereignty | Keys, data, and models live on the user's disk under explicit visibility labels. |

### Non-goals (for 0.x)

- Not a general-purpose distribution competing with Debian/Fedora on package breadth.
  We *are* Debian downstream; breadth is inherited.
- Not a blockchain, not a token, not a consensus system. The node network is a
  content-addressed, cryptographically-identified overlay, not a ledger.
- Not a training platform. Local fine-tuning is a Tier-4 stretch goal, not a core promise.
- Not an anonymity network. The overlay provides authentication and confidentiality,
  **not** traffic-analysis resistance. Anyone needing anonymity should run Tor over it.
  This must be stated plainly in the UI.

### The one-line architecture

> A Debian-derived base, plus a small set of Rust daemons speaking JSON-RPC over Unix
> sockets, that together expose (a) what this machine can do, (b) local AI inference,
> (c) a cryptographically-identified P2P overlay, and (d) a permission broker — with an
> agent layer on top that drives ordinary open-source applications through that broker.

---

## 2. Overall OS architecture

### 2.1 Layer model

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ L5  USER EXPERIENCE                                                          │
│     tier-selected shell: headless CLI · lightweight Wayland · full desktop   │
│     otwono-shell (assistant UI) · ordinary apps (LibreOffice, GIMP, Firefox) │
├──────────────────────────────────────────────────────────────────────────────┤
│ L4  AGENT LAYER                (unprivileged, brokered)                      │
│     otwono-agentd  — planner/executor, conversation, tool dispatch           │
│     tool registry  — typed actions with declared capability requirements     │
│     app adapters   — LibreOffice/GIMP/Audacity/Kdenlive/mpv/browser drivers  │
├──────────────────────────────────────────────────────────────────────────────┤
│ L3  OTWONO CORE SERVICES       (privileged, minimal, Rust daemons)           │
│  ┌────────────┬────────────┬────────────┬────────────┬────────────────────┐  │
│  │ otwono-hwd │ otwono-aid │otwono-netd │otwono-stored│ otwono-permd      │  │
│  │ hardware + │ AI runtime │ P2P overlay│ CAS + data  │ capability broker │  │
│  │ capability │ + models   │ + routing  │ + labels    │ + audit log       │  │
│  ├────────────┼────────────┼────────────┼────────────┼────────────────────┤  │
│  │ otwono-idd │ otwono-svcd│otwono-updated│                               │  │
│  │ node ident.│ dist. svcs │  A/B updates │                               │  │
│  └────────────┴────────────┴────────────┴───────────────────────────────── ┘ │
│     all speak: JSON-RPC 2.0 / NDJSON over /run/otwono/<svc>.sock             │
├──────────────────────────────────────────────────────────────────────────────┤
│ L2  BASE OS                                                                  │
│     systemd · Debian userland · Flatpak/bubblewrap · networkd/NM · PipeWire  │
├──────────────────────────────────────────────────────────────────────────────┤
│ L1  KERNEL + PLATFORM ENABLEMENT                                             │
│     Linux kernel · DRM/GPU · NPU drivers · SPI/I2C/UART for radio hardware   │
│     device trees (arm64) · ACPI/UEFI (amd64) · Landlock · seccomp · eBPF     │
├──────────────────────────────────────────────────────────────────────────────┤
│ L0  HARDWARE                                                                 │
│     x86_64 desktops/laptops/servers · arm64 SBCs · GPUs · NPUs · radios      │
└──────────────────────────────────────────────────────────────────────────────┘
```

### 2.2 Why daemons and not a single process

A single "OTWONO service" would be simpler to write and impossible to secure or to tier.
Splitting along **privilege** and **failure-domain** lines gives us:

- `otwono-permd` can be tiny and auditable — it is the only component that must be correct
  for the security model to hold.
- `otwono-netd` faces the hostile network and can be sandboxed hard (no filesystem, no
  exec, seccomp-narrow) without crippling the rest.
- `otwono-aid` holds gigabytes of model weights; it must be restartable and OOM-killable
  without taking networking down.
- On a Tier-0 board, `otwono-aid` may simply not be installed. Feature tiering is then a
  packaging decision, not a runtime `if`.

### 2.3 The Local Control Plane

> **Status: `VERIFIED`.** Implemented in `otwono-proto` and running on a booted amd64
> image. The rest of this section describes what exists.

One IPC mechanism for everything: **JSON-RPC 2.0, newline-delimited, over Unix domain
sockets** at `/run/otwono/<service>.sock`.

- Peer credentials come from `SO_PEERCRED` (uid/gid/pid).
- Authorization comes from a **capability token** in the request `params._cap`, issued by
  `otwono-permd`. `describe` is the sole unauthenticated method.
- Sockets have unit-enforced modes; the agent runs as its own `otwono-agent` user.

Rationale: language-neutral, mockable in a test with a temp directory, debuggable with
`socat`, and free of a D-Bus dependency in a 200 MB headless image. A D-Bus bridge for
desktop integration is allowed as a *bridge*, never as the source of truth.

### 2.4 Component responsibility table

| Component | Responsibility | Never does |
|---|---|---|
| `otwono-hwd` | Probe hardware; publish the capability profile; watch for hotplug | Decide feature policy for other subsystems beyond publishing the profile |
| `otwono-aid` | Model catalog, download, admission control, inference sessions | Touch the network overlay directly |
| `otwono-idd` | Generate/hold node identity keys; sign and verify | Perform network I/O |
| `otwono-netd` | Links, transport, peer discovery, routing, gateways | Read user files; store user data |
| `otwono-stored` | Content-addressed store, visibility labels, replication policy | Make trust decisions about peers |
| `otwono-svcd` | Host and consume distributed services (wiki, forum, profiles, media) | Bypass `otwono-stored` labels |
| `otwono-permd` | Issue/verify capability tokens; policy; user prompts; audit log | Perform the privileged action itself |
| `otwono-updated` | A/B image updates, verification, rollback | Update itself in place |
| `otwono-agentd` | Plan and execute user intent through declared tools | Hold ambient privilege; run as root |

---

## 3. Build-system strategy

### 3.1 The decision

**Debian-derived rootfs assembled by a staged, declarative image builder.**

Considered and rejected:

| Option | Why not |
|---|---|
| **Yocto / OpenEmbedded** | Best-in-class for embedded reproducibility, but we must ship LibreOffice, GIMP, Kdenlive, Firefox on two architectures. Maintaining a desktop application stack in Yocto is a multi-person-year commitment. Rejected for the general-purpose path. |
| **Buildroot** | Excellent for a 30 MB appliance, no package manager on-device, no desktop story. Rejected. |
| **NixOS** | Superb reproducibility and atomic updates — genuinely tempting. Rejected for 0.x because ARM SBC board enablement (vendor kernels, device trees, non-mainline BSPs) is significantly more work there, and it forces the whole project into one paradigm early. Revisit for 1.x; see open question OQ-2. |
| **Arch / Alpine** | Alpine's musl base breaks too much proprietary GPU/NPU tooling; Arch has no stable release train suitable for an appliance-like update model. |
| **Debian** | **Chosen.** First-class amd64 *and* arm64, enormous mature application set, `snapshot.debian.org` gives time-pinned reproducible package sources, no vendor lock-in, and a well-understood `debootstrap` → customize → image pipeline. |

### 3.2 Pipeline

Numbered, idempotent stages under `build/stages/`, driven by `build/Makefile` and a
declarative recipe in `build/recipes/<target>.toml`:

```
00-probe-env      verify host tools + reachable mirrors; fail early with a clear message
10-bootstrap      debootstrap/mmdebstrap a minimal base rootfs for $ARCH
20-base-config    locale, users, systemd presets, fstab, apt pinning
30-otwono         install OTWONO daemons, units, schemas, default policy
40-kernel         kernel + initramfs + firmware + (arm64) device trees / U-Boot
50-image          partition (GPT: ESP + A + B + data), populate, install bootloader
60-verify         checksums, manifest, then QEMU boot smoke test
```

Every stage:

- declares its network needs in a header comment,
- is safe to re-run,
- writes to `out/<target>/<stage>/`,
- appends to a build manifest.

### 3.3 Reproducibility

| Mechanism | Purpose |
|---|---|
| `SOURCE_DATE_EPOCH` | Deterministic timestamps in tar, ext4, and generated files |
| Snapshot-pinned mirror (`snapshot.debian.org/archive/debian/<ts>/`) | Same package versions on every run |
| `manifest.lock` (package, version, arch, sha256) | Auditable, diffable bill of materials |
| Fixed filesystem UUIDs derived from the recipe + epoch | Bit-identical images |
| `SHA256SUMS` beside every artifact | Verification and update deltas |

Reproducibility target for 0.x: **byte-identical rootfs tarball across two runs on the
same host with the same snapshot pin**. Full cross-host bit-reproducibility is a 1.x goal.

### 3.4 Parameterization (and this environment)

`BASE_DISTRO`, `BASE_SUITE`, `BASE_MIRROR`, `ARCH`, and `TIER_PROFILE` are recipe
parameters. Debian is the canonical product base. The current OTWONO Cloud dev environment
is behind an allow-listing proxy that **rejects Debian mirrors and permits Ubuntu
mirrors**, so an `ubuntu-noble` recipe exists and is what CI exercises here. The pipeline
must never assume one or the other.

---

## 4. AMD64 and ARM64 support strategy

### 4.1 Principle: one OS, two boot stories

Everything above the bootloader is architecture-neutral: same daemons, same units, same
schemas, same package names. Divergence is confined to `40-kernel` and `50-image`.

| Concern | amd64 | arm64 |
|---|---|---|
| Firmware | UEFI (OVMF under QEMU) | UEFI (EDK2/AAVMF) where available; U-Boot on most SBCs |
| Bootloader | GRUB2 EFI, or systemd-boot | GRUB2 EFI on SBSA boards; U-Boot + extlinux/boot script on SBCs |
| Hardware description | ACPI | Device Tree (per-board `.dtb`), ACPI on SBSA servers |
| Kernel | Debian generic amd64 | Debian generic arm64 where it boots; per-board vendor kernel where it must |
| Storage | NVMe/SATA/USB | eMMC/SD/USB/NVMe |
| Image | GPT + ESP, UEFI-bootable `.img` and hybrid ISO | GPT + ESP `.img` written to SD/eMMC; per-board bootloader offset blobs |

### 4.2 Board support packages

Boards that need non-mainline enablement get a **BSP directory** rather than patches
scattered through the builder:

```
build/bsp/<vendor>-<board>/
  bsp.toml          kernel source/flavour, dtb name, console, u-boot offsets
  overlays/         device-tree overlays
  firmware/         non-redistributable blobs are referenced by URL, never committed
  hooks/            pre/post image hooks
```

Policy: **mainline kernel first.** A vendor kernel is a documented exception with an exit
plan, because vendor kernels are where long-term maintenance dies.

### 4.3 Cross-building

- Rust: `--target aarch64-unknown-linux-gnu` with `aarch64-linux-gnu-gcc` as linker.
  *(Confirmed available in the dev environment.)*
- Rootfs: `debootstrap --foreign` + `qemu-aarch64-static` second stage via `binfmt_misc`,
  or native `arm64` builders when available.
- Anything that cannot be cross-built is built inside a QEMU-user chroot, never on the
  developer's word that "it should work".

### 4.4 QEMU targets

| Target | Machine | Firmware |
|---|---|---|
| `amd64-qemu` | `q35`, virtio | OVMF |
| `arm64-qemu` | `virt`, `-cpu cortex-a72`, virtio | AAVMF/EDK2 |
| `arm64-rpi4` | real hardware; `raspi3b`/`raspi4b` under QEMU is partial | U-Boot |

No KVM in this dev environment ⇒ TCG only ⇒ boot tests are minutes, not seconds. Test
harnesses must set generous timeouts and stream the serial console to a log file.

---

## 5. Hardware capability-tier system

### 5.1 Why a vector, not a number

A Raspberry Pi 5 with 16 GB RAM and no GPU, and a 4-core laptop with an RTX 4060 and 8 GB
RAM, are not comparable on one axis. So the profile is a **capability vector** and the
overall tier is a documented composition of it.

### 5.2 Axes

Each axis is classified independently by `otwono-hal` + `otwono-capability`:

| Axis | Inputs | Classes |
|---|---|---|
| `compute` | core count, threads, base/max MHz, ISA extensions (AVX2, AVX-512, NEON, SVE, i8mm, dotprod) | `minimal · low · medium · high · extreme` |
| `memory` | total RAM, available RAM, swap, memory bandwidth hint | `minimal · low · medium · high · extreme` |
| `accelerator` | discrete/integrated GPU, VRAM, compute API (CUDA/ROCm/Vulkan/OpenCL), NPU (RKNN, Hailo, Coral, Intel NPU, AMD XDNA), TOPS estimate | `none · npu_small · igpu · gpu_small · gpu_large · gpu_multi` |
| `storage` | total/free bytes, rotational vs SSD vs NVMe, measured or declared throughput class | `constrained · standard · fast · bulk` |
| `network` | link types present, uplink presence, radio hardware (LoRa, 802.15.4, Wi-Fi AP/mesh capable) | `offline · intermittent · lan · broadband · gateway` |
| `power` | AC vs battery vs PoE, thermal headroom, declared TDP budget | `constrained · managed · unconstrained` |

### 5.3 Overall tiers

The overall tier is the **weakest of the binding axes** for that tier's requirements — a
machine is only as capable as its bottleneck.

| Tier | Name | Typical hardware | Local AI | Node role |
|---|---|---|---|---|
| **T0** | `MICRO` | Pi Zero 2 W, 512 MB–1 GB, 1–2 cores | No local LLM. Rule/grammar assistant + delegation to a trusted peer or the cloud if the user allows | Relay / store-and-forward only |
| **T1** | `EDGE` | Pi 4 2–4 GB, RK3566 | 1–3B quantized (Q4), CPU only, single agent, no RAG index | Full node, no bulk replication |
| **T2** | `BALANCED` | Pi 5 8–16 GB, mini-PC, laptop with iGPU | 7–8B Q4, embeddings + local RAG, 1–3 agents, ASR small | Full node + selective replication |
| **T3** | `CAPABLE` | Desktop, ≥16 GB RAM, discrete GPU ≥8 GB VRAM | 14–32B, GPU offload, speculative decoding, ASR/TTS, optional image gen | Full node + gateway + serves AI to authorized peers |
| **T4** | `WORKSTATION` | ≥32 GB RAM, ≥24 GB VRAM or multi-GPU/NPU | 70B-class, concurrent agents, image/video models, optional fine-tuning | Everything, plus a network AI provider and archival replica |

### 5.4 The profile is the contract

`otwono-hwd` publishes a **capability profile** validated against
`schemas/capability-profile.schema.json`. Every other subsystem consumes the profile.
No subsystem may run its own "is this box big enough" check. Consequences:

- Tiering is testable offline: feed a fixture profile, assert the enabled feature set.
- Tiering is overridable: `/etc/otwono/capability.override.toml` lets a user force a tier
  (up or down) — with a loud warning if forced up, because an over-forced tier turns into
  OOM kills, not magic.
- Hotplug (eGPU attach, USB LoRa dongle, AC unplug) re-evaluates the profile and emits a
  change event; subsystems subscribe rather than poll.

Detail: `docs/hardware/CAPABILITY-TIERS.md`.

---

## 6. AI runtime abstraction

### 6.1 The problem

Inference backends differ wildly by hardware: llama.cpp with Vulkan on a Pi, llama.cpp
with CUDA on a desktop, ONNX Runtime with RKNN on a Rockchip NPU, vLLM on a workstation,
and a *remote peer's* GPU over the node network. The agent layer must not know or care.

### 6.2 The abstraction

`otwono-aid` exposes one interface — `infer`, `embed`, `transcribe`, `synthesize`,
`vision`, `list_models`, `session` — over the Local Control Plane, and dispatches to a
**backend** selected by the capability profile and the model manifest.

```
        agent / apps / peers
                 │  JSON-RPC (infer, embed, ...)
        ┌────────▼─────────┐
        │   otwono-aid     │  model catalog · admission control · session mgmt · queueing
        └────────┬─────────┘
     ┌───────────┼───────────┬──────────────┬───────────────┐
┌────▼────┐ ┌────▼────┐ ┌────▼─────┐  ┌─────▼──────┐  ┌─────▼──────┐
│llama.cpp│ │llama.cpp│ │ONNX RT   │  │   vLLM     │  │ remote     │
│  CPU    │ │GPU      │ │NPU       │  │ (T3/T4)    │  │ peer via   │
│ (all)   │ │CUDA/ROCm│ │RKNN/XDNA │  │            │  │ otwono-netd│
│         │ │/Vulkan  │ │/Hailo    │  │            │  │            │
└─────────┘ └─────────┘ └──────────┘  └────────────┘  └────────────┘
```

Rules:

- **Backends are integrations.** We do not write an inference engine.
- Backends are **out-of-process** and supervised. A backend crash must not take `otwono-aid`
  down, and must surface as a typed error, not a hang.
- **Admission control is mandatory.** Before loading a model, `otwono-aid` checks the
  profile's memory/VRAM headroom against the model manifest's declared footprint and
  refuses with a clear error rather than triggering the OOM killer. This is the single most
  common way local-AI systems become unusable on small hardware.
- **Models are content-addressed** and stored via `otwono-stored`, with manifests declaring
  minimum tier, quantization, context limit, footprint, licence, and hash. Model files are
  never committed to git.
- **Remote inference is opt-in, per-peer, and visible.** Sending a prompt to a peer is a
  data-egress event: it is subject to the visibility label of the material in the prompt
  and it is logged.

Detail: `docs/ai/AI-RUNTIME.md`.

### 6.3 The assistant

`otwono-agentd` is a planner/executor over a **typed tool registry**. Each tool declares:
its parameters (JSON Schema), the capabilities it requires, whether it is reversible, and
its blast radius. The agent cannot invoke an action that is not a registered tool, and
every invocation goes through `otwono-permd`.

Tier shapes the agent: T0 gets a deterministic command-grammar assistant with no LLM; T1
gets a single-step tool-calling loop; T2+ get planning, RAG over local knowledge, and
multi-step execution; T3+ get parallel sub-agents.

---

## 7. Decentralized node architecture

**Name:** OTWONO Node Mesh (**ONM**).

### 7.1 Position

ONM is a **first-class OS subsystem**, installed and running in the base image, not an
application. `otwono-netd` starts at boot, generates/loads the node identity, brings up
every available link, and offers local services whether or not the Internet exists.

Normal Internet networking is untouched — NetworkManager/systemd-networkd continue to own
the ordinary stack. ONM is an **overlay beside it**, not a replacement for it.

### 7.2 What a node is

A node is: a persistent keypair, a set of links, a content store with labels, a service
registry, and optionally an AI provider. Nodes are peers; there are no servers, though
nodes may take on **roles**:

| Role | Meaning | Typical tier |
|---|---|---|
| `leaf` | Participates, stores only its own data | T0–T1 |
| `relay` | Forwards traffic for others | T1+ |
| `cache` | Holds `REPLICATED` content for availability | T2+ |
| `gateway` | Bridges ONM to the Internet (and vice-versa, if the user allows) | T2+ with uplink |
| `ai-provider` | Offers inference to authorized peers | T3+ |
| `archive` | Long-term replica of selected collections | T4 / dedicated storage |

Roles are proposed from the capability profile and confirmed by the user. Nothing that
consumes the user's bandwidth, disk, or GPU for other people's benefit is ever enabled
silently.

### 7.3 Subsystem decomposition

```
otwono-netd
├── link/          LinkAdapter implementations (ethernet, wifi, wifi-direct, BLE,
│                  LoRa, 802.15.4, AX.25, USB-gadget, internet-transport)
├── transport/     Noise-secured channels; QUIC where IP exists
├── discovery/     mDNS (LAN), link-local beacons (radio), DHT (Internet), manual peers
├── routing/       reachability table, next-hop selection, store-and-forward queue
├── gateway/       ONM↔Internet bridging, opt-in, policy-limited
└── rpc/           local control plane surface
```

Each link adapter reports a **bandwidth class** and a **duty-cycle constraint**. LoRa is
`trickle` (hundreds of bytes per second, legally duty-cycle-limited): the router must never
attempt bulk transfer over it, and services must have a `trickle`-safe representation
(text-first, images on demand). This is a hard architectural constraint, not a tuning knob.

### 7.4 Offline behaviour

With no Internet: local peer discovery, mesh routing, local services, distributed search
over reachable peers, messaging with store-and-forward, and media sharing all continue.
With no peers at all: every service still runs locally against the local store — the wiki,
the notes, the profile site, and the assistant are all usable on one disconnected machine.

Detail: `docs/network/NODE-NETWORK.md`.

---

## 8. Node identity

### 8.1 Shape

- **Long-term identity key:** Ed25519, generated on first boot, stored in
  `/var/lib/otwono/identity/` with `0600`, sealed to a TPM 2.0 or ARM TrustZone-backed
  keystore where one exists.
- **NodeID:** a multihash/multibase encoding of the SHA-256 of the public key, rendered as
  a short human-checkable fingerprint (e.g. `otw1:qm7f-2k9x-...`).
- **Independent of IP, MAC, hostname, and physical network.** The identity survives moving
  between networks, changing hardware, and reinstalling if the key is backed up.
- **Derived keys:** X25519 for key agreement (Noise), separate short-lived session keys,
  and per-service subkeys — the long-term key signs, it does not encrypt bulk traffic.
- **Device vs person:** a *node* identity is a device. A *user* identity is separate and may
  span several of the user's nodes; user↔node binding is a signed certificate. Conflating
  them is a mistake we are explicitly avoiding.

### 8.2 Rotation, revocation, recovery

- Rotation: a new key signed by the old one, published as a signed succession record.
- Revocation: a signed revocation record propagated as `REPLICATED` content; peers cache it.
- Recovery: the identity is exportable as an encrypted backup (passphrase + optional
  Shamir split). Losing the key means losing the identity — the UI must say this once, in
  plain language, at first boot, and offer backup immediately.

No global registry, no consensus, no blockchain. Trust is local and explicit: users mark
peers as known, with a petname, exactly like SSH known-hosts but with a usable UI.

Detail: `docs/network/NODE-IDENTITY.md`.

---

## 9. Networking layers

ONM's own stack, deliberately mapped to familiar layer numbering:

| Layer | Name | Content | Reuse |
|---|---|---|---|
| **N0** | Physical | Ethernet, Wi-Fi (incl. AP/IBSS/Wi-Fi Direct), BLE, LoRa SX126x/127x, 802.15.4, AX.25 packet radio, USB gadget, serial | kernel drivers |
| **N1** | Link adapter | Uniform datagram/stream API with MTU, bandwidth class, duty cycle, energy cost | our thin abstraction |
| **N2** | Secure channel | Noise `XX` mutual authentication → AEAD channel. QUIC (TLS 1.3 + raw public keys) where IP exists | `snow`/`rust-libp2p`, `quinn` |
| **N3** | Identity & addressing | NodeID-addressed frames, independent of IP | ours |
| **N4** | Discovery | mDNS/DNS-SD on LAN, beacons on radio links, Kademlia DHT on the Internet, manual/QR pairing | `libp2p-kad`, `libp2p-mdns` |
| **N5** | Routing | Link-state within a mesh partition; store-and-forward across partitions; opportunistic/DTN semantics on `trickle` links | candidates: Babel, Yggdrasil, Reticulum |
| **N6** | Data | Content-addressed blocks (BLAKE3), signed mutable pointers, chunked transfer, resumable | `iroh`/`bao`/`libp2p-bitswap` candidates |
| **N7** | Services | Service records, request/response and pub/sub over the overlay, HTTP-over-ONM for local websites | ours + `hyper` |

### 9.1 Reuse posture

Writing a mesh routing protocol is a research project, not a sprint. The plan is to
**integrate an existing one** and keep `routing/` behind an interface so the choice can be
revisited. Reticulum is the closest fit for radio + DTN semantics; Yggdrasil is the
strongest for IP-overlay routing; Babel is proven for Wi-Fi mesh. The evaluation is
tracked as open question OQ-4 and must end in an ADR with measurements, not opinions.

Similarly, the Internet-side transport, discovery, and DHT are **rust-libp2p**, not ours.
Our code is the link adapters for non-IP radio, the label/policy enforcement, and the glue.

### 9.2 Internet gateways and federation

- A `gateway` node with an uplink can, if the user opts in, (a) let ONM peers reach the
  Internet through it under an explicit policy, and (b) let ONM nodes on different physical
  networks find each other via the DHT and public relays.
- **Federation between separate node networks** happens when both have Internet: networks
  exchange signed *network descriptors*, then selectively replicate `PUBLIC`/`REPLICATED`
  collections and forward addressed messages across the boundary. Federation is
  per-collection and per-policy, never "join everything".

---

## 10. Data visibility model

Every object in `otwono-stored` carries a label. These are enforced, not advisory.

| Label | Storage | Network behaviour | Encryption |
|---|---|---|---|
| `PRIVATE` | Local only | Never transmitted by any subsystem on its own | At rest with the node key; keys never leave |
| `SHARED` | Local | Transmitted only to explicitly authorized nodes/users | Per-recipient key wrapping; content encrypted |
| `PUBLIC` | Local, served on request | Served to any peer permitted by network policy | Signed, not encrypted |
| `REPLICATED` | Local + peer replicas | Actively copied to caching/archive peers | Signed, not encrypted; replication policy attached |

Rules that fall out of this:

- **Default `PRIVATE`.** Unlabelled or unparseable ⇒ `PRIVATE`. Fail closed.
- **Promotion is an explicit user act.** The agent may propose; only the user promotes.
- **Demotion is honest.** Un-publishing removes local serving and asks peers to drop, but
  the UI must state plainly that replicated public data cannot be recalled.
- **Labels propagate through derivation.** A summary of a `PRIVATE` document is `PRIVATE`.
  This is enforced in `otwono-stored`'s provenance chain, and it is the mechanism that stops
  an agent from laundering private data into a public post.
- **Prompts are data.** Sending `PRIVATE` content to a remote inference provider is an
  egress event that the label model must block by default.
- A `REPLICATED` object carries a policy: replica target, TTL, max hops, size cap, and
  whether re-replication is allowed.

Detail: `docs/security/DATA-VISIBILITY.md`.

---

## 11. Distributed service architecture

Services are **applications of the same three primitives**: content-addressed storage,
signed mutable pointers, and addressed messaging. We are not building eight independent
systems.

| Service | Built from |
|---|---|
| Local websites / profile sites | `PUBLIC` content collection + a signed site pointer + HTTP-over-ONM rendering |
| Wiki / knowledge base | Append-only signed page revisions; last-writer-wins per page with explicit merge for conflicts |
| Forums | Signed posts in topic collections; moderation = per-node subscription to signed moderation lists, not global authority |
| Messaging | Addressed, end-to-end encrypted envelopes with store-and-forward for offline recipients |
| Document / image / audio / video sharing | Chunked content-addressed blobs + a manifest; streamed on demand |
| Decentralized media viewing | Range-requested chunks with local transcode via ffmpeg/mpv |
| Distributed search | Local full-text index over local + subscribed content; queries fan out to reachable peers with per-peer scope and rate limits |
| AI services between nodes | `otwono-aid` exposed over ONM to authorized peers, with quotas |
| Permission-controlled data sharing | Falls directly out of `SHARED` labels + per-peer authorization |

Design rules:

- Every service must have a **`trickle`-safe mode** — a text-first representation that
  survives a LoRa link.
- Every service must work **entirely locally** with zero peers.
- Services are addressed by **NodeID + service name**, never by IP or DNS.
- No service may bypass `otwono-stored`'s label enforcement to reach the network.

Detail: `docs/services/DISTRIBUTED-SERVICES.md`.

---

## 12. Security boundaries

### 12.1 Trust zones

| Zone | Contents | Privilege |
|---|---|---|
| **Z0** | Kernel, firmware, TPM | Full |
| **Z1** | `otwono-permd`, `otwono-idd`, `otwono-updated` | Root, minimal, heavily hardened, small enough to audit |
| **Z2** | `otwono-hwd`, `otwono-aid`, `otwono-stored`, `otwono-svcd` | Dedicated users, narrow capabilities |
| **Z3** | `otwono-netd` | Dedicated user, **no filesystem write outside its spool, no exec**, network only — this is the hostile-input boundary |
| **Z4** | `otwono-agentd`, app adapters | Unprivileged user, **zero ambient privilege**, everything brokered |
| **Z5** | User applications (LibreOffice, GIMP, browser) | User, sandboxed via Flatpak/bubblewrap where practical |
| **Z6** | Remote peers | Untrusted by default; authenticated ≠ trusted |

Data crossing Z6→Z3 is untrusted bytes. Data crossing Z3→Z2 has been authenticated but is
still untrusted content. **Content from the network is never instruction.**

### 12.2 The permission broker

`otwono-permd` is the security kernel. Every privileged operation is a **typed action**
with declared parameters. The broker resolves an action against policy to
`allow | deny | ask` and issues a **short-lived, single-purpose capability token**.

- Tokens are scoped (service, method, resource pattern), time-limited, and one-shot for
  destructive actions.
- **User confirmation is required** for: deleting or overwriting user data, promoting a
  visibility label, spending money, installing software, changing security policy, enabling
  a network role, sending data off-node, and any action marked irreversible.
- Everything is written to an **append-only, hash-chained audit log** that the user can
  read and the agent cannot edit.
- Policy is declarative, human-readable, and diffable in `/etc/otwono/policy.d/`.

### 12.3 Agent-specific threats

The agent is the newest and most dangerous attack surface, because it turns text into
actions. Explicit mitigations:

| Threat | Mitigation |
|---|---|
| Prompt injection from a web page, document, email, or peer content | Untrusted content is tagged with provenance and cannot grant capabilities; high-blast-radius tools require user confirmation regardless of what the model "decided" |
| Confused deputy (agent used as a privilege bridge) | Capability tokens are scoped to the *originating user request*, not to the agent |
| Data exfiltration via a plausible-looking action | Label-aware egress checks; `PRIVATE` never leaves without explicit confirmation |
| Model supply chain | Models are hash-pinned with signed manifests; unverified models require explicit opt-in and run with reduced tool access |
| Runaway automation | Resource and rate limits; a global kill switch; dry-run mode for destructive plans |

### 12.4 Platform hardening

Verified boot where the hardware supports it, full-disk encryption (TPM-sealed with a
passphrase fallback), systemd unit hardening as a baseline (`ProtectSystem=strict`,
`NoNewPrivileges`, `PrivateDevices`, `SystemCallFilter`), Landlock for per-daemon
filesystem scoping, and seccomp profiles for `otwono-netd`.

Detail: `docs/security/SECURITY-MODEL.md`, `docs/security/THREAT-MODEL.md`.

### 12.5 What we do not promise

We do not promise anonymity, traffic-analysis resistance, or protection against an attacker
with physical access to an unencrypted disk. Saying so plainly is part of the security model.

---

## 13. Update architecture

### 13.1 A/B images with rollback

Two root slots plus a persistent data partition:

```
GPT: [ ESP ] [ root_a ] [ root_b ] [ otwono-data ]
```

- Updates are **atomic**: write the inactive slot, verify, flip the boot pointer, reboot.
- **Rollback is automatic**: a boot-attempt counter (GRUB env on amd64, U-Boot `bootcount`
  on arm64) reverts to the previous slot if the new one fails to reach a healthy state.
- A userspace health check must confirm success before the new slot is marked good.
- `/var` and `/home` live on the data partition and survive updates. The root filesystem is
  treated as **replaceable**, which is what makes rollback safe.

Candidate implementations, to be settled by ADR: **RAUC** (mature, embedded-focused,
bundle signing, works on both architectures) or **systemd-sysupdate** (fewer moving parts,
newer). We will not write our own updater.

### 13.2 Layers update independently

| Layer | Mechanism | Cadence |
|---|---|---|
| Base image (kernel, base OS, daemons) | A/B image update, signed | Release train |
| Applications | Flatpak / apt | Independent |
| AI models | Content-addressed, hash-pinned, tier-gated | Independent |
| Policy and schemas | Signed configuration bundles | Independent |

Decoupling matters: a user must be able to take a security fix without a new model, and a
new model without a new kernel.

### 13.3 Updates over the mesh

Update bundles are content-addressed and **`REPLICATED`**, so a node with an uplink can
seed an update to an offline cluster of nodes over ONM. Signature verification is against
the release key, so a peer relaying an update can never tamper with it — which is precisely
why content addressing plus signing, rather than transport trust, is the right primitive.

Detail: `docs/build/UPDATE-ARCHITECTURE.md`.

---

## 14. Application integration strategy

### 14.1 The rule

The OS **drives** mature applications. We add an adapter, not a replacement.

### 14.2 The adapter contract

An app adapter is a declarative manifest plus a small driver, providing:

1. **Discovery** — is the app installed, which version, which backends.
2. **Capabilities** — the typed actions it exposes (`document.replace_text`,
   `image.resize`, `video.trim`), each with a JSON Schema.
3. **Invocation** — the preferred control channel.
4. **Verification** — how to check the action actually happened.
5. **Reversibility** — undo, or a pre-action snapshot.

### 14.3 Control channels, in order of preference

1. **Documented API / scripting interface** — LibreOffice UNO, GIMP Script-Fu/Python-Fu,
   Inkscape actions, mpv IPC socket, Krita Python.
2. **CLI** — ffmpeg, ImageMagick, pandoc, `rg`, `fd`, git, apt, systemctl.
3. **File-format manipulation** — edit the ODF/SVG/project file directly, then reload.
4. **Accessibility APIs** (AT-SPI) — last resort, for apps with no other surface.
5. **Screen/pointer synthesis** — explicitly discouraged; requires an ADR. It is brittle,
   unverifiable, and unauditable.

### 14.4 Non-negotiables

- Every adapter action goes through `otwono-permd`.
- Every destructive action is preceded by a snapshot or is refused.
- Adapters degrade by tier: on T0/T1 the CLI adapters (ffmpeg, pandoc, ImageMagick) are
  present but the GUI apps may not be installed; the registry reflects reality rather than
  letting the agent hallucinate a tool that is not there.
- The agent may only call registered adapter actions. There is no generic "run this shell
  command" tool without an explicit, separately-confirmed capability.

Detail: `docs/ai/APP-INTEGRATION.md`.

---

## 15. Testing strategy

### 15.1 Pyramid

| Level | What | Where | Gate |
|---|---|---|---|
| **L1 Unit** | Pure logic: classifiers, parsers, policy evaluation | `crates/*/src` | Every commit |
| **L2 Fixture** | Probes against captured real `/proc` and `/sys` trees from real machines | `crates/*/tests/fixtures/` | Every commit |
| **L3 Contract** | JSON Schema validation of every emitted document | `tests/` | Every commit |
| **L4 Integration** | Daemons over the Local Control Plane on a temp socket dir | `tests/integration/` | Every commit |
| **L5 Multi-node** | Several nodes in network namespaces / QEMU VMs: discovery, transport, replication, partition healing | `tests/integration/` | Nightly |
| **L6 Image / boot** | QEMU boot of amd64 and arm64 images to a healthy state | `build/qemu/` | Every image build |
| **L7 Hardware** | Real boards: Pi 4, Pi 5, RK3588, x86 laptop with a GPU | manual / lab | Every release |

### 15.2 Rules that matter more than the pyramid

- **Hardware probes are injectable.** Every probe takes a root path. Production passes `/`;
  tests pass a fixture directory. No probe may hardcode `/proc` or `/sys` at a call site.
  This is what makes hardware detection testable on a machine you do not have.
- **Fixtures come from real hardware.** A synthetic fixture is allowed only when marked as
  synthetic, and it is a placeholder until a real capture replaces it.
- **QEMU for anything architecture-specific.** With no KVM in this environment, TCG runs
  take minutes; harnesses stream the serial console to a log and use generous timeouts.
- **Network tests use partitions.** A distributed system that has never been partitioned in
  a test is a distributed system that does not work. Partition, heal, and assert
  convergence.
- **Boot log or it did not boot.** "Verified" requires an artifact.
- **Negative security tests are first-class.** Assert that an unauthorized call is refused,
  that a `PRIVATE` object is not transmitted, and that injected instructions in peer content
  do not produce a privileged action.

---

## Appendix A — Open questions

Tracked in `docs/decisions/OPEN-QUESTIONS.md`.

## Appendix B — Decision records

`docs/decisions/` holds ADRs. The architecture above encodes ADR-0001 through ADR-0008;
anything marked "candidate" here is deliberately unresolved and must not be treated as
settled.

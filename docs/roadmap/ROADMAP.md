# OTWONO AI OS — Phased Development Roadmap

Each phase has an **exit criterion** that is a verifiable artifact — a test run, a build
log, or a boot log — not an opinion. A phase is not complete until that artifact exists.

Status legend: `SPECIFIED` (design only) · `IMPLEMENTED` (code + unit tests)
· `VERIFIED` (exercised end to end with a log).

---

## Phase 0 — Foundation ✅ current

**Goal:** a clean, extensible skeleton nobody has to undo later.

| Item | Status |
|---|---|
| `CLAUDE.md` engineering instructions | `IMPLEMENTED` |
| Documentation structure and architecture proposal | `IMPLEMENTED` |
| Environment probe (`tools/probe-env.sh`) | `VERIFIED` — run, output recorded |
| Rust workspace, crate layout, CI-able test suite | `VERIFIED` — `cargo test --workspace` passes |
| Hardware detection (`otwono-hal`) | `IMPLEMENTED` — probes + fixture tests |
| Capability classification (`otwono-capability`) | `IMPLEMENTED` — unit tests over synthetic and captured profiles |
| `otwono-hwctl` CLI with JSON output | `VERIFIED` — run on this host, output validates against the schema |
| Build system skeleton, recipes, QEMU harnesses | `IMPLEMENTED` — `00-probe-env` runs; no image built yet |

**Exit criterion:** `cargo test --workspace` green and `otwono-hwctl profile --json` emitting
a schema-valid profile on real hardware. **Met.**

---

## Phase 1 — Bootable base images  ← *recommended next milestone*

**Goal:** an image that boots, on both architectures, with a boot log to prove it.

1. `10-bootstrap` — minimal rootfs for amd64 and arm64 (arm64 via `binfmt_misc` + `qemu-aarch64-static`).
2. `20-base-config` — systemd, users, fstab, locale, apt pinning.
3. `40-kernel` — kernel, initramfs, arm64 device trees.
4. `50-image` — GPT with ESP + `root_a` + `root_b` + data; GRUB EFI (amd64), U-Boot/EFI (arm64).
5. `60-verify` — QEMU boot to a login prompt, serial captured to a log.
6. `otwono-hwctl` shipped in the image and run at first boot.

**Exit criterion:** `out/amd64-qemu/boot.log` and `out/arm64-qemu/boot.log` both showing a
login prompt, plus `otwono-hwctl profile --json` output captured from **inside** each VM.

**Risks:** no KVM ⇒ slow TCG boots; Debian mirrors blocked in this environment ⇒ the Ubuntu
recipe is what CI can exercise; arm64 U-Boot/EFI on `qemu-virt` needs care.

---

## Phase 2 — Service architecture and the permission broker

**Goal:** the Local Control Plane exists and the security model has teeth.

1. `otwono-proto` — JSON-RPC 2.0 / NDJSON over UDS, client and server, `describe`, typed errors.
2. `otwono-hwd` — the first real daemon; publishes the profile, emits hotplug change events.
3. `otwono-permd` — typed actions, policy evaluation, capability tokens, hash-chained audit log.
4. systemd units with the full hardening baseline; Landlock scoping.
5. Integration tests over a temp socket directory; **negative** authorization tests.

**Exit criterion:** an integration test in which an unauthorized caller is refused, an
authorized caller succeeds, and the audit log's hash chain verifies.

---

## Phase 3 — Node identity and secure transport

1. `otwono-idd` — Ed25519 generation, TPM sealing where present, backup/restore, rotation.
2. NodeID encoding, fingerprints, the pairing flow.
3. `otwono-netd` skeleton with the `LinkAdapter` trait and an Ethernet/IP adapter.
4. Noise `XX` over the link; QUIC via rust-libp2p on IP.
5. mDNS discovery on the LAN.

**Exit criterion:** two QEMU VMs on a virtual LAN discover each other, complete a mutually
authenticated handshake, and exchange a signed message. Log captured. Identity survives a
reboot of both VMs.

---

## Phase 4 — Local AI runtime

1. `otwono-aid` with the backend abstraction and llama.cpp CPU first.
2. Model catalog, signed manifests, content-addressed storage, tier gating.
3. Admission control with a real refusal path — `ModelTooLargeForTier` must be observed.
4. GPU/NPU backends behind capability detection.
5. Tiered assistant shapes T0–T2.

**Exit criterion:** the same `ai.infer` request served on an amd64 VM and an arm64 VM with
tier-appropriate models, plus a test proving admission control refuses an oversized model
instead of triggering the OOM killer.

---

## Phase 5 — Content store and data visibility

1. `otwono-stored` — BLAKE3 CAS, chunking, dedup.
2. The four labels, enforced. Encryption at rest. Provenance propagation.
3. Egress enforcement in `otwono-netd`, duplicated as defence in depth.
4. Replication policy for `REPLICATED`.

**Exit criterion:** the negative test suite from `docs/security/DATA-VISIBILITY.md` §6
passes, including a property test that derived content inherits the most restrictive label.

---

## Phase 6 — First distributed services

Profile site, wiki, and messaging, in that order — they exercise all three primitives
between them. `onm://` addressing, local resolver, browser integration, `Trickle`-safe modes.

**Exit criterion:** a three-node QEMU network where node A's wiki page is readable on node
B, an offline message to node C is delivered when C returns, and a network partition heals
with convergence asserted.

---

## Phase 7 — Agent layer and application integration

Tool registry, first adapters (pandoc, ffmpeg, ImageMagick, mpv, LibreOffice), planner and
executor, snapshot-before-destructive, dry-run mode.

**Exit criterion:** an end-to-end task ("convert these images and put them in a document")
executed through the broker with a complete audit trail, on both architectures.

---

## Phase 8 — Updates

RAUC or systemd-sysupdate integration, signed bundles, boot-counter rollback, mesh-delivered
updates.

**Exit criterion:** a VM updated A→B, and a deliberately broken image rolled back
automatically. Both boot logs captured.

---

## Phase 9 — Mesh and radio

Wi-Fi Direct and LoRa link adapters, store-and-forward, the routing decision from OQ-4,
duty-cycle enforcement, DTN semantics.

**Exit criterion:** two nodes exchange messages with no IP network between them, on real
radio hardware, with duty-cycle compliance measured.

---

## Phase 10 — Desktop, federation, hardening

Tiered desktop shells, gateways, federation, a security review, and reproducible-build
verification.

---

## Sequencing rationale

Bootable images come before daemons because a daemon that has never run on the target is a
hypothesis. Identity precedes networking because every network operation needs it. The
permission broker precedes the agent because retrofitting a security model onto a working
agent has never once gone well. AI comes before distributed services because it is the
project's headline feature and its hardware assumptions must be validated early, while the
architecture is still cheap to change.

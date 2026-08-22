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

## Phase 1 — Bootable base images ✅ complete

**Goal:** an image that boots, on both architectures, with a boot log to prove it.

1. `10-bootstrap` — minimal rootfs for amd64 and arm64 (arm64 via `binfmt_misc` + `qemu-aarch64-static`).
2. `20-base-config` — systemd, users, fstab, locale, apt pinning.
3. `40-kernel` — kernel, initramfs, arm64 device trees.
4. `50-image` — GPT with ESP + `root_a` + `root_b` + data; GRUB EFI (amd64), U-Boot/EFI (arm64).
5. `60-verify` — QEMU boot to a login prompt, serial captured to a log.
6. `otwono-hwctl` shipped in the image and run at first boot.

**Exit criterion:** `boot.log` for both architectures showing a login prompt, plus the
capability profile recovered from **inside** each VM.

**Met on both architectures.** Each `boot.log` reaches `otwono login:`, and stage 60
recovers `capability-profile.json` from the guest's own data partition, reporting `x86_64`
and `aarch64` respectively. The arm64 image is entirely cross-built on an x86_64 host.
Details, and the five bugs booting it exposed, are in `docs/build/VERIFICATION-LOG.md`.

**Not covered by Phase 1:** no image has run on real hardware, and automatic A/B rollback
is not implemented (both slots exist and are bootable; nothing counts boot attempts yet).

**Risks encountered:** no KVM ⇒ TCG boots take minutes, so the harness now terminates as
soon as its required patterns appear rather than burning the full timeout; Debian mirrors
blocked ⇒ only the Ubuntu recipes are exercised here.

---

## Phase 2 — Service architecture and the permission broker ✅ complete

**Goal:** the Local Control Plane exists and the security model has teeth.

1. `otwono-proto` — JSON-RPC 2.0 / NDJSON over UDS, client and server, `describe`, typed errors.
2. `otwono-hwd` — the first real daemon; publishes the profile, emits hotplug change events.
3. `otwono-permd` — typed actions, policy evaluation, capability tokens, hash-chained audit log.
4. systemd units with the full hardening baseline; Landlock scoping.
5. Integration tests over a temp socket directory; **negative** authorization tests.

**Exit criterion: met, and then some.** 14 integration tests run both daemons over real
Unix sockets: an unauthorized caller is refused, an authorized caller succeeds, and the
hash chain verifies. Beyond the criterion, the same path was exercised on **booted
amd64 and arm64 images** — each guest fetched its profile through permd and hwd at first
boot, and the audit log it wrote was recovered and verified on the host.

**Carried forward (not done):** both daemons run as root; the dedicated Z2/Z3 users and
Landlock scoping listed above are outstanding, and need group-aware socket binding. There
is no confirmation channel yet, so `Ask` fails closed with an error.

---

## Phase 3 — Node identity and secure transport ✅ complete

1. `otwono-idd` — Ed25519 generation, TPM sealing where present, backup/restore, rotation.
2. NodeID encoding, fingerprints, the pairing flow.
3. `otwono-netd` skeleton with the `LinkAdapter` trait and an Ethernet/IP adapter.
4. Noise `XX` over the link; QUIC via rust-libp2p on IP.
5. mDNS discovery on the LAN.

**Exit criterion: met.** `make TARGET=amd64-qemu-ubuntu two-node-test` boots two VMs from
one pristine image onto a segment with no DHCP server; they discover each other over mDNS,
each authenticates the other's NodeID against the key it handshook with, and each holds an
identity it generated itself on first boot. Evidence in `docs/build/VERIFICATION-LOG.md`.

**Carried forward:** no radio link adapter, no routing or store-and-forward, no encrypted
identity backup, and no TPM sealing.

**Closed after Phase 3 (ADR-0010).** `otwono-netd` no longer reads the keystore. The
Ed25519 signing key belongs to `otwono-idd`; the mesh daemon holds only the X25519
agreement key and asks for each session signature over the brokered control plane. Proven
by an integration test that deletes `node.key` from disk and requires the handshake to
succeed anyway. Both daemons still run as root, so the separation is by process and code
path, not yet kernel-enforced.

---

## Phase 4 — Local AI runtime  ← *recommended next milestone*

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

# OTWONO AI OS — Phased Development Roadmap

Each phase has an **exit criterion** that is a verifiable artifact — a test run, a build
log, or a boot log — not an opinion. A phase is not complete until that artifact exists.

Status legend: `SPECIFIED` (design only) · `IMPLEMENTED` (code + unit tests)
· `VERIFIED` (exercised end to end with a log).

---

**Direction not yet scheduled:** `docs/roadmap/CLUSTER-VISION.md` records the cluster,
priority-tier, reach, knowledge-bundle and moderation direction, with the open questions
and the two interactions (ranking against privacy, rewards against concentration) that
need settling before any of it is built.

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
   **Superseded by ADR-0024**: `Ask` now opens a pending confirmation and returns its id, and a second
   socket carries the answer. Still fails closed — nothing is authorised until somebody
   approves — and still inert on the shipped image, where the asker and the confirmer are
   the same uid.

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

## Phase 4 — Local AI runtime  ← *in progress*

1. ~~`otwono-aid` with the backend abstraction and llama.cpp CPU first.~~ **done**
2. ~~Model catalog, signed manifests, content-addressed storage, tier gating.~~ **done**
   (including `ai.models.pull` over `otwono-fetchd`, per ADR-0014)
3. ~~Admission control with a real refusal path — `ModelTooLargeForTier` must be observed.~~
   **done**
4. GPU/NPU backends behind capability detection.
5. Tiered assistant shapes T0–T2.

**Exit criterion:** the same `ai.infer` request served on an amd64 VM and an arm64 VM with
tier-appropriate models, plus a test proving admission control refuses an oversized model
instead of triggering the OOM killer. — **met**, against a synthetic model rather than a
tier-appropriate one; see slice 6 and OQ-6.

**Done so far (slice 1):** the manifest contract and its JSON Schema, the footprint
arithmetic, admission control with every refusal exercised, backend selection as a pure
function, the on-disk catalog, and `otwono-aid` serving `ai.capabilities`,
`ai.models.list` and `ai.admit` over the control plane. Item 3 — the half of the exit
criterion that does not need an engine — is met and tested on both architectures'
fixture profiles.

**Done (slice 2):** manifest signature verification against a `/etc/otwono/publishers.d`
trust store that ships empty, with trusted / unsigned / untrusted-publisher / tampered kept
as four distinct outcomes; and the out-of-process backend supervisor, with its crash, hang,
protocol and flooding paths tested against a fake backend.

**Done (slice 3):** llama.cpp integrated as a supervised adapter process (ADR-0011) and
`ai.infer` implemented. A prompt goes from a control-plane client through the permission
broker, admission control, `otwono_ai::supervisor`, `otwono-llama-backend` and
`llama-server` into a GGUF model and returns generated tokens. Backends are discovered on
disk rather than compiled in, so one build serves a CPU-only Pi and a CUDA workstation.
Build stage 35 produces the engine for either architecture from a pinned upstream tag,
verified against the commit that tag pointed at; it is opt-in (`AI_ENGINE=llama.cpp`)
because the engine is 17 MiB per architecture and a ten-minute build.

**Done (slice 4):** the engine is confined. The adapter applies a Landlock ruleset to
itself before starting `llama-server` (ADR-0012), so the engine may read the model store
and the system libraries and nothing else — in particular not `/var/lib/otwono/identity`.
It fails closed on a kernel that will not enforce it. Both booted images report
`sandbox=full`.

**Done (slice 5):** models can be *installed*, and installing verifies. `ai.models.install`
hashes the weights against the manifest's `blake3` and refuses on mismatch — closing a hole
where a signed manifest plus a swapped blob loaded as trusted — and installs atomically.
`ai.models.verify` re-checks on demand. Guarded by a new `ai.admin` capability, because
changing what a node will run is not the same power as reading its catalog. The full-stack
inference test now installs through this path rather than planting a blob, so the model it
runs is one the daemon verified.

**Done (slice 6):** inference runs on a booted node. An image built with
`AI_SMOKE_MODEL=1` bundles a generated model and a boot unit that installs it over the
control plane and runs a completion; both architectures print
`OTWONO-AI-INFER-OK … tokens=8`. **The exit criterion is met**, with one honest
qualification: the model is synthetic, so this proves the path — daemons, broker,
capability tokens, Landlock, systemd hardening, engine — and not that a 1B model performs
acceptably on a Pi. Tier-appropriate models remain OQ-6.

**Still open:** Also missing:
streaming, PID/mount isolation and a seccomp filter around the engine, **model download
`ai.models.remove`, embeddings, ASR and TTS,
GPU/NPU backends, and the tiered assistant shapes.

---

## Phase 5 — Content store and data visibility

1. ~~`otwono-store` — chunking at ADR-0016's parameters, BLAKE3 content addressing, the
   object model, visibility labels, and the on-disk chunk store with verified reads.~~
   **done** (the daemon, `otwono-stored`, is separate and not yet written)
2. ~~The four labels, enforced at the boundary that matters — `store.serve` refuses
   anything but `PUBLIC` and `REPLICATED`, and refuses identically whether an object is
   private or absent. Encryption at rest, uniformly. Provenance propagation, so derived
   content cannot launder a label. Demotion, which stops future serving and says plainly
   that it recalls nothing.~~ **done**, and per-recipient `SHARED` key wrapping with it —
   ADR-0019, booted: the object is encrypted before it is chunked, the content key is sealed
   once per recipient to a *signed* sharing binding, and the unwrapping key is a third node
   key held by `otwono-idd`. Serving a `SHARED` object to the peers it names works over a
   real link host-side but has not run between two booted nodes. Adding and removing
   recipients (§5) is **done** and booted: each node grants a mesh-learned peer and revokes
   it again, and removing the last recipient is refused rather than costing the owner their
   own file.
3. ~~Egress enforcement in `otwono-netd`, duplicated as defence in depth.~~ **done** —
   ADR-0017 gave the ONM content-fetch protocol; `otwono-netd` calls `store.serve` and
   re-checks the label itself, with deliberately different code (`may_leave_a_node` is an
   allow-list of wire strings, not a call into `otwono-store`).
4. Replication policy for `REPLICATED`. **not started.**
5. ~~The **cluster cache** — a bounded, encrypted, tier-scaled contributed store
   (ADR-0015, `docs/services/CLUSTER-CACHE.md`).~~ **done**, including fan-out fetch
   and ADR-0018's file handoff for objects too large for the control plane. Chunking
   parameters were OQ-16 and are settled by ADR-0016.

**Exit criterion:** the negative test suite from `docs/security/DATA-VISIBILITY.md` §6
passes, including a property test that derived content inherits the most restrictive label.
— **met.** A refusal is indistinguishable from not-found, derived content inherits the most
restrictive label, and demotion stops future serving, all over real sockets with a real
broker. The fourth — "a `PRIVATE` object must never appear on any link" — is proven **on a
link**: two booted VMs on a segment with no DHCP, mutually authenticated over Noise XX, each
refusing the other a `PRIVATE` object it demonstrably holds, with the refusal byte-identical
to the one an absent object gets. See `docs/build/VERIFICATION-LOG.md`.

**Still outstanding here**, and named rather than folded into a later phase: the
`REPLICATED` replication policy (item 4), which does not exist and fails closed. That is the
right way for a feature to be missing but is not the same as being done.

`SHARED` is no longer on that list. It has now passed between two booted nodes end to end:
each seals an object to the other, each asks what it has been sent, and each fetches and
opens it — with no id passing between the machines by any other route (ADR-0020, which
settled the discovery gap ADR-0019 did not anticipate).

One thing about it is still worth carrying forward rather than declaring finished: a
recipient set cannot be changed after the fact, so sharing with somebody new today means
sharing the file again as a new object, and removing somebody does not exist at all.

---

## Phase 6 — First distributed services

Profile site, wiki, and messaging, in that order — they exercise all three primitives
between them. `onm://` addressing, local resolver, browser integration, `Trickle`-safe modes.

**Note on labelling.** Entries in `docs/build/VERIFICATION-LOG.md` titled "Phase 6 slice 1"
through "slice 7" are the cluster cache and the content path, which are Phase 5 item 5
by this document. They are left under their original headings rather than renamed, because a
verification log is a record of what was done and when, and quietly relabelling it would make
it a worse record. Phase 6 as defined here has not started.

**Exit criterion:** a three-node QEMU network where node A's wiki page is readable on node
B, an offline message to node C is delivered when C returns, and a network partition heals
with convergence asserted.

---

## Phase 7 — Agent layer and application integration

Tool registry, first adapters (pandoc, ffmpeg, ImageMagick, mpv, LibreOffice), planner and
executor, snapshot-before-destructive, dry-run mode.

Then the two household applications, which exist to prove the layers beneath them carry real
weight — an offline school and a private ledger are the cases where "local-first" and "the
user owns the keys" stop being slogans:

- **Education** (`docs/services/EDUCATION.md`) — curriculum shared as `PUBLIC` content,
  learner records `PRIVATE` and never replicated, signed transcripts. The accreditation
  question is OQ-18 and is not an engineering task.
- **Finance** (`docs/services/FINANCE.md`) — entirely `PRIVATE`, passphrase-encrypted,
  file-import only until OQ-19 is settled.

Also in this phase, and ahead of both of them in usefulness per unit of effort: a
**companion client** so the node is reachable from the device a person actually carries
(`docs/services/PORTABLE-APPS.md`). A Rust core with a thin shell — Linux and Android
first, Windows next, a PWA for iOS. The application model itself is OQ-20 and needs an ADR;
Apple platforms are OQ-21 and need a decision that is commercial rather than technical.

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

Wi-Fi link adapters in all three roles — `Station`, `AccessPoint` (the node runs `hostapd`
and *is* the network) and `WiFiMesh` (802.11s) — plus Wi-Fi Direct and LoRa, store-and-forward,
the routing decision from OQ-4, duty-cycle enforcement, DTN semantics. The `AccessPoint` role
is what lets a street of nodes reach each other with nobody's uplink involved; chipset and
per-region regulatory support is OQ-15.

**Exit criterion:** two nodes exchange messages with no IP network between them, on real
radio hardware, with duty-cycle compliance measured.

---

## Phase 10 — Desktop, federation, hardening

Tiered desktop shells, gateways, federation, a security review, and reproducible-build
verification.

The desktop is specified in `docs/services/DESKTOP.md`: a tier-selected shell, a
customisable dashboard, the node contribution control (one on/off switch plus storage, RAM,
CPU and GPU sliders), an assistant toggle where *off* means nothing is listening, integrated
VMs through KVM/libvirt, a few built-in games, and the finance surface carrying a crypto
wallet alongside the household's accounts.

Most of its *content* belongs to subsystems specified elsewhere — `FINANCE.md`,
`EDUCATION.md`, `docs/ai/` — and the media applications are integrated, never rewritten
(CLAUDE.md §2.3). What is genuinely new at this phase is the shell, the dashboard and the
contribution control.

**The largest open question in the phase**: which desktop environment or compositor. GNOME,
KDE, a bespoke Wayland shell and wlroots-based options have very different costs at T0, and
the answer needs measurement on real hardware rather than preference. It gets an ADR.

---

## Sequencing rationale

Bootable images come before daemons because a daemon that has never run on the target is a
hypothesis. Identity precedes networking because every network operation needs it. The
permission broker precedes the agent because retrofitting a security model onto a working
agent has never once gone well. AI comes before distributed services because it is the
project's headline feature and its hardware assumptions must be validated early, while the
architecture is still cheap to change.

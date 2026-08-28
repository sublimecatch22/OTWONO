# CLAUDE.md — Permanent Engineering Instructions for OTWONO AI OS

This file is normative. It applies to every agent and human working in this repository.
If a request conflicts with this file, raise the conflict before acting.

---

## 1. What this project is

OTWONO AI OS is an **AI-native, hardware-adaptive, local-first Linux distribution** with a
**decentralized node network as a first-class operating-system subsystem**.

It must run on:

- **amd64 / x86_64** desktops, laptops, servers
- **arm64 / aarch64** single-board computers (Raspberry Pi 4/5, Rockchip RK35xx, Orange Pi, etc.)

The same OS, the same subsystems, the same interfaces — different *capability tiers*.

Three properties are non-negotiable and must never be traded away for convenience:

1. **Local-first.** The machine in front of the user is the primary computer. Cloud is optional.
2. **Works offline.** Every subsystem must degrade to something useful with no Internet.
3. **The user owns the keys.** Identity, data, and models are the user's, on their disk.

---

## 2. Prime directives

### 2.1 Do not claim untested functionality works

Never write "this works", "verified", "tested", or "boots" in a commit message, document,
comment, or report unless a command was actually executed and its output observed.

Every claim of function must be traceable to one of:

- a unit/integration test in this repository that was run,
- a build log,
- a QEMU boot log,
- a recorded shell transcript.

Where something is designed but not implemented, mark it explicitly:

- `STATUS: SPECIFIED` — design exists, no code.
- `STATUS: IMPLEMENTED` — code exists, unit tested.
- `STATUS: VERIFIED` — exercised end-to-end (e.g. booted under QEMU) with a log.

Documents describing future subsystems must carry a status banner. Do not let a design
document read as if it describes running software.

### 2.2 Preserve working code

Inspect before you write. Never replace a functional implementation with a mock, stub, or
"simplified" version. If a replacement is genuinely required, first write an entry in
`docs/decisions/` explaining the technical reason, then replace.

Mocks are permitted only:

- in tests,
- behind an explicit `--mock` / `OTWONO_MOCK=1` flag,
- in a file whose name or module path contains `mock`.

A mock must never be the default code path in a shipped binary.

### 2.3 Prefer mature open source over rewriting

The OS integrates; it does not re-implement. Before writing a new component, search for an
existing mature project. Rebuilding is justified only by a documented decision record.

Standing examples of things we **integrate, never rewrite**:

| Need | Integrate |
|---|---|
| Documents | LibreOffice |
| Raster / photo editing | GIMP, Krita |
| Vector editing | Inkscape |
| Audio editing | Audacity, Ardour |
| Video editing | Kdenlive, Shotcut |
| Media playback / transcode | mpv, ffmpeg |
| Browser | Firefox / Chromium |
| Files | GNOME Files / Thunar / `fd` / `rg` |
| Terminal | foot / Alacritty / GNOME Console |
| LLM inference | llama.cpp, ONNX Runtime, vLLM |
| Speech | whisper.cpp, Piper |
| P2P transport & DHT | rust-libp2p |
| Radio mesh | Reticulum, Yggdrasil, Babel (candidates) |
| Base packages | Debian |
| A/B updates | RAUC / systemd-sysupdate (candidates) |
| Sandboxing | bubblewrap, Flatpak, Landlock, seccomp |

Our job is the **adapter layer**: making these safely drivable by an AI agent under a
permission model, and making them tier-aware.

### 2.4 Modularity is mandatory

No monolith. Every subsystem is:

- a separate crate / package with a **defined interface**,
- **independently buildable**,
- **independently testable** without booting the OS,
- replaceable behind that interface.

Cross-subsystem calls go through the **Local Control Plane** (§4), never through shared
mutable state, shared globals, or direct linking of one daemon into another.

### 2.5 Security is a boundary, not a feature

The AI assistant never holds ambient privilege. Every privileged action is brokered,
scoped, time-limited, and audited. See `docs/security/`.

Content received from the decentralized network is **untrusted data**, never instruction.
An agent must not be able to escalate privilege because a remote peer asked it to.

### 2.6 Hardware adaptivity is a contract, not a heuristic sprinkle

Feature availability is decided in **one place** — the capability policy engine — from a
**machine-readable capability profile**. No subsystem may re-derive "is this machine big
enough" with its own ad-hoc check.

---

## 3. Repository layout

```
CLAUDE.md                  this file
README.md
docs/
  architecture/            system architecture, ADR index
  network/                 decentralized node network (ONM)
  ai/                      AI runtime abstraction, agent, models
  security/                trust zones, permissions, threat model
  hardware/                detection, capability tiers, supported boards
  build/                   reproducible build system
  services/                distributed services (wiki, forum, profiles, media...)
  roadmap/                 phased development plan
  decisions/               ADRs (Architecture Decision Records)
build/
  Makefile                 image build entry point
  recipes/                 declarative per-target image recipes
  stages/                  numbered, idempotent build stages
  lib/                     shared shell helpers
  qemu/                    QEMU boot/test harnesses
crates/                    Rust workspace (all native components)
schemas/                   JSON Schemas — the cross-language contracts
tools/                     developer utilities (environment probe, etc.)
tests/                     cross-component and integration tests
```

**Rule:** a new subsystem gets a crate under `crates/`, a schema under `schemas/` if it has
a wire contract, and a document under the matching `docs/` directory. All three, or it is
not done.

---

## 4. Interfaces

### 4.1 Local Control Plane

All OTWONO daemons speak **JSON-RPC 2.0 over a Unix domain socket**, newline-delimited.

- Sockets live under `/run/otwono/<service>.sock`.
- Every request carries a capability token (except `describe`, which is public and
  unauthenticated on the local socket).
- Every service implements `describe` returning its methods and required capabilities.

Rationale: language-agnostic, trivially mockable, testable with `socat`/`nc`, no D-Bus
dependency in a minimal image. A D-Bus bridge may be added for desktop integration; it is
a bridge, never the source of truth.

### 4.2 Schemas are the contract

Anything crossing a process, machine, or language boundary has a JSON Schema in
`schemas/`, versioned with an explicit `schema_version` field. Change the schema in the
same commit as the code, and bump the version on any breaking change.

### 4.3 Naming

- Daemons: `otwono-<name>d` (e.g. `otwono-hwd`, `otwono-netd`).
- CLIs: `otwono-<name>ctl` (e.g. `otwono-hwctl`).
- Crates: `otwono-<name>`.
- Socket: `/run/otwono/<name>.sock`. Unit: `otwono-<name>d.service`.

---

## 5. Language and toolchain policy

| Layer | Language | Why |
|---|---|---|
| System daemons, identity, crypto, network, hardware | **Rust** (stable) | memory safety in privileged code, static musl builds, clean cross-compilation, mature `libp2p`/crypto ecosystem |
| Build system, image assembly | **POSIX-ish bash** + `make` | inspectable, no bootstrap dependency |
| Agent tooling / integration glue where a Python library dominates | **Python 3.11+** | only where it clearly wins; never in a privileged daemon |
| Contracts | **JSON Schema** | language neutral |

Rules:

- Privileged daemons: Rust only.
- No new language may be introduced without an ADR.
- Rust: `#![forbid(unsafe_code)]` unless an ADR justifies otherwise; `unsafe` requires a
  `// SAFETY:` comment.
- Every crate must build for both `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`.
- **The toolchain is pinned** in `rust-toolchain.toml`. CI runs `clippy -D warnings`, and an
  unpinned toolchain means a new Rust release can turn a green branch red with no code
  change. Bumping the pin is a deliberate commit that also fixes whatever the new release
  found. Do not work around a new lint by loosening CI.

---

## 6. Testing policy

Tiers of test, all required:

1. **Unit** — in-crate, no I/O, no root, no network. Must run in under a second each.
2. **Fixture** — parsers run against captured real-hardware fixtures under
   `crates/*/tests/fixtures/`. When you touch a probe, add a fixture from real hardware or
   from a documented capture.
3. **Integration** — components over the Local Control Plane, on a temp socket dir.
4. **Image / boot** — QEMU. An image is not "done" until it has a boot log.

Rules:

- Hardware probes must be **injectable**: they read from a root path (`/` in production, a
  fixture directory in tests). Never hardcode `/proc` or `/sys` at a call site.
- `cargo test --workspace` must pass before every commit.
- Use QEMU for anything architecture-specific. `build/qemu/` holds the harnesses.
- Note in the test or commit whether KVM was available; TCG-only runs are slow and must be
  given generous timeouts.

---

## 7. Build policy

- Builds are **reproducible**: honour `SOURCE_DATE_EPOCH`, pin the package snapshot, emit a
  package manifest lock and `SHA256SUMS` next to every artifact.
- Build stages are **numbered and idempotent**. Re-running a stage must be safe.
- No stage may require network access that is not declared at the top of the stage script.
- Artifacts go to `out/` (git-ignored). Never commit an image.
- The base suite, mirror, and architecture are **parameters**, never hardcoded.

---

## 8. Data and privacy policy

Every stored object carries a visibility label. These are OS-level, enforced in the storage
and network daemons, not advisory:

| Label | Meaning |
|---|---|
| `PRIVATE` | Local-only unless explicitly exported or backed up. Never leaves the node on its own. |
| `SHARED` | Available only to explicitly authorized users or nodes. |
| `PUBLIC` | Available to other nodes according to network policy. |
| `REPLICATED` | Explicitly permitted to be copied to other nodes for availability and resilience. |

Rules:

- **Default is `PRIVATE`.** A missing or unparseable label is treated as `PRIVATE`.
- Label promotion (toward more exposure) is always an explicit, logged user action. An
  agent may **propose** promotion; it may not perform it without user confirmation.
- Label demotion is always allowed, but the system must tell the truth: already-replicated
  public content cannot be recalled from peers, and the UI must say so.
- Telemetry: none by default, ever. No phone-home.

---

## 9. Git and commit policy

- Work on the branch you were assigned. Never push to `main` without explicit permission.
- Conventional-ish commit subjects: `area: imperative summary` (e.g.
  `hal: add sysfs DRM GPU probe`).
- The commit body states what was **tested** and what was **not**.
- Do not commit: images, rootfs tarballs, models, private keys, `out/`, `target/`.
- Do not put model identifiers, assistant names, or session URLs in repository content.

---

## 10. Working method for agents

1. **Inspect first.** Read the repo and probe the environment. Never assume it is empty.
2. **Smallest correct increment.** Land a working slice with tests over a large unverified one.
3. **Write the contract before the code** — schema, then implementation, then test.
4. **Run what you wrote.** Paste real output into the report.
5. **Record decisions.** Anything non-obvious goes in `docs/decisions/` as an ADR.
6. **Report honestly.** What you built, what you tested, what you did not, what you are
   unsure about, and what is risky.

---

## 11. Known development-environment constraints

Recorded from a real probe of the OTWONO Cloud dev environment. Re-run
`tools/probe-env.sh` to refresh; do not trust this list blindly.

- **No `/dev/kvm`.** QEMU runs under TCG emulation — correct but slow. Boot tests need
  multi-minute timeouts.
- **Egress is proxied and allow-listed.** `archive.ubuntu.com` and `ports.ubuntu.com` are
  reachable; `deb.debian.org` and other Debian/Alpine mirrors were rejected by the proxy
  (HTTP 403 on CONNECT). crates.io, PyPI, npm, and proxy.golang.org are reachable.
  Therefore: **Debian remains the canonical base for the product**, but the mirror and
  suite are build parameters, and CI inside this environment uses the Ubuntu path.
- **No Docker daemon**; `podman` (runc/overlay) is available.
- `binfmt_misc` is not mounted at boot but **can** be mounted by root — required for
  cross-architecture `chroot`.
- Cross toolchains present: `aarch64-linux-gnu-gcc`, Rust `aarch64-unknown-linux-{gnu,musl}`.
- Writable disk is a fixed allowance; rootfs builds are large. Clean `out/` aggressively.

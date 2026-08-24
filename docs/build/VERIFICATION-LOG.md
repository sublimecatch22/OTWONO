# Verification Log

What has actually been executed, and where. CLAUDE.md §2.1 forbids claiming function
without evidence; this file is that evidence for Phase 0.

Everything below was run in the OTWONO Cloud development environment
(Ubuntu 24.04.4, Linux 6.18.44, x86_64, 4 cores, 15 GiB RAM, **no `/dev/kvm`**) on
2026-08-22. Re-run `tools/probe-env.sh` before trusting any of it on a different host.

---

## Rust workspace

Run against the **pinned toolchain** in `rust-toolchain.toml` (Rust 1.97.0). The pin exists
because PR #1's first CI run failed on a clippy `collapsible_match` lint present in 1.97
and absent in the 1.94 this work was developed against — a green local run is only
meaningful if it uses the same compiler CI does.

| Command | Result |
|---|---|
| `cargo test --workspace` | **83 tests pass**, 0 fail (33 capability unit, 29 hal unit, 4 hal fixture, 4 capability fixture, 7 hwctl, 5 schema contract, 1 doc-test) |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo fmt --all --check` | clean |
| `cargo build --workspace --target aarch64-unknown-linux-gnu` | builds |
| `file target/aarch64-unknown-linux-gnu/release/otwono-hwctl` | `ELF 64-bit LSB pie executable, ARM aarch64` |
| `qemu-aarch64-static -L /usr/aarch64-linux-gnu … otwono-hwctl profile --root …/aarch64-rpi5-8gb-synthetic` | **runs and classifies correctly** — the arm64 binary executes, it does not merely link |
| `shellcheck -S warning tools/*.sh build/{stages,qemu,lib}/*.sh` | clean |
| Same suite re-run under Rust 1.94 before the pin | also clean — the 1.94/1.97 clippy divergence was the only difference |

## Hardware detection on the live machine

`otwono-hwctl profile` on the dev VM:

```
  cpu            Intel(R) Xeon(R) Processor @ 2.80GHz — 4 logical / 4 physical
  isa            avx2 avx512f vnni
  memory         15.7 GiB total, 14.9 GiB available
  accelerators   none
  tier           T2_BALANCED
  limited by     accelerator (None < GpuSmall)
  axes           compute=medium memory=high accelerator=none storage=standard
                 network=broadband power=unconstrained
```

The classification is correct and the limiting axis is named, which is the whole point of
the vector design (ADR-0004).

**Fixture round-trip:** `tools/capture-hw-fixture.sh` captured this machine into
`crates/otwono-hal/tests/fixtures/x86_64-cloud-vm/` (68 files), and probing that fixture
produces the same tier, axes, and limiting factor as probing `/`. This is the evidence
that the injectable-root design actually works, rather than merely compiling.

## Build pipeline

| Stage | Target | Result |
|---|---|---|
| `probe` | — | 28 ok, 2 warnings, 0 failures |
| `05-host-tools` | `amd64-qemu` | built and verified `x86-64` |
| `05-host-tools` | `arm64-qemu` | built and verified `ARM aarch64`; the staged release binary was then executed under qemu-user |
| `10-bootstrap` | `amd64-qemu-ubuntu` | **debootstrap succeeded**, 205 MiB rootfs |
| `10-bootstrap` | `arm64-qemu-ubuntu` | **cross-bootstrap succeeded** — `binfmt_misc` mounted and the aarch64 handler registered by the stage, `--foreign` second stage run under `qemu-aarch64-static`; 230 MiB rootfs; `file rootfs/bin/ls` → `ARM aarch64` |
| `10-bootstrap` | `amd64-qemu` (Debian) | **fails early with the intended message** — the egress proxy returns 403 for `deb.debian.org` and the stage names the Ubuntu alternative rather than failing deep inside debootstrap |
| `20-base-config` | `amd64-qemu-ubuntu` | packages installed in the chroot, serial getty enabled |
| `30-otwono` | `amd64-qemu-ubuntu` | binaries, schemas, units installed; `chroot rootfs /usr/bin/otwono-hwctl tier` runs |
| Idempotence | `amd64-qemu-ubuntu` | re-running stage 20 skips with a note; the manifest updates in place rather than appending duplicates |
| `40-kernel`, `50-image` | — | **not implemented**; both fail loudly with the intended plan (Phase 1) |
| `60-verify` | — | harness implemented; no image to boot yet |

Note: `chroot rootfs otwono-hwctl tier` reports `T0_MICRO` because a bare chroot has no
`/proc` or `/sys`. That is the fail-closed behaviour the design requires — an undetectable
machine must classify down, never up.

## Phase 1 — both images boot

**`boot-test` passes for `amd64-qemu-ubuntu` and `arm64-qemu-ubuntu`.** Both images boot
under QEMU (TCG, no KVM) to a login prompt, and the capability profile is recovered
afterwards from the guest's own data partition. amd64 reaches login in roughly 40 seconds
of guest time; arm64 is fully emulated and considerably slower.

| | amd64 | arm64 |
|---|---|---|
| Architecture in profile | `x86_64` | `aarch64` |
| Tier | `T0_MICRO` | `T0_MICRO` |
| Limiting axis | compute (Minimal < Low) | compute (Minimal < Low) |
| Accelerator detected | `simple-framebuffer`, no compute API | none |
| Kernel | 6.8.0-31-generic, 14.9 MiB | 6.8.0-31-generic, 18.2 MiB |
| initramfs | 18.0 MiB | 17.3 MiB |
| Bootloader | `BOOTX64.EFI`, 3.6 MiB | `BOOTAA64.EFI`, 3.5 MiB |
| Image on disk | 420 MiB | 477 MiB |

The arm64 path is entirely cross-built on an x86_64 host: `debootstrap --foreign` plus a
second stage under `qemu-aarch64-static`, apt and `update-initramfs` in the emulated
chroot, and `grub-mkstandalone --format=arm64-efi` run by the target's own grub packages
inside that chroot.

amd64 detail:

```
PASS: every required pattern appeared
  matched: otwono login:
  matched: OTWONO-CAPABILITY-OK
[60-verify] profile recovered from the guest's data partition:
[60-verify]   tier            T0_MICRO
[60-verify]   limiting factor compute (Minimal < Low)
[60-verify]   architecture    x86_64
[60-verify]   axes            compute=minimal memory=low accelerator=igpu
                              storage=constrained network=offline power=unconstrained
```

`T0_MICRO` is the correct answer for the test VM, not a defect: 2 vCPUs classify `compute`
as `minimal`, and the composed tier is the weakest binding axis. Seeing the tiering
mechanism produce a defensible answer on a machine that is genuinely small is the point.

The boot chain observed in the log: GRUB menu with both A/B entries → slot A auto-selected
→ kernel and initramfs loaded from the ESP → `root=LABEL=OTWONO-ROOT-A` → systemd →
`EXT4-fs (vda2): re-mounted r/w` → `otwono login:`.

Artifacts: `out/amd64-qemu-ubuntu/boot.log`, `.../capability-profile.json`,
`.../otwono-amd64-qemu-ubuntu.img` (8 GiB apparent, 419 MiB on disk).

### Bugs this found

Five defects surfaced that no amount of desk-checking would have, listed because each one
says something about where the design was weak.

**1. Kernel with no initramfs.** `update-initramfs: No such file or directory`. The kernel
package only *recommends* `initramfs-tools`, and the build uses `--no-install-recommends`,
so the rootfs got a kernel plus a dangling `/boot/initrd.img` symlink. Stage 40's assertion
that the initramfs exceeds 4 MiB turned this into a clear build-time error rather than an
unbootable image found minutes into a TCG boot.

**2. 467 MiB of firmware for hardware QEMU does not expose.** `linux-firmware` was 60% of a
768 MiB rootfs. `[packages] firmware` is now empty for the QEMU recipes and remains
available per BSP. The rootfs dropped to 322 MiB.

**3. Console output going to a console nobody was reading.** `/dev/console` is whichever
`console=` argument comes *last*, so `console=ttyS0 console=tty0` sent every unit's console
output to the VGA console while the harness captured the serial port. The order is now
`console=tty0 console=ttyS0,115200`.

**4. Persistent state written to the wrong filesystem — the serious one.** The first boot
that passed the console check still wrote `capability-profile.json` to `/var/lib/otwono` on
the **root** partition, not to the mounted data partition. Cause: `nofail` in fstab removes
a mount from `local-fs.target`'s ordering (systemd.mount(5)), so `After=local-fs.target`
no longer guarantees it is mounted; the service won the race and wrote into the directory
underneath the mount point, which the mount then shadowed.

Had this shipped, node state would have lived on the root filesystem — and an A/B update
replaces the root filesystem, so the capability profile and later the **node identity keys**
would be silently destroyed on every update. The fix is `RequiresMountsFor=/var/lib/otwono`,
and every future OTWONO unit that touches persistent state needs it.

This is the case for verifying against the artifact rather than the console: the console
marker said `OTWONO-CAPABILITY-OK` on the broken build. Only reading the file back off the
guest's disk exposed it.

**5. A framebuffer classified as an integrated GPU.** The amd64 guest reports a
`simple-framebuffer` DRM card — the EFI framebuffer handover, which has no shader cores and
no compute API. The probe's rule was "any DRM driver implies Vulkan", so it came back as
`accelerator=igpu`.

This was not QEMU-specific: every UEFI machine with no real graphics driver loaded looks
like this, as does every server with an ASPEED or Matrox BMC display. `otwono-hal` now
carries an explicit display-only driver list, and such devices are still reported —
honestly, with an empty `compute_apis` — but contribute nothing to the accelerator axis.
Re-verified on the target: the amd64 guest now reports `accelerator=none`.

The tier was unaffected (`igpu` is still below `gpu_small`, so nothing was unlocked), but
it would have misled a user and any later logic keyed on `igpu` meaning "try GPU offload".
Only running the probe on a real boot surfaced it — no fixture in the repository contained
a framebuffer-only machine.

### Other corrections made while wiring Phase 1

- `fstab` mounted root `ro`, which cannot boot while `/var` is still on the root
  filesystem. Root is `rw` until the immutable-root work in Phase 8; the partition layout
  is already correct for it.
- The capability-report unit had `PrivateNetwork=yes`. sysfs's net class is namespaced, so
  the probe would have seen only `lo` and reported every machine offline.
- `nofail` on the ESP and data mounts, so a missing partition does not drop the machine
  into emergency mode.

Environment findings that shaped stage 50:

- `losetup --partscan` creates **no partition device nodes** here — there is no udev. Stage
  50 therefore uses no loop devices and no mounts: `mkfs.ext4 -d` populates ext4 from a
  directory and mtools populates FAT, then each filesystem is written into the image at its
  partition offset. Verified in isolation: ownership, modes and contents survive.
- `SOURCE_DATE_EPOCH` is exported as `0` in this environment, which predates the FAT epoch
  (1980) and makes mtools write nonsense dates. The build clamps it, with a warning.

## What has *not* been verified

- **Only QEMU has been booted.** No image has run on real hardware — no Raspberry Pi, no
  Rockchip board, no physical x86 machine. QEMU `virt` supplies its own device tree, so the
  arm64 SBC path (U-Boot, per-board DTBs, vendor kernels) is completely unexercised.
- **Both test VMs classify as `T0_MICRO`**, so no tier above T0 has been exercised on a
  booted system.
- **Automatic A/B rollback is not implemented.** The layout and both menu entries exist and
  slot B is bootable, but nothing counts boot attempts or switches slots yet (Phase 8).
- **The root filesystem is mounted rw.** An immutable root needs `/var` moved off it first;
  a ro root with `/var` still on it does not boot. Phase 8.
- No daemon exists, so nothing about the Local Control Plane, permissions, identity,
  networking, AI runtime, storage, or distributed services has been exercised at all —
  those are all `SPECIFIED`.
- The Debian recipes cannot be bootstrapped in this environment, so only the Ubuntu path
  has been exercised. The Debian path is unverified.
- Two of the three hardware fixtures are **synthetic**, hand-written from published
  specifications, and marked as such in their `capture.json`. Only `x86_64-cloud-vm` is a
  real capture. No probe has run on a real Raspberry Pi, Rockchip board, or GPU machine.
- Reproducibility is designed but not measured: no two-run byte-comparison has been done,
  and snapshot mirror pinning is not wired up.

---

## Phase 2 — the control plane runs on both booted architectures

**`boot-test` passes with the daemons in the image on `amd64-qemu-ubuntu` and
`arm64-qemu-ubuntu`.** Both produce the identical trail below; the arm64 daemons are
cross-compiled on an x86_64 host and run under full emulation.

Both daemons start under the full hardening baseline, and the guest fetches its own
capability profile *through* the control plane at first boot:

```
[  OK  ] Started otwono-permd.service - OTWONO permission broker.
[  OK  ] Started otwono-hwd.service   - OTWONO hardware daemon.
OTWONO-CONTROL-PLANE-OK tier=T0_MICRO audit_records=3
```

Stage 60 then recovers three artifacts from each image and checks them on the host:

| Artifact | Source | amd64 | arm64 |
|---|---|---|---|
| `capability-profile.json` | data partition, local probe | `T0_MICRO`, `x86_64` | `T0_MICRO`, `aarch64` |
| `control-plane-profile.json` | data partition, via permd + hwd | `T0_MICRO` — **matches** | `T0_MICRO` — **matches** |
| `audit.jsonl` | root partition, written by the broker | 3 records, **chain intact** | 3 records, **chain intact** |

Neither boot log contains a `Failed to start .*otwono` line.

The audit trail the guest wrote, verified on the host with `otwono-permd --verify-audit`:

```
seq=1 request         uid:0  hw.read  allow   prev=000000000000… hash=4c30beda0b10…
seq=2 token_issued    uid:0  hw.read  issued  prev=4c30beda0b10… hash=7b2b774b9a9c…
seq=3 token_verified  uid:0  hw.read  valid   prev=7b2b774b9a9c… hash=5e6e6dc245f4…
```

That sequence is the whole point of the phase: a capability was requested, a policy decision
was recorded, a token was issued, and a separate daemon verified it before serving data —
with each record chained to the one before.

### Workspace

| Command | Result |
|---|---|
| `cargo test --workspace` | **153 tests pass** (87 before Phase 2) |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo fmt --all --check` | clean |
| `shellcheck` over `tools/`, `build/{stages,qemu,lib,files}` | clean |

14 of those tests are integration tests running both daemons on real Unix sockets rather
than in-process, because `SO_PEERCRED` is part of what is under test.

### Bugs this found

**6. A shell quoting error that only a boot could surface.** The control-plane check began
life inlined in the unit's `ExecStart`. Through a heredoc *and* systemd's escaping the
quoting broke, and the only symptom was `/bin/sh: 1: Syntax error: Unterminated quoted
string` several minutes into a QEMU run.

It is now a script in `build/files/`, installed by stage 30 and linted by shellcheck in CI.
A quoting error there now fails a fifteen-second job instead of a ten-minute boot. The
general rule this produced: **no non-trivial shell inside a systemd unit** — put it in a
file that the linter can see.

**8. binfmt registration belonged to every chrooting stage, not just the first.** The arm64
run failed in stage 50 with `Exec format error` from `grub-mkstandalone`. Only stage 10
registered the aarch64 handler, and that run skipped stage 10 as already complete;
registration lives in the kernel, so it does not survive a container restart either. Every
partial rebuild of a foreign-arch target hit this. `ensure_foreign_arch_support()` now
lives in `build/lib/common.sh` and is called by all five stages that chroot.

**7. The boot harness burned the full timeout on a missing marker.** A guest sitting at a
login prompt never exits, so a marker that never arrives cost the entire 600s. The harness
now notes when the login prompt appears — systemd is finished by then — and gives up after
a 60s grace, naming the missing markers and any failed OTWONO unit.

### What Phase 2 has *not* verified

- **Both daemons run as root.** The dedicated Z2/Z3 users and Landlock scoping in the
  Phase 2 description are not implemented. User separation needs group-aware socket
  binding so a non-root `otwono-hwd` can reach the broker's socket.
- **No confirmation channel exists**, so an `Ask` decision returns an error. That is
  fail-closed and correct for now, but it means the confirmation path itself is untested
  beyond the unit level.
- **The audit chain is tamper-evident, not tamper-proof.** An attacker who can rewrite the
  whole file can recompute every hash. Anchoring the chain head somewhere the writer cannot
  reach is Phase 3 work.
- Only one tier (`T0_MICRO`) and one policy shape have been exercised on a booted system.
  Both QEMU guests classify identically, so the tiering logic itself is still only covered
  by fixtures.

---

## Phase 3 — two nodes form a mesh

**`make -C build TARGET=amd64-qemu-ubuntu two-node-test` passes.**

Two VMs booted from one pristine image onto a private QEMU socket segment — no host
bridge, no root, and deliberately no DHCP server — discovered each other over mDNS and
mutually authenticated:

```
node A identity: otw1:cdbt-mhkj-1cn7-90xn  addr 169.254.45.141/16   connected 1
node B identity: otw1:38ch-v2b2-zc2f-9mg9  addr 169.254.158.157/16  connected 1
PASS: two nodes discovered and mutually authenticated
```

Each node generated its own identity on first boot (a re-run produced a different pair,
confirming it is not baked into the image), each obtained a distinct IPv4 link-local
address with no DHCP server present, and each authenticated the other's NodeID against the
key it handshook with.

### Workspace

| Command | Result |
|---|---|
| `cargo test --workspace` | **239 tests pass** (153 before Phase 3) |
| `clippy -D warnings`, `fmt --check`, `shellcheck` | clean |

### The defect this found

**9. The boot test baked a private node key into the release image.** Stage 60 booted the
artifact in place. A guest writes its first-boot state to its own disk, so the boot
generated a node identity and left the private key inside the file every device would be
flashed from. Confirmed by extracting the data partition: `/identity/node.key` with a
usable ed25519 seed, plus both first-boot profiles, plus the audit log on the root
partition.

Every device flashed from such an image would share one NodeID and one private key, and any
of them could impersonate any other. It also invalidated the `SHA256SUMS` written in stage
50, because the file changed after it was checksummed — and stage 60 verified the checksum
only *before* booting, so nothing noticed.

It surfaced because both VMs reported the same fingerprint and each then skipped the
other's mDNS advertisement as its own broadcast. Nothing short of booting two nodes from
one image would have shown it.

Fixed three ways: stage 60 boots a copy; the artifact is checked afterwards for any
first-boot residue and the checksum re-verified; and the two-node harness refuses a source
image that already contains an identity, failing in about a second instead of timing out.

### What the harness cost

Of six failures in the two-node test, five were the harness rather than the system:
`pipefail` killing the script twice (firmware detection, then `mesh_field`), waiting for
`otwono-netd` lines that only ever reach the journal, matching a truncated marker prefix on
a serial console that flushes mid-line, and — the interesting one — QEMU giving both guests
the same default MAC, so IPv4 link-local derived the same address for both and duplicate
detection could not help, since each node saw only its own ARP probe.

Worth recording because the ratio is the point: an integration test at this level spends
most of its failures on itself before it earns the one real finding.

### What Phase 3 has *not* verified

- **No radio.** Only TCP over IP. The `LinkAdapter` interface exists and LoRa's constraints
  are modelled and unit-tested, but no non-IP adapter has been written.
- **No routing and no store-and-forward.** Peers connect directly or not at all.
- **`otwono-netd` reads the keystore directly**, so two processes can reach the node's
  private keys. The split — `otwono-idd` holding the Ed25519 key, `netd` holding only the
  agreement key and asking `idd` to sign each session proof — is not done. *(Closed after
  Phase 3; see "Phase 3.5" below.)*
- **No encrypted identity backup, no TPM sealing, no revocation records.**
- **Only two nodes, on one segment, on amd64.** No partition-and-heal test, no arm64 run of
  the two-node test, nothing on real hardware.

---

## Phase 3.5 — the signing key leaves the network daemon (ADR-0010)

`otwono-netd` no longer opens `node.key`. It holds only the X25519 agreement key, registers
that key with `otwono-idd` at startup, and asks for one brokered signature per handshake.

### The mesh still forms

`make -C build TARGET=amd64-qemu-ubuntu two-node-test`:

```
node A identity: otw1:qpqb-a5gz-d456-9cxh   addr 169.254.156.119/16   connected 1
node B identity: otw1:rphd-9vmg-ehfv-kmnf   addr 169.254.139.34/16    connected 1
PASS: two nodes discovered and mutually authenticated
```

`make -C build TARGET=amd64-qemu-ubuntu verify` also passes, including the new pristine
check: *no identity, profiles, audit log or seeded machine-id in the artifact*, and
`otwono-amd64-qemu-ubuntu.img: OK` against the checksum stage 50 recorded.

### Workspace

| Command | Result |
|---|---|
| `cargo test --workspace` | **268 tests pass** (239 before) |
| `clippy -D warnings`, `fmt --check`, `shellcheck -S warning` | clean |

### Proving the boundary rather than asserting it

`tests/control-plane/tests/key_separation.rs` runs the real daemons over real sockets.
The decisive test **deletes `node.key` from disk** after both mesh daemons have bound, then
requires the handshake to succeed anyway.

File permissions were the obvious lever and are the wrong one here: CI runs as root, root
ignores mode bits, and a `chmod 000` test would have passed whether or not `otwono-netd`
read the key. Deleting the file is decisive for any uid.

The suite also pins the two halves against each other — a caller with `id.sign_session` and
no agreement secret cannot handshake, and a caller with the agreement secret and no
`id.sign_session` cannot either — and checks that `id.sign_session` refuses any payload
that is not a 32-byte handshake hash, so the oracle cannot be steered.

### Two defects this found

**10. The image shipped a seeded `/etc/machine-id`.** debootstrap leaves a concrete value
behind and nothing cleared it, so every device flashed from the image was, to systemd, the
same machine. systemd derives per-host secrets from it — including the IPv4 link-local
address — so both VMs came up on `169.254.158.157/16` and could not reach each other on a
segment with no DHCP server. It is also the DHCP DUID and the journal's host identity.

Same class as the private-node-key defect from Phase 3: one value that must be per-device,
baked into a distributable artifact. Stage 50 now truncates the file immediately before the
filesystem is sealed — systemd's documented "generate one at first boot" marker — and stage
60 refuses an image whose `/etc/machine-id` is non-empty.

Worth noting that the Phase 3 run passed *despite* this, on distinct addresses. The bug was
already present; that run got lucky.

**11. A failed dial was never retried.** mDNS delivers `ServiceResolved` once per
resolution, and the discovery loop only dialled on that event. A dial that lost a startup
race — the peer's listener not yet bound, an address still settling — was permanent. Both
nodes then sat at `known=1 connected=0` indefinitely, having found each other and connected
to nothing.

This is a real robustness defect that the key split *exposed* rather than caused: the extra
startup round trips shifted the timing enough to lose the race that had previously been won.
`run_discovery` now sweeps known, unconnected, addressable peers every 30 seconds when no
new advertisement arrives.

### Diagnosis cost, and what fixed it

The failure looked identical to a permission denial, and the reason was only in the journal
— invisible on a serial console. Two things resolved it, and both are the pattern that has
worked every time in this project:

1. **Reproduce on the host first.** A new integration test with *two independent* brokered
   nodes — the case the single-harness tests missed — passed immediately. That ruled out the
   handshake logic in seconds instead of a 15-minute QEMU cycle, and pointed at the
   environment.
2. **Put the reason where it can be seen.** `otwono-netd --peers` prints the peer table with
   each peer's last error, and the boot-time mesh check now runs it whenever peers are known
   but none are connected. The very first run with it printed the answer.

### What Phase 3.5 has *not* changed

- **Every daemon still runs as root.** The separation is by process and code path, not by
  the kernel. `node.key` at 0600 stops another *user*, not another root process. Finishing
  this needs the Z2/Z3 user separation and Landlock work, which is not done.
- **No TPM sealing, no encrypted backup, no revocation records.** Unchanged.
- **arm64 untested for this change.** The workspace builds for it; the two-node test has
  only been run on amd64.
- **Nothing on real hardware.**

---

## Phase 4 slice 1 — admission control, on both architectures

The half of the Phase 4 exit criterion that does not need an inference engine: a model
catalog, a manifest contract, and a refusal path that is exercised rather than assumed.
**No engine is linked, and `ai.infer` says so.**

### Boot verification, both targets

| Target | Result |
|---|---|
| `make TARGET=amd64-qemu-ubuntu verify` | PASS |
| `make TARGET=arm64-qemu-ubuntu verify` | PASS |
| `make TARGET=amd64-qemu-ubuntu two-node-test` | PASS |

Both guests emit, from `otwono-aid` running under its own systemd unit:

```
OTWONO-AI-OK tier=T0_MICRO local_inference=unavailable models=0
```

`local_inference=unavailable` is the honest state of every build today, and it is asserted
at boot rather than discovered at first use. The audit record count per boot rose from 3 to
6, which is the visible trace of `otwono-aid` asking the broker for `hw.read` — the
capability tier reaching it through the control plane rather than a second probe.

The mesh is unaffected:

```
node A identity: otw1:y71d-a858-4h1d-vesa  addr 169.254.33.121/16   connected 1
node B identity: otw1:d8na-bsev-87xh-jnm1  addr 169.254.156.92/16   connected 1
PASS: two nodes discovered and mutually authenticated
```

### Workspace

| Command | Result |
|---|---|
| `cargo test --workspace` | **332 tests pass** (268 before) |
| `clippy -D warnings`, `fmt --check`, `shellcheck -S warning` | clean |

### Three defects this found

**12. `otwono-aid` was not staged into the image.** Stage 05 carries an explicit binary
list and the new daemon was not on it, so the unit existed and the binary did not. Caught
by requiring `OTWONO-AI-OK` in the boot harness before believing the daemon worked — the
same discipline that has caught every other "it builds, therefore it runs" assumption in
this project.

**13. The AI daemon would have computed a different tier than the hardware daemon.** It
probed hardware itself, and its unit runs with `PrivateNetwork=yes` — so it would have
classified the network axis from an empty namespace and disagreed with `otwono-hwd` about
the same machine. CLAUDE.md §2.6 puts that decision in one place; two processes deriving it
independently is exactly how they drift.

Fixed by having `otwono-aid` fetch the profile from `otwono-hwd` over the brokered control
plane and fail closed if it cannot. That also *keeps* `PrivateNetwork=yes` valid for the
daemon, which is a security improvement rather than a concession: it now reads no hardware
and downloads nothing.

**14. Artifact extraction could not read a dirty filesystem.** The boot harness kills QEMU
rather than shutting the guest down, so every partition the guest wrote is left dirty.
`debugfs` on a dirty ext4 fails outright with a bitmap checksum mismatch instead of reading
around it. This had been latent: it only surfaced once `otwono-aid` began creating its
catalog directories at startup, leaving more uncommitted metadata at the moment of the
kill. Stage 60 now replays the journal with `e2fsck -fy` on the extracted *copy* — the same
recovery a real machine performs after a hard power-off. The pristine-artifact check
deliberately does **not** fsck: if the release image ever needs a journal replay, something
wrote to it, and that is the defect that check exists to catch.

### What slice 1 has *not* done

- **No inference.** No engine is linked, `installed_backends()` is empty, and `ai.infer`
  returns `NoBackendAvailable`. Nothing here has run a model. The `ai.infer` half of the
  Phase 4 exit criterion is not met.
- **Signatures are carried, not verified.** A manifest's `signature` field is parsed and
  its *absence* is enforced (unsigned models need an explicit opt-in, and an unsigned
  tool-capable model is flagged as executable content). The signature itself is not checked
  against a publisher key.
- **No model download, no embeddings, ASR, TTS or vision, no sessions, no GPU/NPU backend,
  no tiered assistant shapes, no remote inference over ONM.**
- **Admission is tested against fixture profiles, not against real memory pressure.** The
  arithmetic and every refusal are unit-tested; no machine has yet been pushed to the point
  where the reserve is what saved it.

---

## Phase 4 slice 2 — provenance, and a backend that cannot hang the daemon

Two things that had to be in place *before* an inference engine, not after: the check that
decides whether a model may be loaded at all, and the supervision contract the engine will
run behind.

### Boot verification, both targets

| Target | Result |
|---|---|
| `make TARGET=amd64-qemu-ubuntu verify` | PASS |
| `make TARGET=arm64-qemu-ubuntu verify` | PASS |

```
OTWONO-AI-OK tier=T0_MICRO local_inference=unavailable models=0 publishers=0
```

`publishers=0` is the shipped default and is deliberate. No publisher key is baked into the
image: shipping one would mean every OTWONO node automatically trusts whoever holds it,
which is the node operator's decision and not the image builder's.

### Workspace

| Command | Result |
|---|---|
| `cargo test --workspace` | **365 tests pass** (332 before) |
| `clippy -D warnings`, `fmt --check`, `shellcheck -S warning` | clean |

### Four provenance outcomes, not two

| Outcome | Loadable |
|---|---|
| Signature verifies, publisher trusted | yes |
| No signature | only with an explicit opt-in |
| Signature verifies, publisher unknown | only with an explicit opt-in |
| **Signature does not verify** | **never** |

The last row is the point. `allow_unsigned` means *"I know where this came from"*; it must
never silently cover *"somebody changed this in transit"*. There is a test asserting a
tampered manifest is refused with `allow_unsigned` both false and true.

The canonicalizer is written out rather than delegated to `serde_json`'s key ordering.
`serde_json` sorts keys today only because `preserve_order` is off, and any transitive
dependency can turn that on. The meaning of every signature must not depend on a feature
flag nobody is watching.

### Supervision, proved against a backend that misbehaves to order

Thirteen tests run a shell script as the "backend" and make it hang, die with a status,
die by signal, emit garbage instead of a hello, claim a future protocol version, flood a
single line past the cap, and answer non-JSON. A process boundary is the whole subject, so
an in-process double would have proved nothing — it cannot hang in a blocking read or be
killed by a signal.

### Two defects this found, both in code written the same hour

**15. The supervisor could hang inside the code meant to prevent hanging.** The first
version spawned a reader thread per read and joined it. On timeout it killed the child —
but the fake backend's `sleep` grandchild survived, inherited stdout, and held the pipe
open, so the join never returned. The test suite wedged, and three `sleep 600` processes
were still running afterwards.

Fixed two ways, both necessary. One long-lived reader thread feeding a channel, so nothing
is ever joined; and `process_group(0)` at spawn with a group-wide kill on terminate, so the
whole backend subtree dies. The second matters beyond the test: a real backend is a wrapper
script around an engine, and killing only the wrapper leaves the engine holding gigabytes.

**16. A zombie is not a running process, and the test could not tell.** The subtree-kill
test checked `/proc/<pid>` for existence. When the wrapper dies its children are reparented,
and whether they are reaped promptly is up to whatever PID 1 is — inside a container, often
nothing. The `/proc` entry lingers on a killed process, so the test flaked. It now reads the
process state and treats `Z` as dead, which is the honest reading: the property under test
is that the process was killed, not that somebody collected its exit status.

Both were caught by tests written in the same change as the code, which is the only reason
they were caught at all — neither would have shown up until a real engine hung in
production.

### One thing worth recording about the test fixtures

Turning on verification broke the integration fixtures, which carried a placeholder
`"AA=="` signature. That is the correct failure: a test that pastes a fake signature stops
proving anything the moment verification joins the path. They now sign for real, via a
`testing` feature on `otwono-ai`, with two publisher keys so *trusted* and *unknown signer*
are both reachable.

### What slice 2 has *not* done

- **Still no inference.** `installed_backends()` is empty and `ai.infer` refuses. The
  supervisor has never run an inference engine — only shell scripts pretending to be one.
- **The `hello` protocol is asserted, not negotiated.** One version, no capability
  exchange, no streaming. Streaming is what real inference needs and it is not here.
- **No publisher key is distributed.** The trust store works; nothing populates it.
- **Signature verification has not been exercised on a booted node with a real signed
  model,** because there are no models. Only `publishers=0` has been observed on hardware.

---

## Phase 4 slice 3 — llama.cpp, actually running

A prompt now goes from a control-plane client, through the permission broker and admission
control, into a real inference engine, and comes back as generated tokens. The slice is
the integration itself: no engine was written, and none was linked.

### The shape, and why it has three processes

```
otwono-aid  ──NDJSON JSON-RPC on stdio──▶  otwono-llama-backend  ──HTTP over a
 (daemon)      (otwono_ai::supervisor)         (otwono-llama)        Unix socket──▶  llama-server
```

Each boundary earns its place, and ADR-0011 records the alternatives that were rejected:

- **The daemon links no engine.** FFI into `libllama` would put `unsafe` in a privileged
  daemon and make `cargo test --workspace` need a C++ toolchain and an engine build on
  every machine — the whole workspace behind its slowest dependency.
- **A Unix socket, not a loopback TCP port.** `llama-server` has no authentication, so a
  port on `127.0.0.1` would let any local account drive the model and read what is in
  flight. A socket in a `0700` directory is protected by the filesystem.
- **An adapter process rather than HTTP in the daemon.** llama.cpp is one backend of
  several and the others look nothing like it — whisper.cpp has no server, Piper reads
  stdin. The daemon learns one dialect; adapters absorb the differences.

### Verified end to end, against a real engine

Not a mock anywhere in the chain. `tests/control-plane/tests/ai_infer_llama.rs` stands up
the permission broker and `otwono-aid` on real sockets, with a filesystem laid out like an
installed node, and asks for a completion:

```
test a_prompt_goes_all_the_way_to_the_model_and_back ... completion: "thkIth}M of.Ebq"
ok
test result: ok. 6 passed; 0 failed
```

The text is gibberish because the model's weights are random, and that is deliberate — see
below. What the tests assert is that real work happened and was accounted for: tokens
predicted, prompt evaluated, the right stop reason, one engine process and not two, no
engine left running afterwards.

Two of those six deserve naming:

- **The context window the engine gets is the one admission control granted.** The test
  reads the engine's own `/proc/<pid>/cmdline` and checks `--ctx-size 256`. This is the
  join most likely to rot silently: if the number admission control computed against the
  node's memory budget never reaches the engine, the whole calculation is decoration and
  everything still appears to work.
- **The engine does not outlive the daemon.** Otherwise a node fills up with orphaned
  engines holding a model each.

### There is no model to test with, so one is generated

Downloading a published model is not available here — the egress allow-list has no model
host — and a 2 GB download is a poor test dependency anywhere. So
`tools/make-tiny-gguf.py` synthesizes one: a genuine GGUF with the tensors, tokenizer and
metadata llama.cpp requires for `general.architecture = llama`, at 386 KB.

The vocabulary is printable ASCII with **no byte-fallback tokens**, and that detail was
learned the hard way. The first version had the usual 256 `<0xXX>` tokens; a model with
random weights samples uniformly, promptly emitted bytes that were not valid UTF-8, and
llama.cpp's response parser returned a 500. That looks exactly like an integration bug and
is not one. Restricting the vocabulary makes every possible output valid UTF-8 by
construction, so the only thing left that can fail the test is the thing under test.

This proves the integration and says nothing about output quality. A test asserting on the
*text* would be asserting on one model's behaviour, which is not what is being integrated.

### Both architectures, and the same tokens from each

The arm64 engine is cross-compiled, so "it is an ELF for aarch64" is a weak claim. It was
run:

| Engine | How | Result |
|---|---|---|
| x86-64, native | directly | 7/7 end-to-end tests pass |
| aarch64, cross-compiled | `qemu-aarch64-static`, via a wrapper script | 7/7 pass |

Both produced `thkIth}M of.Ebq` for the same prompt and seed. Identical output across two
architectures is a stronger statement than either run alone.

The wrapper script is also the shape the supervisor was built for — "a backend is
realistically a wrapper script around an engine" — so the subtree-kill path was exercised
for real rather than against a stand-in.

### Boot verification

| Target | Result | Boot line |
|---|---|---|
| `amd64-qemu-ubuntu`, `AI_ENGINE=llama.cpp` | PASS | `OTWONO-AI-OK tier=T0_MICRO local_inference=available backends=llama-cpp-cpu models=0 publishers=0` |
| `arm64-qemu-ubuntu`, `AI_ENGINE=llama.cpp` | PASS | `OTWONO-AI-OK tier=T0_MICRO local_inference=available backends=llama-cpp-cpu models=0 publishers=0` |
| `amd64-qemu-ubuntu`, default (no engine) | PASS | `OTWONO-AI-OK tier=T0_MICRO local_inference=unavailable backends=none models=0 publishers=0` |

The third row is not a formality. The boot check's logic changed in this slice, so the
path a stock image takes was re-verified from a clean tree — a rebuild in place would have
tested "no engine requested" against a rootfs that still had one from the previous run.

`models=0` is the honest state: the engine is installed and discovered, and there is
nothing for it to run because no model ships in an image and no download exists.
**Inference has not been performed on a booted node** — only on the host, against the
engines those images contain.

The boot check now cross-checks two figures that come from the same probe: if the backend
list and the `local_inference` flag disagree, the node is lying about itself and the check
fails. Backends are discovered on disk, not compiled in, so one build serves a CPU-only Pi
and a CUDA workstation and `ai.capabilities` describes the machine.

### Workspace

| Command | Result |
|---|---|
| `cargo test --workspace` | **434 tests pass** (365 before) |
| `clippy -D warnings`, `fmt --check`, `shellcheck -S warning` | clean |

### Four defects found by the tests that were written for them

**17. The engine was put in its own process group, which is precisely wrong.** It reads as
the tidy thing to do and it defeats the mechanism it looks like it supports: the supervisor
kills the *adapter's* process group, so an engine in its own group is outside the blast
radius. `Drop` does not save you either — it does not run on SIGKILL. The end-to-end test
caught it immediately: `llama-server 19627 survived the adapter it was started by`. The
engine now inherits the adapter's group, and the adapter signals the engine by pid.

**18. The stop reason was read from fields the engine no longer sends.** The mapping was
written against `stopped_eos` / `stopped_word` / `stopped_limit`; the engine reports a
single `stop_type` string. Every completion came back as `Other` — with the text intact,
which is what makes it dangerous: it looks like working software. Both shapes are now read,
newest first, because an engine upgrade must not be able to silently degrade a field.

**19. A build log redirected through a directory that did not exist yet.** Stage 35 wrote
cmake's log to `"$BUILD/../cmake.log"` before creating `$BUILD`, so the redirect failed
before cmake ran — and the error message named a path with `/../` in it, which is nobody's
idea of a clue. Only caught by running the stage's clone-and-build path from scratch rather
than against the cache I had already warmed by hand.

**20. A fast-failing engine's last words were lost to a race — found by CI, not by me.**
`stderr_tail()` read whatever the reader thread had accumulated so far. A dead child and a
fully-drained pipe are different events, and an engine that cannot load a model says why
and exits within milliseconds, so `try_wait` reported the exit while the explanation was
still in the pipe. The caller got `""` — the diagnosis lost in exactly the case it is
collected for.

It passed here every time and failed on a GitHub runner, which is the whole lesson: an
idle four-core box is not a schedule, it is one schedule. The reader now sets a flag at
EOF and `stderr_tail()` waits for it, bounded at two seconds so an engine that left a child
holding the pipe cannot hang the error path. The timeout branch was reordered to kill
first and read after, for the same reason — a complete tail beats whatever arrived by the
deadline.

The regression test makes the race deterministic instead of hoping for it: the fake engine
writes 200 lines and exits at once, and the assertion is on the *last* line. Re-run 12
times with eight spinners saturating the CPU: 0 failures.

### Reproducibility: measured, and a correction

CLAUDE.md §7 requires reproducible builds, so this was tested rather than assumed.

One real defect was found and fixed. **The bundled browser chat UI is embedded in the
binary, and its asset filenames are content hashes that change between builds** — 2.6 MB
of a diff between two builds of the same commit, confirmed by reading the differing region
of `.rodata` and finding `bundle.DUYuKz9T.js` against `bundle.diFjY-ze.js`. It is now built
with `LLAMA_BUILD_UI=OFF`, which is right for three separate reasons: the engine is already
started with `--no-webui`, an unauthenticated HTTP surface should not ship unused, and the
binary lost 3.2 MB (17.3 MB → 14.1 MB). `-ffile-prefix-map` was added in the same pass so
the artifact does not depend on where it was compiled.

**The second finding was wrong, and it was mine.** This log previously recorded that a
source of nondeterminism remained after those fixes. It does not. That conclusion came
from comparing against a second build whose source tree I had produced with `cp -r`, and a
`cp -r` copy — byte-identical in content and identical in file modes — deterministically
yields a *different* engine binary. I measured my own test harness and reported it as a
property of the build.

The corrected result, from five builds:

| Build | Source | Result |
|---|---|---|
| 1 | the canonical checkout | `ab5adad…` |
| 2 | same checkout, different build directory | `ab5adad…` |
| 3 | `cp -a` copy at a different path | `ab5adad…` |
| 4 | fresh `git clone` of the pinned tag | `ab5adad…` |
| 5 | second fresh `git clone`, via the new check | `ab5adad…` |

So: **the engine build is reproducible on this host across independent clones**, which is
the property that matters, since a clone is how anyone else obtains the source. Note the
scope — all five ran on one machine. Cross-host reproducibility is untested and remains a
1.x goal (`docs/build/BUILD-SYSTEM.md` §4).

`tools/check-engine-reproducibility.sh` now makes this repeatable rather than a thing
somebody checked once:

```
$ make -C build TARGET=amd64-qemu-ubuntu engine-repro-check
  build one: ab5adadd37c06ffb4036eb60218421cf9d9d3a73fb0f0481efef4a28aa92088e
  build two: ab5adadd37c06ffb4036eb60218421cf9d9d3a73fb0f0481efef4a28aa92088e
REPRODUCIBLE
```

It clones twice on purpose. Two builds from one checkout is the easy case and would have
passed throughout — including while the web UI was still making the binary
irreproducible — so it would have proved nothing.

**Left open: why a `cp -r` copy builds differently.** It is reproducible in its own right —
two `cp -r` copies at different paths both produced `da02b5b…` — so there is a real cause
and not a flake. The copies differ from the original in no file content and no file mode;
only in timestamps and directory layout. But a fresh `git clone` also has fresh timestamps
and fresh directories, and *that* reproduces, so timestamps alone do not explain it. The
differing bytes are in `.text`, `.rodata` and `.data.rel.ro` while `.eh_frame` is
identical, and no meaningful string differs — the signature of addresses being assigned
differently, not of content changing. Recorded rather than chased: `cp -r` is on nobody's
path to this source, and each hypothesis costs a fifteen-minute build. It is a curiosity
with evidence attached, not a known defect in what ships.

### What this slice does not do

- **No streaming.** One request, one response. Interactive use wants tokens as they are
  produced, which needs several frames per request *and* a control plane that can carry
  them onward. `llama-server` can stream; the gap is ours.
- **No sandbox around the engine.** It is a large C++ program parsing untrusted model files,
  running with the adapter's privileges, confined only by `otwono-aid.service`'s hardening
  and a new `MemoryMax=80%` cgroup cap. bubblewrap or Landlock is the right answer and is
  not written.
- **No model distribution**, so `ai.models.pull` is still absent and no booted node has a
  model.
- **No GPU variants.** Discovery has directories for `vulkan`, `cuda` and `rocm` and
  selection already prefers them correctly, but no build stage produces them.
- **Phase 4's exit criterion is not met.** It asks for the same `ai.infer` served on an
  amd64 *and* an arm64 VM with tier-appropriate models. Inference has run on the host
  against both architectures' engines; it has not run on a booted VM, because there is no
  model on one.
- **The arm64 engine has not been through the reproducibility check.** The check accepts
  `--arch arm64` and the cross build works, but only amd64 has actually been run twice.

---

## Phase 4 slice 4 — the engine runs confined

ADR-0011 bounded the blast radius of an engine *crash*. This bounds the blast radius of an
engine *compromise*, which is a different problem and, with `ai.models.pull` coming, the
more urgent one: `llama-server` exists to parse binary files that came from somewhere else,
and it was running with reach into `/var/lib/otwono/identity` — the node's Ed25519 private
key.

The adapter now applies a Landlock ruleset **to itself** before it ever starts an engine.
Landlock is inherited and cannot be undone, so the engine is confined by construction
rather than by remembering to confine it.

### Why the adapter restricts itself rather than the child

The obvious place is between fork and exec, which in Rust means `Command::pre_exec` — and
that is `unsafe`. Putting `unsafe` into the process that handles untrusted model files, in
order to make it safer, is a poor trade. Restricting at startup avoids it entirely and
confines the adapter too, which is strictly better: the adapter has no more business
reading the node's private key than the engine does.

The cost is that the policy must be fixed before any `backend.load` names a model, which is
why the adapter is given a *model directory* and `--model-dir` is required rather than
defaulted. A default would be a boundary nobody chose.

### Verified on the kernel that ships, because the dev kernel has no Landlock

This is the part worth being precise about. The OTWONO dev environment's kernel returns
`ENOSYS` for `landlock_create_ruleset` — `securityfs` is not even mounted — so **enforcement
cannot be demonstrated where this was written.** The images build their own kernel, so
verification moved into QEMU:

| Target | Boot line |
|---|---|
| `amd64-qemu-ubuntu`, `AI_ENGINE=llama.cpp` | `OTWONO-AI-OK … backends=llama-cpp-cpu sandbox=full models=0 publishers=0` |
| `arm64-qemu-ubuntu`, `AI_ENGINE=llama.cpp` | `OTWONO-AI-OK … backends=llama-cpp-cpu sandbox=full models=0 publishers=0` |

`sandbox=full` is the kernel's own answer, not ours: the boot check runs the adapter's
`--probe`, which applies a ruleset and reports what was actually enforced.

On the host, the confinement test **skips and says why** rather than passing quietly:

```
test the_running_engine_is_confined_by_the_kernel_not_only_by_the_adapter ...
SKIPPED: this kernel does not enforce Landlock, so confinement cannot be demonstrated here.
```

### What the policy allows, and the point of what it does not

Read-only: the engine's own directory, the system library paths, `/proc`, `/sys`, the model
store. Writable: the runtime directory, and only that. Everything else is denied — and the
list of "everything else" is the deliverable: `/var/lib/otwono/identity`, `/var/log/otwono`,
`/etc/otwono`, `/root`, `/home`. There is a unit test that names each of those and fails if
any ever appears in the rule table.

The model store is readable and **not** writable, which matters more than it looks: an
engine that could write the blob store could replace a signature-verified model with its
own, and the next load would trust it.

### Fails closed

On a kernel that will not enforce Landlock the adapter refuses to start. `--allow-unconfined`
overrides it, explicitly, with a warning on stderr at every start rather than once at
install. A security boundary that silently degrades is not one.

### Workspace

| Command | Result |
|---|---|
| `cargo test --workspace` | **443 tests pass** (434 before) |
| `clippy -D warnings`, `fmt --check`, `shellcheck -S warning` | clean |

### One defect, and it was in the safety check itself

**21. The availability probe answered "available" on a kernel with no Landlock at all.**
The first version built a ruleset and treated success as support. Under the crate's
best-effort compatibility mode, `create()` succeeds on a kernel whose
`landlock_create_ruleset` returns `ENOSYS`; only `restrict_self` reveals that nothing was
enforced. So the probe printed `landlock=available` on the very kernel that had just
refused to confine anything — the wrong answer for the input that a fail-closed decision
rests on, which is the worst place to be wrong.

The crate keeps its own runtime ABI query private, deliberately, on the reasoning that
policies should not vary with the kernel underneath them. So the probe now gets the
authoritative answer the only way available: it *applies* a ruleset and reports what the
kernel enforced, in a process that exits immediately afterwards. The tests ask the adapter
rather than probing in-process, because probing in-process would confine the test runner
for every test after it.

### What this does not do

- **No PID or mount namespace, and no seccomp filter.** A compromised engine can still burn
  CPU and see what any process sees. bubblewrap would cover this; it costs a base-image
  dependency and a fourth process in the chain, and is the obvious next step if it becomes
  necessary.
- **`/proc` and `/sys` are readable**, because ggml's CPU detection needs them. That is a
  real widening and is stated in ADR-0012 rather than buried.
- **Landlock governs new opens, not descriptors already open** when it is applied.
- **The engine has still never been compromised in a test.** What is verified is that the
  boundary exists and that the kernel enforces it, not that it survives an actual exploit.

---

## Phase 4 slice 5 — a model can be installed, and installing verifies

### Scope, stated up front

This slice was started as `ai.models.pull` and delivers the *install* half. The fetch half
is blocked on something real rather than on effort: `otwono-aid` runs with
`PrivateNetwork=yes` and `RestrictAddressFamilies=AF_UNIX`, and a child process inherits
that namespace, so a downloader cannot simply be spawned the way a backend adapter is.
Giving the daemon a network would undo a deliberate choice — it is the process that must
keep answering when other things break. Downloading therefore needs a separate brokered
component with its own namespace and a policy about which hosts it may contact, and that
policy is a design decision, not an implementation detail. Recorded as **OQ-13**;
`ai.models.pull` stays absent until it is answered.

The split has a payoff already banked: everything that decides whether to *trust* a model is
now tested exhaustively with no network anywhere near it.

### The hole this closes

`blake3` in a manifest was only a *filename*. `Catalog::blob_path` joined it onto the blob
directory and **nothing ever hashed the contents**. So a manifest signed by a trusted
publisher, paired with somebody else's bytes, installed and loaded as trusted — the
signature covered the manifest, the manifest named a digest, and no code compared the
digest to the file. The signature work of slice 2 was doing half a job and the earlier
verification log recorded the placeholder digest in the test fixtures without noticing what
it implied.

`ai.models.install` hashes the blob and refuses on mismatch. The chain now runs end to end:
a trusted publisher signs a manifest, the manifest names a digest, the digest names these
exact bytes.

### What was built

| Method | Capability | Behaviour |
|---|---|---|
| `ai.models.install` | `ai.admin` (new) | Verifies provenance, size and digest, then installs atomically |
| `ai.models.verify` | `ai.read` | Re-hashes an installed model against its manifest |

`ai.admin` is a new action in the permission registry with `BlastRadius::Irreversible`.
Reading a catalog and changing what a node will run are different powers, and there is a
test asserting an `ai.read` token cannot install.

Order of checks is deliberate: provenance first (no reason to hash gigabytes for a manifest
we will refuse anyway), then size (a truncated download is the common case and costs a
`stat`), then the digest.

### Verified

| Command | Result |
|---|---|
| `cargo test --workspace` | **459 tests pass** (443 before) |
| `clippy -D warnings`, `fmt --check`, `shellcheck -S warning` | clean |
| `amd64-qemu-ubuntu` boot, `AI_ENGINE=llama.cpp` | `OTWONO-AI-OK … backends=llama-cpp-cpu sandbox=full models=0 publishers=0` |

The boot was re-run because both `otwono-aid` and `otwono-permd` changed — a new action in
the registry is validated against the shipped policy at startup, and that is exactly the
kind of change that boots fine on a developer's machine and fails on an image.

**The full-stack inference test now installs through this path** rather than planting a blob
with a placeholder digest. So the model that gets loaded and produces tokens is one the
daemon hashed and accepted, and the caveat recorded in slice 3 — "the catalog does not
currently verify content against the digest" — no longer applies.

### The tests worth naming

- **A correctly signed manifest with the wrong bytes is refused.** The case the whole slice
  exists for, asserted both as a unit test and over real sockets.
- **A tampered manifest is refused with `allow_unsigned` both false and true.** The opt-in
  means "I know where this came from", never "somebody changed this".
- **A failed install leaves the blob directory empty.** No `.incoming-*` stray that
  `weights_present` would count as a model.
- **`verify` notices weights swapped after install**, which is the honest limit of
  verify-at-install stated as a test rather than a caveat.
- **Hashing a file larger than one chunk is correct.** The read loop is the kind of thing
  that works on small inputs and silently truncates on large ones, so it gets 2 MB.

### One design gap this surfaced, in the previous slice

Fail-closed confinement (ADR-0012) had no operator escape hatch **at the daemon level**. The
adapter accepted `--allow-unconfined`, but `otwono-aid` never passed it, so on a kernel
without Landlock there was no way to run inference at all — a decision that belongs to an
operator in a unit file, not to us by omission. `otwono-aid` now takes
`--allow-unconfined-backends`, off by default, and logs at every backend start when it is
on. Found because the full-stack test could not spawn a backend on this dev kernel; it was
a real hole and not a test artifact.

### What this does not do

- **No `ai.models.pull`.** See OQ-13 above. Nothing downloads anything.
- **No `ai.models.remove`**, so a catalog only grows.
- **Verification is not re-done at load.** Deliberate — it is linear in model size, and the
  attacker it would stop already has write access to a root-owned directory. `ai.models.verify`
  is the answer for on-demand re-checking, and it is a thing a caller asks for.
- **No progress reporting.** `ai.models.install` is synchronous, which is fine for a local
  copy and will not be fine for a download.

---

## Phase 4 slice 6 — inference on a booted node, and two defects only a boot could find

Every previous claim about inference was measured on a build host. This slice moves it onto
the target: a booted node installs a model over its own control plane and completes a
prompt.

| Target | Boot line |
|---|---|
| `amd64-qemu-ubuntu` | `OTWONO-AI-INFER-OK model=otwono-smoke-test backend=llama-cpp-cpu tokens=8` |
| `arm64-qemu-ubuntu` | `OTWONO-AI-INFER-OK model=otwono-smoke-test backend=llama-cpp-cpu tokens=8` |
| `amd64-qemu-ubuntu`, no smoke flag | `OTWONO-AI-OK … sandbox=full models=0` and the release-purity check passes |

**Phase 4's exit criterion is met**, with one qualification stated rather than buried: the
model is synthetic, so this proves the *path* — daemons, broker, capability tokens,
admission control, Landlock, systemd hardening, adapter, engine — and not that a 1B model
performs acceptably on a Pi. Tier-appropriate models are OQ-6.

### How it works, and what stays out of a release image

`AI_SMOKE_MODEL=1` adds build stage 36, which generates the model (~400 KB, random weights,
not downloaded and not committed), writes a manifest whose digest is computed from the
bytes just written, and installs a boot unit plus a policy drop-in granting `ai.read`,
`ai.infer` and `ai.admin`.

That policy is the reason this is opt-in and loud. A shipped `ai.admin` grant lets anything
on the box change what the node will run. Stage 60 now asserts a release image contains
neither the model nor the drop-in, so forgetting to turn it off fails the build rather than
reaching a user.

The check drives `otwono-aictl`, a new CLI. The AI subsystem had none — there was no way to
install a model or run a prompt without hand-writing JSON-RPC into a socket. It asks the
broker for a capability like any other client, so if policy does not grant the action it
fails exactly as anything else would.

### Defect 22: our own hardening prevented the engine from being confined

First boot: `OTWONO-AI-INFER-FAIL`, with the adapter reporting *"this kernel does not
enforce Landlock"* — on a kernel that demonstrably does, and on the same boot where the
check had just printed `sandbox=full`.

The landlock syscalls are in systemd's **`@sandbox`** group, not `@system-service`.
`otwono-aid.service` filters to `@system-service` with `SystemCallErrorNumber=EPERM`, so
the adapter's `landlock_create_ruleset` returned EPERM — and the crate maps any error other
than `EOPNOTSUPP` to "not implemented". Hardening written to protect the daemon was
disabling the confinement protecting the node from the daemon's own child, and the symptom
was indistinguishable from an old kernel.

Confirmed rather than guessed, by asking the image's own systemd:

```
$ systemd-analyze syscall-filter @system-service | grep -i landlock   # nothing
$ systemd-analyze syscall-filter @sandbox
    landlock_add_rule
    landlock_create_ruleset
    landlock_restrict_self
    seccomp
```

Fixed by adding `SystemCallFilter=@sandbox` to the unit — three syscalls, plus `seccomp`,
which a process can only use to restrict itself further.

### Defect 23: the boot check was measuring the wrong process

Worse than the first, and hidden by it. `sandbox=full` was printed on the very boot where
the engine could not be confined at all, because the check ran the adapter's `--probe`
*itself* — under `otwono-ai-check.service`, which has no `SystemCallFilter`. It measured its
own environment and reported it as the daemon's.

A status check that answers a different question than the one it appears to answer is worse
than no check: it is the reason the first failure was confusing rather than obvious. The
daemon now probes once at startup, in its own sandbox, and publishes the result as
`backend_confinement` in `ai.capabilities`; the boot check reads that over the socket.

### Defect 24: the engine could not start in the image it shipped in

Second boot got further and failed with
`libgomp.so.1: cannot open shared object file`. llama.cpp links OpenMP and the minimal base
had no `libgomp1`. Two fixes, and the second matters more: `libgomp1` added to the recipes,
and **stage 35 now walks the engine's `readelf -d` NEEDED list against the rootfs and fails
the build** if anything is missing. This cost a twenty-minute QEMU cycle to discover; the
check finds it in a second, for any future engine variant.

### Workspace

| Command | Result |
|---|---|
| `cargo test --workspace` | **471 tests pass** (459 before) |
| `clippy -D warnings`, `fmt --check`, `shellcheck -S warning` | clean |

### A note on the marker mechanism

The QEMU runners took `OTWONO_BOOT_EXPECT` to *replace* the required-marker list. Using it
to add the inference marker would have silently stopped checking the other four — a test
that passes for a worse reason. Added `OTWONO_BOOT_EXPECT_EXTRA`, which appends.

### Defect 25: a test flake CI found and this machine could not

`an_engine_that_exits_immediately_is_a_crash_with_its_stderr` failed on the runner with
`Text file busy (os error 26)` — `ETXTBSY` on exec, not the crash it was asserting.

A property of fork/exec in a multithreaded program, not of the code under test. `cargo test`
runs tests as threads of one process: when one thread has just written a fake engine script
and another forks for its own spawn, the child inherits the writer's still-open descriptor
until it execs, and `execve` on an inode any process holds open for writing returns
`ETXTBSY`. Rust opens files `O_CLOEXEC`, which closes the window at exec but not between
fork and exec.

Fixed with a bounded retry in the three tests that exec a script they wrote — deliberately
in the tests and not in `Engine::start`, because a shipped node execs an installed binary
nobody is writing, and a retry there would paper over a real failure. Re-run ten times with
six spinners saturating the CPU: 0 failures.

The pattern is now familiar enough to name: this is the third defect this slice that only
appeared somewhere other than a quiet developer machine. An idle four-core box is one
schedule, and passing on it is weak evidence.

### What this does not prove

- **Nothing about model quality or speed.** Random weights, eight tokens, under TCG.
- **Nothing about real models.** No node has run anything a person would want to use, and
  nothing downloads models (OQ-13).
- **Nothing about concurrent load.** One prompt, one engine, one sequence.

---

## Pi 4 boot-chain investigation (ADR-0013) — measurement only, nothing built

Recorded 2026-08-23. This entry exists because ADR-0013 is a decision with no code and no
boot behind it, and the measurements it rests on should be reproducible by someone who
doubts them. **Nothing in this section is a claim that anything boots on a Pi.**

### What was measured, and how

Reachability, by `git ls-remote --heads` through the environment's proxy:

```
tianocore/edk2               git: OK
tianocore/edk2-platforms     git: OK
tianocore/edk2-non-osi       git: OK
raspberrypi/firmware         git: OK
raspberrypi/rpi-eeprom       git: OK
pftf/RPi4                    git: OK
```

Git works for all of them; GitHub Releases and the API do not (403 at the proxy), which is
why the ADR treats "distributed as a release archive" as a real cost.

Clone sizes, both blobless (`--filter=blob:none --depth 1`):

- `raspberrypi/firmware` with `sparse-checkout set boot` — **131 MB**, HEAD `06df1d1a`
  (2026-08-21). Contains `start4.elf` (2,298,048 bytes), `fixup4.dat`, `bcm2711-rpi-4-b.dtb`
  (56,407 bytes), and 380 overlays.
- `tianocore/edk2` — **171 MB**, and `.gitmodules` lists 13 further submodules (openssl,
  mbedtls, libspdm, brotli, oniguruma, googletest, …). That is the floor for building
  `RPI_EFI.fd` ourselves, before edk2-platforms, edk2-non-osi, and an EDK2 build
  environment.

Package presence, `ports.ubuntu.com` `noble` `main`+`universe`, `binary-arm64` (1,591,568
lines of `Packages` fetched and grepped):

```
linux-firmware-raspi       0
raspi-firmware             0
linux-image-raspi          1        6.8.0-1004.4
rpi-eeprom                 1        20.4-1ubuntu2
u-boot-rpi                 1        2024.01+dfsg-1ubuntu5
```

No VideoCore firmware package for arm64 in either component, which is what forces the
pinned git checkout above.

`u-boot-rpi` ships its build configs, so the decisive facts came from the package itself
rather than from documentation:

```
$ zcat usr/share/doc/u-boot-rpi/configs/config.rpi_4.gz | grep -E 'EFI_LOADER|BOOTEFI_BOOTMGR|EFI_VARIABLE_FILE_STORE|BOOTCOUNT'
CONFIG_EFI_LOADER=y
CONFIG_CMD_BOOTEFI_BOOTMGR=y
CONFIG_EFI_VARIABLE_FILE_STORE=y
# CONFIG_BOOTCOUNT_LIMIT is not set
```

The first three are why the decision is possible: a U-Boot that can run our existing
`BOOTAA64.EFI` and store EFI variables on the ESP is already packaged in an archive this
environment can reach. The fourth is why ADR-0008 now carries a note — the arm64 rollback
counter it names is not compiled in (OQ-14).

The `pftf/RPi4` disqualifiers are quotations from its own `Readme.md` at HEAD `6bd9f0a`
(2026-05-28), reproduced in ADR-0013; the 3 GB default RAM cap and its interactive-only
override are the ones that end the argument for a headless node whose capability tier is
derived from total memory.

`tryboot_a_b` — the A/B mechanism the native chain would need — is real and mature:
`firmware-2711/release-notes.md` records "[tryboot] conditional statement + tryboot_a_b
mode" promoted to DEFAULT on 2022-12-01. GPT is likewise supported by the Pi 4 bootloader:
"Add support GPT and Hybrid MBR partition tables" (2020-05), which is what lets stage 50's
existing `ef00` ESP serve as the Pi's boot partition unchanged.

### Why there is no boot log

```
$ qemu-system-aarch64 -machine help | grep -i raspi
raspi0  raspi1ap  raspi2b  raspi3ap  raspi3b
```

QEMU 8.2.2 here has no Raspberry Pi 4 machine model, and there is no Pi hardware attached
to this environment. The chain in ADR-0013 therefore cannot be exercised at all until an
image is written to a card and a board is booted with a serial console. ADR-0013 is
**STATUS: SPECIFIED** and says so.

### What this does not prove

- **That the chain works.** Every link is plausible from vendor documentation and package
  metadata. None has been run.
- **That the ESP layout survives contact with the EEPROM.** GPT support is documented; that
  our particular GPT, offsets, and FAT parameters are acceptable to `start4.elf` is not
  known.
- **That U-Boot's `EFI_LOADER` will load our GRUB build specifically.** `CONFIG_EFI_LOADER=y`
  says the subsystem is present, not that this binary runs.

---

## Egress investigation (ADR-0014) — measurement only, nothing built

Recorded 2026-08-23. As with ADR-0013, this is a decision with no code behind it, and the
measurements it rests on should be reproducible. **Nothing here claims a fetcher exists.**

### The systemd directive the hardening depends on

ADR-0014 leans on `IPAddressDeny=localhost link-local multicast` to contain SSRF. That was
checked rather than assumed, on systemd 255 (255.4-1ubuntu8.17):

```
$ systemd-analyze verify ./otwono-fetchd-probe.service
Binding to IPv6 address not available since kernel does not support IPv6.
$ echo $?
0
```

Silence is only evidence if the tool is capable of speaking, so the same tool was given a
misspelled key and an invalid token:

```
$ systemd-analyze verify ./bogus.service
bogus.service:6: Unknown key name 'IPAddressDenyy' in section 'Service', ignoring.
bogus.service:7: Invalid address prefix is specified in [Service] IPAddressDeny=, ignoring
  assignment: not-a-real-token
```

It catches both, so its acceptance of `localhost link-local multicast` means the directive
and the tokens are real. The IPv6 line is this container's kernel, not the unit.

What this does **not** establish is that the filter runs: `IPAddressDeny` is a cgroup BPF
program, and on a kernel without `CONFIG_CGROUP_BPF` systemd logs a failure and starts the
unit anyway. The ADR says so in its consequences rather than treating the directive as a
boundary.

### The HTTP client, measured rather than argued

Both candidates resolved and their dependency trees counted on 2026-08-23:

| | crates (`cargo tree -e normal`, unique) | async runtime |
|---|---|---|
| `ureq` 3.4.0 | **28** | none |
| `reqwest` 0.13.4, `default-features = false`, `features = ["rustls","blocking"]` | **85** | `tokio` + `hyper` |

Both land on `rustls` + `ring`; the difference is the runtime. This workspace has no async
runtime in it today, and `reqwest`'s blocking mode runs a tokio reactor internally, so the
narrower crate is also the one that does not change the shape of the codebase.
`ca-certificates` is already in all four recipes, so no image change is needed for trust
roots.

### What already exists, and therefore is not being rebuilt

The pieces ADR-0014 assumes are in the tree and tested:

- `net.egress` is already a registered action with `BlastRadius::Egress` and
  `always_confirm: true`, so `net.fetch` is a narrowing of something that exists rather
  than a new concept — the same move `id.sign` → `id.sign_session` made in ADR-0010, whose
  rationale the registry's own test states.
- `otwono-ai`'s `install()` already streams a BLAKE3 over the blob, checks size first, and
  stages-and-renames. Because the fetcher hands back a spool path and nothing more, that
  code is what verifies a downloaded model, unchanged and with no network near it.
- `otwono-aid.service` has `PrivateNetwork=yes` and `RestrictAddressFamilies=AF_UNIX`, and
  keeps both under this decision.

### What this does not prove

- **That any of it works.** There is no `otwono-fetchd` crate, no `net.fetch` action, no
  schema and no unit. `cargo test --workspace` still reports 471 tests, because this
  changed documentation only.
- **That the allow-list model is workable against real model hosts.** A host needing a
  query string, a signed URL, or an auth header does not fit the named-source interface as
  specified. That is a deliberate constraint and it is untested against a real registry.
- **That the covert channel is as narrow as claimed.** The bound on a path suffix is a
  design intent, not a measurement.

---

## Phase 4 slice 7 — brokered egress, and three defects a real server found

ADR-0014 implemented. `otwono-fetch` (the rules) and `otwono-fetchd` (the daemon) are in
the tree, `net.fetch` is a registered action, `schemas/egress-source.schema.json` is the
allow-list contract, and `docs/network/EGRESS.md` describes the subsystem.

### Workspace

```
cargo test --workspace   541 passed, 0 failed   (471 before this slice)
cargo clippy --workspace --all-targets -- -D warnings   clean
cargo fmt --all --check                                 clean
cargo build --workspace --release --target aarch64-unknown-linux-gnu   ok
qemu-aarch64-static … otwono-fetchd --check    prints the allow-list, rc=0
```

70 new tests: 34 in `otwono-fetch`, 9 in `otwono-fetchd`, 21 over the control plane, 5
schema contract tests, 1 in the action registry.

### The design in one line

A caller names a source and a path suffix. It never supplies a URL, so it cannot choose the
scheme, host, port, query string or a header — the only bytes it contributes to what leaves
this node are 256 bytes of a restricted alphabet, logged per fetch.

### Measured before deciding: what `http::Uri` actually does

The redirect check is the security boundary, so its parser's behaviour was measured rather
than assumed. Seventeen hostile URLs through `http::Uri`, and the four results that shaped
the code:

| Input | Result |
|---|---|
| `https://evil.example.com@huggingface.co/a` | `host()` returns `huggingface.co` — userinfo is stripped, so a host comparison is already safe. Refused anyway. |
| `https://HuggingFace.CO/a` | host preserved verbatim — a byte comparison would reject a legitimate redirect, so matching is ASCII-case-insensitive |
| `https://huggingface.co/a/../../b` | **not normalised** — `..` arrives intact, so the path rules must run on redirect targets too, not only on caller input |
| `https://huggingface.co/a%2f..%2fb` | `%2f` not decoded — so `%` is refused outright rather than decoded, because an encoded delimiter is a traversal only if something downstream decodes it |

`file://` and a backslash in the authority are both rejected by the parser itself.

The same crate parses the URL that goes on the wire, deliberately: validating with one
parser and requesting with another means the two can disagree about what the host is, and
that disagreement is the whole attack.

### Defect 26: an unknown field in the allow-list was silently ignored

The schema contract test caught it before anything shipped. `Source` had no
`deny_unknown_fields`, so `max_byte = 10` — a plausible typo — parsed as a source with no
cap rather than as an error, and the operator who wrote it had no way to tell. In the one
file that decides where a node may send bytes, silent acceptance is the least affordable
failure. Fixed with `deny_unknown_fields` on both the entry and the file, and the schema
carries `additionalProperties: false` to match.

The same test found the schema's host pattern accepting `10.0.0.5` while the loader refused
it. A `not: {pattern: "^[0-9.]+$"}` clause makes the two agree.

### Three defects that only a live server produced

The state machine had 21 passing integration tests against a network double before any of
this. All three of the following got through it.

**Defect 27: the fetcher did not trust the system's certificates.** Pointed at a real host,
it refused with `invalid peer certificate: UnknownIssuer`. `ureq` defaults to the Mozilla
roots compiled into the binary, so `/etc/ssl/certs` — the store the rest of the OS uses, and
the one `ca-certificates` populates in every recipe — was not what the fetcher consulted. A
node could not have fetched from a mirror behind a private CA, and nothing in the image
would have explained why. ADR-0014 asserted "`ca-certificates` is already in every recipe",
which was true and irrelevant. Fixed with `ureq`'s `platform-verifier`: 31 crates against
28, and `/etc/ssl/certs` is now load-bearing.

**Defect 28: transparent decompression would have silently corrupted resumed downloads.**
`gzip` is a default feature of `ureq`. A decompressed body is not the bytes the server's
`Range` header addresses, so the second call of every resumed fetch would have asked for the
wrong offset. Against a server that refuses the range it fails loudly; against one that
accepts it, it assembles a corrupt file that only the caller's digest catches — after the
whole transfer. Fixed by dropping the feature. We fetch opaque blobs; the caller hashes
them.

**Defect 29: a partial that could never be resumed made the caller loop forever.** With no
`Content-Length` there is nothing to resume *to*. The first call filled its budget and
returned `complete: false`; the second asked for a range the server refused; the partial was
discarded; the third started over. Observed running six times with no progress. The fix is
to refuse once, clearly: an object whose size the server will not state must fit one call,
and the message says to retry with a larger `max_bytes`. Two regression tests, including one
that the ordinary short-body case still completes.

None of the three is exotic. All three needed a server that was not written by the same
person as the client.

### The live run

```
call 1: bytes_have 16384  bytes_total 40256  complete false
call 2: bytes_have 32768  bytes_total 40256  complete false
call 3: bytes_have 40256  bytes_total 40256  complete true   blob_path …/e9c3….blob

curl:   5d33fd0c1128bc0266f96f36ced03c47  40256 bytes
fetchd: 5d33fd0c1128bc0266f96f36ced03c47  40256 bytes
IDENTICAL
```

Real `otwono-permd` issuing a real `net.fetch` token, real Unix sockets, real TLS to
`pypi.org` with a 16 KiB per-call budget, three resumed `Range` requests, reassembled
byte-for-byte. The refusal paths were exercised on the same running instance: a `..` segment
(`-32602`), a token scoped to a different source (`-32000`), no token at all (`-32000`), a
second fetch served from the spool without touching the network, and `fetch.discard`
emptying it.

TLS was reachable at all only because this environment's proxy tunnels allow-listed hosts,
which is also how Defect 27 surfaced — the proxy's CA is in the system store and not in
`ureq`'s bundled roots.

One more real-world encounter, not a defect: PyPI redirects `/simple/six` to `/simple/six/`,
and this daemon refuses to follow a redirect to a path ending in `/`. That is intended — it
fetches objects, not listings — and it is now recorded in `EGRESS.md` as a limitation rather
than discovered later.

### The unit, checked but not booted

`systemd-analyze verify` accepts `otwono-fetchd.service` as the stage writes it, complaining
only that `otwono-permd.service` and `/usr/bin/otwono-fetchd` are absent on the build host.

The syscall filter was checked against the target rootfs rather than assumed, because
`@system-service` is exactly what caused Defect 22 for Landlock:

```
$ chroot out/amd64-qemu-ubuntu/rootfs systemd-analyze syscall-filter @network-io
connect getpeername getsockname getsockopt recvfrom recvmsg sendmsg sendto
setsockopt shutdown socket
```

`@network-io` is inside `@system-service`, and `getrandom` is in `@default`. Everything a
TLS client needs is permitted, so this unit does not repeat that mistake.

### What this does not prove

- **Nothing has booted.** No image was built with `otwono-fetchd.service`, so the unit's
  hardening — `IPAddressDeny`, `ReadWritePaths`, the syscall filter — has been read and
  verified, never enforced. `EGRESS.md` says so in its status banner.
- **`IPAddressDeny` has never blocked anything here.** It is a cgroup BPF program and this
  container is not where that gets tested.
- **Nothing large has been fetched.** 40 KB in three calls, not 4 GB in sixty-four. The
  arithmetic is the same; the failure modes of an hour-long transfer are not.
- **`ai.models.pull` still does not exist.** This slice built the thing it was blocked on.
  Wiring `otwono-aid` to call `fetch.get` and hand the spool path to `ai.models.install` is
  the next slice, and it is deliberately not in this one.
- **No proxy configuration ships.** TLS was exercised through this environment's proxy via
  `ureq`'s reading of the standard environment variables. No node sets them, and nothing
  here manages them.

---

## Defect 30: three tests, one scratch file, and the digest of nothing

CI went red on `c9d5b30` — a documentation-only commit that changed no code. The failure was
real, pre-existing, and intermittent: the previous head had been green, and
`cargo test --workspace` passed here twice on the same tree.

```
---- verifying_a_model_reports_a_mismatch_as_a_result_rather_than_an_error ----
ai.models.install refused: the weights do not match the manifest:
  expected blake3 af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262,
  got      138808f4337b2ceb3ac0661641a853cf9d0d1c15f10b9a9dd51cb27c6002455a
```

`af1349b9…` is the BLAKE3 of the empty string, which is what made this quick to place: the
manifest was claiming the digest of a file with nothing in it.

**Root cause.** The test helper hashed a buffer by writing it to a scratch file and calling
the production `hash_file`. The scratch path was `otw-h-{pid}-{body.len()}`. Every test in a
`cargo test` binary shares a pid, and three tests in that file use the same 27-byte body, so
all three named **the same file** and ran as concurrent threads. `fs::write` is
create-truncate-then-write, so between the truncate and the write the file is empty — and a
racing `hash_file` read zero bytes.

Nothing was wrong with the code under test. The installer refused a manifest whose digest
the weights could not have, which is exactly its job.

**Reproduced before fixing**, six spinners saturating the CPU, the test binary at
`--test-threads 8`:

```
40 runs → 1 failure, byte-identical error including both digests
```

**Fixed** by making the scratch path unique per call with a process-wide atomic counter, and
removing the file afterwards. Hashing still goes through `hash_file` rather than hashing the
buffer directly — that is deliberate, because the manifest's digest and the installer's must
come from one implementation, or a bug in it would agree with itself and the test would
prove nothing.

**Same reproduction after the fix:** 60 runs under the same load, 0 failures.

**Audited the rest of the suite** for the same shape. Every other `temp_dir().join(...)` in
the workspace is either tagged per test or sits inside a single `#[test]` with one call
site. This was the only helper shared by several tests that built a name which was not
unique.

### The pattern, now four for four

This is the fourth defect in this project that appeared only somewhere other than a quiet
developer machine — after the systemd syscall filter, the boot check measuring the wrong
process, and the `ETXTBSY` exec race. Three of the four were in test code rather than
product code, which is its own lesson: a test that races is a test that will eventually lie
in whichever direction is least convenient.

It also cost nothing to find and would have cost a great deal to find later. A docs-only
commit turning CI red is a gift — the alternative is the same race surfacing during a
release.

---

## Phase 4 slice 8 — `ai.models.pull`, and a gate that was not checking anything

Wires `otwono-aid` to `otwono-fetchd`, which until now had no production caller. A model can
be downloaded, verified and installed in one brokered call.

### Workspace

```
cargo test --workspace   560 passed, 0 failed   (541 before)
cargo clippy --workspace --all-targets -- -D warnings   clean
cargo fmt --all --check                                 clean
cargo build --workspace --release --target aarch64-unknown-linux-gnu   ok
```

19 new tests: 12 over the control plane, 5 in `otwono-ai`, 2 in `otwono-aid`.

### The ordering, which is the whole design

Each step is cheaper than the next and each can refuse, so the expensive one runs only once
the cheap ones agree: manifest (kilobytes) → provenance → could-it-ever-fit → weights
(gigabytes) → install, which re-hashes → discard the spool copy.

Three of the twelve integration tests assert the *ordering* rather than the outcome, by
checking that the fake host was never asked for the weights. A test that only checked the
final answer would pass while the node downloaded four gigabytes it was always going to
throw away.

### Defect 31: the pre-download size gate never ran

Caught by an integration test, and it was a design error rather than a slip.

The gate called `admit` — the same function `ai.infer` uses. Two things were wrong with
that, and the second is the serious one:

- **`admit` refuses when no backend is installed.** A node with no engine is exactly the
  node that wants to download a model, and the engine is opt-in in the build
  (`AI_ENGINE=llama.cpp`), so the two legitimately arrive in either order. As written, a
  fresh node could not pull the model it was being set up to run.
- **`admit` returns its first error.** `NoBackendAvailable` is raised at line 160;
  the memory arithmetic is at line 182. So on a node with nothing installed, the size check
  **never executed**. Filtering that one error out — the first fix attempted — would have
  let a 40 GiB model onto a 4 GiB board without ever weighing it. The gate would have
  looked present and been inert.

Replaced with `otwono_ai::fits_this_machine`, which asks the narrower question — *could this
machine ever hold this model?* — takes no backend list, and therefore cannot be masked. It
errs toward permitting: a refused download is a hard stop for the user, a wasteful one costs
bandwidth, so the ceiling is the largest memory pool the machine has and anything uncertain
is allowed. Five unit tests, one of which asserts both behaviours at once so the masking
cannot come back:

```
admit(...)             → NoBackendAvailable      (first error, memory never reached)
fits_this_machine(...) → InsufficientMemory      (the real problem)
```

### Two test bugs found in passing

Both mine, both in the new file, both worth naming because they are the shape that makes a
test lie:

- The harness requested an `ai.admin` token for every call, including `ai.models.list`,
  which needs `ai.read`. A blanket grant would have hidden a method losing its guard.
- `oversized()` inflated a manifest *after* signing it, so the test exercised a
  signature-verification failure while claiming to test the size gate.

### Nothing needed to change in the hardening

`otwono-aid` keeps `PrivateNetwork=yes` and `RestrictAddressFamilies=AF_UNIX`. It drives the
fetcher over an `AF_UNIX` socket, which is not network-namespaced, and its existing
`ReadWritePaths=/var/lib/otwono` already covers the spool. The unit gained
`Wants=otwono-fetchd.service` — Wants and not Requires, because a node with no allow-list
still runs local inference perfectly well, and `ai.models.pull` answering "this node cannot
download models" is a better state than the AI daemon refusing to start.

### What this does not prove

- **Nothing has booted with it.** No image was built with `--fetch-socket` in the unit, so
  the ordering, the `Wants=`, and the spool permissions are verified by reading and by
  tests, not by a running node.
- **No real model has been pulled.** The integration tests use a fake host, and the live
  fetch proven in slice 7 was a 40 KB JSON file from `pypi.org`. Nothing has downloaded
  gigabytes, and the hosts that serve real models are not reachable from this environment.
- **`fits_this_machine` has never refused a real download.** Its arithmetic is unit tested
  against fixture profiles; it has not been exercised against a genuine oversized model on
  genuine hardware.
- **Tier-appropriate models remain OQ-6.** The mechanism to fetch one exists; which one to
  fetch, and under what licence, does not.

---

## Phase 5 slice 1 — chunking parameters, measured (ADR-0016)

OQ-16 settled. It is the one parameter in the content store that is a network-wide
compatibility constant: two nodes that chunk the same bytes differently produce
different digests and cannot serve each other, and nothing reports an error — the swarm
just never forms. So it was measured rather than defaulted.

### Method

`fastcdc` 5.0.0 (FastCDC v2020) with BLAKE3, over three inputs chosen to match what a
node actually stores: 256 MiB of high-entropy data standing in for quantized weights, a
57 MiB rootfs tar with real internal duplication, and 117 MiB of source and documentation
text. Harness in the session scratch, x86_64, 4 cores.

`after-insert` is the fraction of chunks still shared after 64 bytes are inserted near
the front — the case content-defined chunking exists for. `fixed` is the same edit
against fixed-size blocks of the same nominal size. Index cost assumes 48 bytes per
entry: a 32-byte digest, an 8-byte offset, a 4-byte length and change.

```

=== /tmp/claude-0/-home-user-OTWONO/8cfe0a17-7dda-505f-a604-d135080a5451/scratchpad/cdc-in/model.bin  (256 MiB) ===
params             chunks   mean KiB   index/GiB after-insert      fixed
2/8/16 KiB          27835        9.4        5.1M       100.0%       0.0%
4/16/64 KiB         13284       19.7        2.4M       100.0%       0.0%
8/32/128 KiB         6661       39.4        1.2M       100.0%       0.0%
16/64/256 KiB        3340       78.5        0.6M       100.0%       0.0%
32/128/512 KiB       1674      156.6        0.3M        99.9%       0.0%
64/256/1024 KiB       831      315.5        0.2M        99.9%       0.0%

=== /tmp/claude-0/-home-user-OTWONO/8cfe0a17-7dda-505f-a604-d135080a5451/scratchpad/cdc-in/rootfs.tar  (57 MiB) ===
params             chunks   mean KiB   index/GiB after-insert      fixed
2/8/16 KiB           5979        9.8        4.9M       100.0%       2.4%
4/16/64 KiB          2822       20.7        2.3M       100.0%       2.2%
8/32/128 KiB         1462       40.0        1.2M        99.9%       1.7%
16/64/256 KiB         733       79.8        0.6M        99.9%       1.1%
32/128/512 KiB        364      160.7        0.3M        99.7%       0.7%
64/256/1024 KiB       193      303.1        0.2M        99.5%       0.0%

=== /tmp/claude-0/-home-user-OTWONO/8cfe0a17-7dda-505f-a604-d135080a5451/scratchpad/cdc-in/text.dat  (117 MiB) ===
params             chunks   mean KiB   index/GiB after-insert      fixed
2/8/16 KiB          12380        9.7        4.9M       100.0%       0.0%
4/16/64 KiB          5667       21.2        2.3M       100.0%       0.0%
8/32/128 KiB         2879       41.7        1.2M       100.0%       0.0%
16/64/256 KiB        1383       86.8        0.6M        99.9%       0.0%
32/128/512 KiB        792      151.6        0.3M        99.9%       0.0%
64/256/1024 KiB       355      338.3        0.1M        99.7%       0.0%
timeout: failed to run command ‘/tmp/claude-0/-home-user-OTWONO/8cfe0a17-7dda-505f-a604-d135080a5451/scratchpad/cdc/target/release/rate’: No such file or directory

/tmp/claude-0/-home-user-OTWONO/8cfe0a17-7dda-505f-a604-d135080a5451/scratchpad/cdc-in/model.bin  (256 MiB)
params              chunk MiB/s chunk+hash MiB/s
4/16/64 KiB                1640             1035
8/32/128 KiB               1763             1146
16/64/256 KiB              1750             1191
64/256/1024 KiB            1775             1252
```

### What the numbers decided

- **Content-defined chunking is not optional.** CDC keeps 99.5–100% of its chunks across
  an insertion; fixed-size blocking keeps **0–2.4%**. A store built on fixed blocks would
  dedup byte-identical files and essentially nothing else.
- **Boundary stability does not choose the parameters** — every set holds ≥99.5%.
- **Neither does throughput** — under 20% variation across a 32× range of chunk sizes,
  and hashing dominates chunking throughout.
- **Index cost does**, spanning 25× across the table. At 64 KiB average the index is
  ~0.4 MiB on a T0 node's 512 MiB contribution and ~96 MiB on a T3 node's 128 GiB.

Chosen: **FastCDC v2020, 16 KiB / 64 KiB / 256 KiB**, one parameter set network-wide.

### What this does not prove

- **Nothing is built.** No content store exists. This measured a candidate dependency
  against candidate parameters; the crate is the next slice.
- **Not measured on ARM.** These are x86_64 numbers with SIMD BLAKE3. A Cortex-A72 will
  be several times slower, and a 4 GB model must be chunked and hashed before it can be
  stored or served. The parameter choice does not depend on it — throughput barely varies
  — but the user-facing wait does, and it is unknown.
- **The insertion test is synthetic.** One 64-byte insertion near the front is the
  canonical CDC benchmark, not a distribution of real edits. Real-world dedup ratios
  across genuinely related files have not been measured.
- **High-entropy data does not dedup and never will.** Chunking buys resumable, parallel,
  verifiable transfer for model weights, not storage savings. Only that is claimed.

---

## Phase 5 slice 2 — the content store's contracts

`otwono-store`: chunking, content addressing, the object model, visibility labels, and the
on-disk chunk store. Pure logic and local files, no daemon — the daemon is the next slice.

### Workspace

```
cargo test --workspace   608 passed, 0 failed   (560 before)
cargo clippy --workspace --all-targets -- -D warnings   clean
cargo fmt --all --check                                 clean
cargo build --workspace --release --target aarch64-unknown-linux-gnu   ok
```

48 new tests: 42 in `otwono-store`, 6 schema contract tests.

### Four decisions worth naming

**Identity is the content and nothing else.** A `ContentId` is BLAKE3 over the chunk list,
the chunking version, and each chunk's digest and length — not the label, not a filename,
not who stored it. Two people who store the same bytes get the same name even if one marks
it `Public` and the other `Private`, which is what makes any holder of a chunk
interchangeable with any other (ADR-0015). Relabelling therefore does not rename anything.
The cost, recorded rather than glossed: a content id **reveals that you hold particular
bytes** to anyone who can guess them, which is the same "holding is publishing" property
ADR-0015 names, and it is why `Private` objects never enter any shared index.

**Labels cannot fail to parse.** `Deserialize` for `Visibility` is infallible: a missing
field, a value from a newer version, a JSON number where a string belongs — all read as
`Private`. The alternative is an error, and an error is a decision a caller can get wrong.
Damaging a record must never make its contents *more* available. `parse_strict` exists
separately for config files, where a human who typed `publik` should be told.

**Every read is verified.** `get_chunk` re-hashes what it read and reports a mismatch as
corruption rather than returning the bytes. A caller asked for particular bytes by name;
anything else is not a smaller answer, it is a wrong one. This is what lets an untrusted
peer serve a chunk safely, and it is tested by writing junk over a stored chunk and
requiring the read to fail.

**Streaming and in-memory chunking must agree.** A 4 GB model on a 4 GB board has to be
chunked without being read into memory, so there are two code paths. A test asserts they
produce identical digests — otherwise dedup would silently depend on which path a file
happened to take.

### The properties the tests actually assert

- Chunking the same bytes twice gives the same chunks. If this can ever be false, two nodes
  cannot serve each other and nothing tells them why.
- An insertion near the front leaves >90% of chunks shared — the ADR-0016 property,
  asserted as a property rather than as its measured figure, which depends on the data.
- Storing the same bytes twice stores them once, and an edited file adds chunks
  proportional to the edit rather than to the file.
- A tampered chunk is reported, not returned. A missing one is "not found", not a short read.
- Derived content inherits the most restrictive input label, checked exhaustively over
  every pair and triple of the four labels — the property test `DATA-VISIBILITY.md` §6 asks
  for, done by enumeration since there are only four.
- An interrupted write leaves no chunk under a finished name.

### What this does not prove

- **There is no daemon.** No control-plane methods, no brokered access, no authorization.
  Everything here is a library that a caller could use wrongly; the enforcement points in
  `DATA-VISIBILITY.md` §4 do not exist yet.
- **Nothing is encrypted.** `PRIVATE` objects sit on disk in the clear. The encryption
  design is written down and unimplemented.
- **The negative suite is not done.** "A `PRIVATE` object must never appear on any link,
  under any code path" cannot be tested until there is a link. What exists is the label
  arithmetic underneath it.
- **No object has crossed a network.** Two nodes agreeing on a content id is proven by
  construction and by test, not by observation.
- **Not measured on ARM beyond building.** The cross-build passes; chunking throughput on a
  Cortex-A72 remains the unmeasured number ADR-0016 named.

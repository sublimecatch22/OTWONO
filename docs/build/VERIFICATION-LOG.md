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

## Phase 1 — amd64 image boots

**`make -C build TARGET=amd64-qemu-ubuntu boot-test` passes.** The image boots under QEMU
(TCG, no KVM) to a login prompt in roughly 40 seconds of guest time, and the capability
profile is recovered from the guest's own data partition afterwards:

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

Four defects surfaced that no amount of desk-checking would have, listed because each one
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

- **arm64 has not been booted.** The arm64 image build has not completed. Only amd64 has a
  boot log. Nothing should claim a bootable arm64 image.
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

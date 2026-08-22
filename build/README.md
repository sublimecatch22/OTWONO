# OTWONO build system

See [`docs/build/BUILD-SYSTEM.md`](../docs/build/BUILD-SYSTEM.md) for the design.

## Status of each stage

| Stage | Status | Notes |
|---|---|---|
| `probe` (`tools/probe-env.sh`) | **VERIFIED** | Run in the OTWONO Cloud dev environment; output recorded |
| `05-host-tools` | **VERIFIED** | Cross-built for amd64 and arm64; the arm64 binary was executed under `qemu-aarch64-static` |
| `10-bootstrap` | **VERIFIED** | amd64 native and arm64 cross-bootstrap (binfmt_misc + qemu-user second stage) both completed |
| `20-base-config` | **VERIFIED** | Packages installed in the chroot; serial getty enabled |
| `30-otwono` | **VERIFIED (amd64 + arm64)** | Binaries, schemas, default policy and four systemd units; on a booted amd64 image both daemons start and the control-plane check passes |
| `40-kernel` | **VERIFIED (amd64 + arm64)** | Kernel 6.8.0-31-generic, 18 MiB initramfs. Asserts the initramfs is a plausible size |
| `50-image` | **VERIFIED (amd64 + arm64)** | GPT + ESP + A/B roots + data, assembled with no loop devices and no mounts. 8 GiB apparent, 419 MiB on disk |
| `60-verify` | **VERIFIED (amd64 + arm64)** | Boots under QEMU to a login prompt and recovers the capability profile the guest wrote to its own data partition |

## Targets

| Target | Base | Bootstraps in OTWONO Cloud? |
|---|---|---|
| `amd64-qemu` | Debian trixie | No — the egress proxy rejects Debian mirrors |
| `amd64-qemu-ubuntu` | Ubuntu noble | Yes |
| `arm64-qemu` | Debian trixie | No — same reason |
| `arm64-qemu-ubuntu` | Ubuntu noble ports | Yes |

Debian is the canonical product base (ADR-0002). The Ubuntu recipes exist because that is
what CI in this environment can actually reach. Both must keep working.

## Adding a board

Create `build/bsp/<vendor>-<board>/bsp.toml` rather than forking a recipe. Mainline kernel
first; a vendor kernel is a documented exception with an exit plan.


## Image assembly without loop devices

Stage 50 builds each filesystem as a standalone file — ext4 via `mkfs.ext4 -d`, FAT via
mtools — and writes it into the disk image at its partition offset. Nothing is mounted and
no loop device is used.

That is not a workaround for this environment alone, though it started as one: `losetup
--partscan` creates no partition nodes here because there is no udev. Avoiding mounts also
removes the build's dependence on the host's mount namespace and device naming, which is
what reproducibility needs.

The boot chain puts the kernel and initramfs on the ESP rather than the root filesystem, so
GRUB needs only FAT support, the standalone EFI binary stays small, and a damaged root
filesystem still reaches a boot menu.

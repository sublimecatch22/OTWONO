# ADR-0009 — Mount-free image assembly, and the kernel on the ESP

**Status:** accepted · **Date:** 2026-08-22

## Context

Stage 50 must turn a rootfs directory into a partitioned, bootable disk image for two
architectures. The conventional recipe is: `losetup --partscan`, `mkfs` each partition
node, `mount`, `cp -a`, `grub-install`, `umount`.

That recipe does not work here, and the reason generalises. `losetup --partscan` created no
partition device nodes in the build environment, because the kernel's partition scan
populates sysfs but **udev** creates `/dev/loopNpM` — and there is no udev in a container
or a minimal CI runner. The workaround (`mknod` from `/sys/class/block/*/dev`) is possible
but leaves the build dependent on the host's device naming and mount namespace, neither of
which is reproducible.

Separately, the boot chain has to work identically on amd64 UEFI and arm64 UEFI, and
`grub-install` wants to probe the device holding `/boot`, which is exactly the thing we
cannot reliably present.

## Decision

**1. Build each filesystem as a standalone file, then write it into the image at its
partition offset. Never mount anything.**

- ext4: `mkfs.ext4 -d <rootfs>` populates the filesystem directly from a directory,
  preserving ownership, modes and device nodes.
- FAT: `mkfs.vfat` then `mmd`/`mcopy` from mtools.
- Offsets come from `partx` reading the GPT out of the image file.
- Each filesystem is written with `dd conv=notrunc,sparse`, so the image stays sparse.

**2. Put the kernel and initramfs on the ESP, not on the root filesystem.**

GRUB then needs only FAT support. The bootloader is a single standalone EFI binary built
by `grub-mkstandalone` **inside the target rootfs**, so the target's own grub packages
produce a binary for the target architecture — on arm64 that runs under the qemu-user
chroot stage 10 already establishes.

**3. Derive every UUID and the FAT volume id from `(target id, role, SOURCE_DATE_EPOCH)`.**

## Consequences

**Good:** the build needs no udev, no loop devices, no privileged mount, and no
device-mapper, so it runs in a container, a CI runner, or an unprivileged-ish sandbox.
Nothing depends on host device naming, which is a precondition for reproducibility.
The bootloader step is architecture-uniform: one command, the chroot supplies the
difference. Kernel on the ESP means a damaged root filesystem still reaches a boot menu,
and A/B slot switching is a FAT file copy rather than a bootloader reinstall.

**Bad:** `mkfs.ext4 -d` fixes the filesystem size at creation, so a partition cannot be
grown by editing the recipe alone — the image is rebuilt, which we do anyway. Kernel
updates must write to the ESP, so the ESP must be sized for two kernels plus two
initramfs images (512 MiB today, against roughly 33 MiB per slot). mtools does not
preserve Unix permissions, which is fine for FAT and would not be for anything else. And
we give up `grub-install`'s automatic handling of exotic setups; the trade is that what we
do instead is fully explicit.

**Note on FAT timestamps:** FAT cannot represent dates before 1980, and this environment
exports `SOURCE_DATE_EPOCH=0`. The build clamps the epoch to 1980-01-01 for the whole run,
with a warning, rather than writing nonsense dates into the ESP.

## Alternatives rejected

- **Loop devices with `mknod` fallback** — works, but keeps the dependence on host device
  naming and needs `CAP_SYS_ADMIN` for the mounts. Strictly worse on both counts.
- **`libguestfs` / `guestfish`** — capable and well-tested, but it boots a helper VM per
  build. Under TCG with no KVM that is minutes of overhead per stage, and it is a heavy
  dependency for something four standard tools already do.
- **`grub-install` against a loop device** — the device-probing problem, plus it needs the
  host to carry grub binaries for both architectures. `grub-efi-arm64-bin` is not
  installable on an amd64 host, which alone rules it out.
- **systemd-boot instead of GRUB** — genuinely attractive: simpler, and its native boot
  counting fits the A/B rollback design better than grubenv. Rejected for now only because
  ADR-0008 and the recipes already specify GRUB and it works. Worth revisiting when the
  Phase 8 boot counter is implemented; that is the moment its advantage would pay.

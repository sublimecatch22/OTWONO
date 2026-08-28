# ADR-0013 — Boot the Raspberry Pi 4 through U-Boot's UEFI, not vendor UEFI and not a native kernel handoff

**Status:** accepted · **Date:** 2026-08-23

## Context

The Raspberry Pi 4B is the reference arm64 target for OTWONO. Everything above the
bootloader already runs there in principle: the arm64 recipe builds, the crates
cross-compile, and stage 50 produces a GPT image with an ESP, a standalone GRUB
`BOOTAA64.EFI`, and the kernel on the ESP (ADR-0009). That image boots under QEMU `virt`,
which presents UEFI in firmware.

A Pi 4 does not. Its boot chain is fixed in silicon and EEPROM:

```
VideoCore ROM → EEPROM bootloader → first bootable FAT partition
              → config.txt → start4.elf → whatever config.txt names as `kernel`
```

There is no UEFI anywhere in that path. So either something bridges VideoCore to an EFI
binary, or the Pi gives up EFI and with it GRUB, ADR-0009's uniform bootloader step, and
ADR-0008's A/B design as written. This is the decision that determines how much of the
existing build survives contact with the board, which is why it is made before the code.

Three candidates: the pftf/RPi4 UEFI firmware, the native VideoCore-to-kernel handoff that
Raspberry Pi OS uses, and U-Boot loaded by VideoCore and acting as the EFI provider.

## Evidence

Measured on 2026-08-23 from the OTWONO Cloud dev environment. Package data is
`ports.ubuntu.com` `noble` `main`+`universe`, `binary-arm64`.

| Fact | Measurement |
|---|---|
| `github.com` HTTPS (releases, API, codeload) | rejected by the egress proxy (403) |
| `git clone` of `raspberrypi/firmware`, `raspberrypi/rpi-eeprom`, `pftf/RPi4`, `tianocore/edk2{,-platforms,-non-osi}` | all reachable |
| `raspberrypi/firmware`, blobless sparse checkout of `boot/` | 131 MB; contains `start4.elf` (2.2 MB), `fixup4.dat`, `bcm2711-rpi-4-b.dtb`, 380 overlays; HEAD `06df1d1` (2026-08-21) |
| `boot/LICENCE.broadcom` | binary redistribution permitted, **only** for use on a Raspberry Pi device |
| `pftf/RPi4` | HEAD `6bd9f0a` (2026-05-28), latest tag `v1.52`; submodules not vendored; firmware distributed via GitHub Releases |
| `tianocore/edk2`, blobless shallow clone | 171 MB, 13 further submodules (openssl, mbedtls, libspdm, …) |
| `linux-image-raspi` | present, 6.8.0-1004.4 |
| `rpi-eeprom` | present, 20.4-1ubuntu2 |
| `u-boot-rpi` | present, 2024.01+dfsg-1ubuntu5; ships `rpi_3/`, `rpi_4/`, `rpi_arm64/` binaries **and** their build configs |
| VideoCore firmware as a package (`linux-firmware-raspi`, `raspi-firmware`) | **absent** from noble main and universe for arm64 |
| `u-boot-rpi` config, `rpi_4` and `rpi_arm64` | `CONFIG_EFI_LOADER=y`, `CONFIG_CMD_BOOTEFI_BOOTMGR=y`, `CONFIG_EFI_VARIABLE_FILE_STORE=y`, `CONFIG_FAT_WRITE=y` |
| same configs | `# CONFIG_BOOTCOUNT_LIMIT is not set` |
| Pi 4 EEPROM, GPT | "Add support GPT and Hybrid MBR partition tables" (2020-05); "USB boot fails if the GPT contains no basic data or EFI partitions" fixed the same release |
| Pi 4 EEPROM, A/B | "[tryboot] conditional statement + tryboot_a_b mode", promoted to DEFAULT 2022-12-01 |
| QEMU in this environment | machine models up to `raspi3b`; **no Pi 4 model exists**, so nothing here can be boot-tested |

## Decision

**VideoCore firmware → U-Boot (`rpi_arm64`) → the existing GRUB `BOOTAA64.EFI` → kernel
from the ESP.**

- The ESP that stage 50 already builds also becomes the Pi's boot partition. It gains
  `config.txt`, `start4.elf`, `fixup4.dat`, `bcm2711-rpi-4-b.dtb`, the overlays, and
  `u-boot.bin`; `config.txt` names `u-boot.bin` as the kernel and sets `arm_64bit=1`.
- The GPT + type-`ef00` ESP from stage 50 is left alone. The Pi 4 bootloader has read GPT
  and looked specifically for EFI partitions since 2020, so one partition serves both the
  VideoCore firmware and UEFI. No second FAT partition, no MBR, no change to the layout.
- U-Boot's `EFI_LOADER` runs `EFI/BOOT/BOOTAA64.EFI` unmodified. **Stage 50 does not
  change.** A Pi recipe and a board-firmware stage add the VideoCore blobs and `u-boot.bin`
  to the ESP; everything above the firmware is the arm64 image we already build.
- The VideoCore blobs come from a **pinned commit** of `raspberrypi/firmware`, fetched by
  git (the only transport that works here), with their hashes recorded in the build
  manifest like any other input.

## Consequences

**Good.** The boot architecture stays single. One `grub.cfg`, one standalone EFI binary,
one kernel-on-the-ESP convention across amd64, arm64 `virt`, and the Pi — ADR-0009 holds on
all three, and a change to the boot menu is one change rather than two. Device tree is
preserved end to end, so the Pi's own DTB and overlays drive the kernel exactly as the
vendor intends: GPIO, VPU, and the DMA workaround behave as they do on Raspberry Pi OS.
All of the board's RAM is visible to the OS, which matters more here than on a general
distribution because `otwono-hwd`'s capability vector is derived from total memory and the
AI admission controller sizes models from it. Board support reduces to a pinned blob
checkout plus one package already in the archive we can reach.

**Bad, and worth naming.**

- Four links — VideoCore, U-Boot, GRUB, kernel — is one more than either alternative. Each
  is a place to fail on a headless board with no screen attached, and the serial console
  becomes the only diagnostic that matters.
- U-Boot's `EFI_LOADER` is not EDK2. It is enough to load and run an EFI binary and it can
  store EFI variables on FAT, but it implements a subset; anything that comes to depend on
  richer firmware services will not find them here. Secure boot is out of scope for this
  ADR either way.
- **This contradicts an assumption in ADR-0008.** That ADR names U-Boot `bootcount` as the
  arm64 rollback counter. The packaged `rpi_4` and `rpi_arm64` binaries are built with
  `CONFIG_BOOTCOUNT_LIMIT` off, so as shipped there is no counter. Phase 8 must either
  rebuild U-Boot with it — the source package is in the same archive — or count boots
  elsewhere. This ADR does not settle that; it is recorded as **OQ-14**.
- The VideoCore blobs are redistributable in binary form only for use on a Raspberry Pi
  device. A Pi image therefore carries a non-free, board-restricted component and is not a
  generic arm64 artifact. The pftf route does not avoid this — its release archive carries
  the same blobs.
- The Pi 4 image will be a **separate recipe**, not the existing `arm64-qemu` one. Two
  arm64 images to build and to keep honest.

**Unverified, and this matters.** No part of this has been booted. QEMU in this environment
tops out at `raspi3b`, there is no Pi 4 machine model, and there is no hardware here. Every
statement above is read from vendor sources, EEPROM release notes, and package metadata on
2026-08-23 — not from a boot log. **STATUS: SPECIFIED.** It moves to `VERIFIED` when a
Pi 4B boots this image and the transcript is in `docs/build/VERIFICATION-LOG.md`, and until
then the fallback below stays live.

## Alternatives rejected

- **pftf/RPi4 UEFI firmware.** The attraction is real — genuine EDK2, and GRUB and the
  whole A/B design work with no Pi-specific reasoning at all. Its own `Readme.md` rules it
  out for this product:

  - "A 3 GB RAM limit is enforced __by default__, even if you are using a Raspberry Pi 4
    model that has 4 GB or 8 GB of RAM", liftable only by walking a human through
    `Device Manager → Raspberry Pi Configuration → Advanced Settings`. On an appliance with
    no keyboard that is not a setting, it is a ceiling — and an 8 GB Pi that reports 3 GB
    is assigned the wrong capability tier and offered the wrong models. The one number the
    OS most depends on would be wrong by default.
  - "the published firmwares default to enforcing ACPI as well as a 3 GB RAM limit", while
    the Pi's peripheral support is device-tree. The same file: "Many drivers (GPIO, VPU,
    etc) are still likely to be missing from your OS, and will have to be provided by a
    third party." A hardware-adaptive OS cannot start by discarding the board's hardware
    description.
  - It is distributed through GitHub Releases, which our proxy rejects, so we would build
    `RPI_EFI.fd` from source: edk2 (171 MB, 13 submodules) plus edk2-platforms plus
    edk2-non-osi plus an EDK2 build environment — and reproducibly, per CLAUDE.md §7.
    Against 131 MB of prebuilt, vendor-pinned boot files for the alternative.
  - It does not even remove the proprietary firmware; it adds a layer above it.

- **Native VideoCore → kernel handoff**, as Raspberry Pi OS does it. The best-tested path
  on this board by a wide margin, and the fallback if U-Boot's EFI proves inadequate. But
  it forks the boot architecture: no EFI means no GRUB on this board, so ADR-0009's uniform
  bootloader step stops being uniform, and A/B becomes `autoboot.txt` with `tryboot_a_b`
  — a second, board-specific update mechanism against ADR-0008's one. The measured facts
  say that mechanism is mature and would work; the cost is carrying two update paths
  forever, decided by which board is in front of you. Rejected for that, not for
  capability.

- **Support only boards with mainline UEFI, and wait.** Coherent, and it would keep the
  build trivial. It also excludes the single most common arm64 SBC and most of the class of
  hardware this OS exists to run on.

## References

- ADR-0008 (A/B updates — the `bootcount` assumption this ADR contradicts on the Pi)
- ADR-0009 (mount-free image assembly, kernel on the ESP — unchanged by this decision)
- OQ-3 (kernel strategy per board), OQ-14 (arm64 boot counter)

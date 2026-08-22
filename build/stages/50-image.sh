#!/usr/bin/env bash
# Stage 50 — assemble the bootable disk image: GPT, filesystems, bootloader.
#
# STATUS: NOT IMPLEMENTED — Phase 1. See docs/roadmap/ROADMAP.md.
#
# Network access: none.
# Privileges: root (loop devices, mkfs, bootloader install).
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"
stage_begin 50-image

cat >&2 <<'PLAN'
Stage 50 is not implemented yet. It is the second half of Phase 1.

Intended layout (docs/build/UPDATE-ARCHITECTURE.md):
  GPT: [ OTWONO-ESP 512M vfat ] [ OTWONO-ROOT-A ] [ OTWONO-ROOT-B ] [ OTWONO-DATA ]

What it must do:
  1. truncate a sparse image of [image] size, sgdisk the four partitions
  2. mkfs each partition with the labels stage 20's fstab already expects
  3. populate ROOT-A from the rootfs with deterministic ownership and timestamps
  4. install the bootloader: grub-efi-amd64 / grub-efi-arm64 into the ESP
  5. write the boot-counter environment used for automatic rollback
  6. emit SHA256SUMS and the package manifest lock beside the image

Reproducibility requirements for this stage:
  - mke2fs -d with a fixed UUID derived from the recipe id and SOURCE_DATE_EPOCH
  - no build-host timestamps anywhere in the image
PLAN
die "stage 50 not implemented (Phase 1)"

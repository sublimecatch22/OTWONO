#!/usr/bin/env bash
# Stage 40 — kernel, initramfs, firmware, and (arm64) device trees.
#
# STATUS: NOT IMPLEMENTED — Phase 1. See docs/roadmap/ROADMAP.md.
#
# Network access: the recipe's mirror (kernel and firmware packages).
# Privileges: root (chroot).
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"
stage_begin 40-kernel

cat >&2 <<'PLAN'
Stage 40 is not implemented yet. It is the first half of Phase 1.

What it must do:
  1. apt-get install the [packages] kernel and firmware inside the rootfs
  2. configure initramfs-tools for the target (virtio, ext4, and the A/B root labels)
  3. regenerate the initramfs
  4. arm64: install and stage the board device trees named by the recipe or BSP
  5. record the kernel version, initramfs size, and hashes in the manifest

Deliberately not stubbed out: a stage that silently produced an image with no kernel would
be worse than one that stops here (CLAUDE.md Section 2.1).
PLAN
die "stage 40 not implemented (Phase 1)"

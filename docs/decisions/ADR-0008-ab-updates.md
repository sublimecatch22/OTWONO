# ADR-0008 — A/B image updates with automatic rollback

**Status:** accepted · **Date:** 2026-08-22

## Context

A headless node in a shed or on a mast must survive a bad update without someone carrying a
keyboard to it. Package-manager upgrades can fail halfway and leave a system that is
neither the old one nor the new one.

## Decision

Two root slots plus a persistent data partition. Write the inactive slot, verify the
signature and hash, flip the boot pointer, reboot, and confirm health from userspace. A
boot-attempt counter (GRUB env on amd64, U-Boot `bootcount` on arm64) rolls back
automatically. Bundles are content-addressed, signed, `REPLICATED`, and therefore
distributable over the mesh. Implementation is an existing tool — RAUC or
systemd-sysupdate, settled in OQ-5 — never our own.

## Consequences

**Good:** an interrupted update cannot brick a device, because the running slot is never
touched; rollback needs no network and no human; content-addressed signed bundles can be
relayed by untrusted peers safely; delta transfer works on a `Narrow` link.

**Bad:** roughly double the root storage — painful on a 8 GB eMMC SBC, which is why the data
partition is separate and the root slot is kept small; the health check must be meaningful
or rollback protects nothing; per-architecture bootloader integration is real, fiddly work.

## Note added 2026-08-23 (ADR-0013)

The arm64 half of the boot-attempt counter above needs revisiting. The `u-boot-rpi`
package in the archive is built with `CONFIG_BOOTCOUNT_LIMIT` off, so on a Raspberry Pi 4
there is no `bootcount` as shipped. Rebuilding U-Boot or counting elsewhere is **OQ-14**,
to be settled in Phase 8. Nothing else in this ADR changes.

## Alternatives rejected

- **apt upgrade in place** — no atomicity, no rollback, and a failure mode that requires
  physical access.
- **OSTree** — technically strong and worth revisiting, but adds a second packaging
  paradigm on top of Debian; A/B is simpler to reason about for a first release.
- **Full reimage from the network** — needs bandwidth we will not always have.

#!/usr/bin/env bash
# Stage 30 — install the OTWONO layer: binaries, schemas, default policy, systemd units.
#
# Network access: none.
# Privileges: root (writes into the rootfs).
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"
stage_begin 30-otwono

ROOTFS="$TARGET_OUT/rootfs"
TOOLS="$TARGET_OUT/host-tools"
[ -d "$ROOTFS/usr" ] || die "no rootfs at $ROOTFS; run stage 10 first"
[ -d "$TOOLS/bin" ]  || die "no staged binaries; run stage 05 first"

# Not skipped when already complete: this stage installs the OTWONO layer itself, so it
# must re-run whenever the binaries are rebuilt. It is cheap and fully idempotent.
require_root

log "installing binaries"
install -d -m 0755 "$ROOTFS/usr/bin"
for b in "$TOOLS/bin/"*; do
    install -m 0755 "$b" "$ROOTFS/usr/bin/$(basename "$b")"
    log "  /usr/bin/$(basename "$b")"
done

log "installing schemas"
install -d -m 0755 "$ROOTFS/usr/share/otwono/schemas"
install -m 0644 "$REPO_ROOT"/schemas/*.json "$ROOTFS/usr/share/otwono/schemas/"

log "creating the OTWONO state directories"
install -d -m 0755 "$ROOTFS/etc/otwono" "$ROOTFS/etc/otwono/policy.d"
install -d -m 0700 "$ROOTFS/var/lib/otwono" "$ROOTFS/var/lib/otwono/identity"
install -d -m 0755 "$ROOTFS/var/log/otwono"

log "installing the first-boot capability report unit"
install -d -m 0755 "$ROOTFS/etc/systemd/system"
cat > "$ROOTFS/etc/systemd/system/otwono-capability-report.service" <<'UNIT'
[Unit]
Description=OTWONO capability profile report
Documentation=file:/usr/share/doc/otwono/CAPABILITY-TIERS.md
After=local-fs.target
Before=multi-user.target
# Ordering against local-fs.target is NOT enough: /var/lib/otwono is mounted `nofail`, and
# per systemd.mount(5) a nofail mount is not ordered before local-fs.target. Without this
# the service wins the race, writes into the directory underneath the mount point, and the
# data lands on the root filesystem — which an A/B update replaces. Every OTWONO unit that
# touches persistent state needs this line.
RequiresMountsFor=/var/lib/otwono

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/libexec/otwono/first-boot-capability-report
StandardOutput=journal+console

# Hardening baseline (docs/security/SECURITY-MODEL.md Section 5). This unit only reads
# /proc and /sys and writes one file, so it can be locked down hard.
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
# Deliberately NOT PrivateNetwork=yes: sysfs's net class is namespaced, so a private
# netns would show the probe only `lo` and the profile would report the machine offline.
RestrictAddressFamilies=
ReadWritePaths=/var/lib/otwono
MemoryDenyWriteExecute=yes
RestrictSUIDSGID=yes
LockPersonality=yes

[Install]
WantedBy=multi-user.target
UNIT
chroot "$ROOTFS" systemctl enable otwono-capability-report.service 2>/dev/null \
    || warn "could not enable otwono-capability-report.service"

log "installing the first-boot capability report script"
install -d -m 0755 "$ROOTFS/usr/libexec/otwono"
cat > "$ROOTFS/usr/libexec/otwono/first-boot-capability-report" <<'SCRIPT'
#!/bin/sh
# Write the capability profile where other subsystems read it, print it for a human, and
# emit one unambiguous marker line.
#
# The marker matters: "Starting OTWONO capability profile report..." appears in the console
# log whether or not the report succeeded, so a boot test that grepped for the unit
# description would pass on a broken build.
set -eu

OUT=/var/lib/otwono/capability-profile.json
TMP="$OUT.tmp"

if ! /usr/bin/otwono-hwctl profile --json > "$TMP"; then
    echo "OTWONO-CAPABILITY-FAILED: could not generate the profile"
    exit 1
fi
mv "$TMP" "$OUT"
# Flush to the block device. A node may lose power at any time, and the boot test
# deliberately kills the VM rather than shutting it down cleanly.
sync

/usr/bin/otwono-hwctl profile || true

TIER=$(/usr/bin/otwono-hwctl tier 2>/dev/null || echo UNKNOWN)
echo "OTWONO-CAPABILITY-OK tier=$TIER profile=$OUT bytes=$(wc -c < "$OUT")"
SCRIPT
chmod 0755 "$ROOTFS/usr/libexec/otwono/first-boot-capability-report"

log "installing documentation"
install -d -m 0755 "$ROOTFS/usr/share/doc/otwono"
install -m 0644 "$REPO_ROOT/docs/hardware/CAPABILITY-TIERS.md" "$ROOTFS/usr/share/doc/otwono/"
install -m 0644 "$REPO_ROOT/README.md" "$ROOTFS/usr/share/doc/otwono/"

manifest_add "otwono-layer" "binaries, schemas, units installed"
stage_mark_complete 30-otwono
stage_done

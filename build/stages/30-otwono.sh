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

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c '/usr/bin/otwono-hwctl profile --json > /var/lib/otwono/capability-profile.json'
ExecStartPost=/usr/bin/otwono-hwctl profile
StandardOutput=journal+console

# Hardening baseline (docs/security/SECURITY-MODEL.md Section 5). This unit only reads
# /proc and /sys and writes one file, so it can be locked down hard.
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateNetwork=yes
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

log "installing documentation"
install -d -m 0755 "$ROOTFS/usr/share/doc/otwono"
install -m 0644 "$REPO_ROOT/docs/hardware/CAPABILITY-TIERS.md" "$ROOTFS/usr/share/doc/otwono/"
install -m 0644 "$REPO_ROOT/README.md" "$ROOTFS/usr/share/doc/otwono/"

manifest_add "otwono-layer" "binaries, schemas, units installed"
stage_mark_complete 30-otwono
stage_done

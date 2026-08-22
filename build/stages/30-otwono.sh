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
ensure_foreign_arch_support "$(recipe_get target arch)"

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

log "installing the default policy"
# Fail-closed by design: this grants root the two read-only capabilities the system needs
# to inspect itself, and nothing else. Every other action is denied until an operator adds
# a rule (docs/security/SECURITY-MODEL.md).
cat > "$ROOTFS/etc/otwono/policy.d/10-default.toml" <<'POLICY'
# OTWONO default policy. Shipped conservative on purpose.
#
# The permission broker denies anything no rule matches, so this file is the entire set of
# things the system may do without an operator explicitly widening it.

[[rule]]
action = "hw.read"
subjects = ["uid:0"]
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "audit.read"
subjects = ["uid:0"]
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "net.read"
subjects = ["uid:0"]
decision = "allow"
ttl_seconds = 300
POLICY

log "installing the control-plane runtime directory"
install -d -m 0755 "$ROOTFS/usr/lib/tmpfiles.d"
cat > "$ROOTFS/usr/lib/tmpfiles.d/otwono.conf" <<'TMPFILES'
d /run/otwono 0755 root root -
d /var/log/otwono 0750 root root -
TMPFILES

log "installing the daemon units"
# Hardening baseline from docs/security/SECURITY-MODEL.md Section 5. Two notes that are
# easy to get wrong and were both learned the hard way:
#   * ProtectSystem=strict makes /run read-only too, so each daemon needs its socket
#     directory in ReadWritePaths.
#   * PrivateNetwork is safe for the broker (AF_UNIX is not network-namespaced) but NOT
#     for otwono-hwd, which must see the host's interfaces to classify the network axis.
cat > "$ROOTFS/etc/systemd/system/otwono-permd.service" <<'UNIT'
[Unit]
Description=OTWONO permission broker
Documentation=file:/usr/share/doc/otwono/SECURITY-MODEL.md
After=systemd-tmpfiles-setup.service local-fs.target
Requires=systemd-tmpfiles-setup.service
Before=otwono-hwd.service

[Service]
Type=exec
ExecStart=/usr/bin/otwono-permd --socket /run/otwono/perm.sock --policy-dir /etc/otwono/policy.d --audit-log /var/log/otwono/audit.jsonl
Restart=on-failure
RestartSec=2

NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
# The broker needs no network at all. AF_UNIX sockets live in the filesystem and are
# unaffected by a private network namespace, so this costs nothing.
PrivateNetwork=yes
RestrictAddressFamilies=AF_UNIX
ReadWritePaths=/run/otwono /var/log/otwono
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
ProtectClock=yes
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM
CapabilityBoundingSet=
AmbientCapabilities=

[Install]
WantedBy=multi-user.target
UNIT

cat > "$ROOTFS/etc/systemd/system/otwono-hwd.service" <<'UNIT'
[Unit]
Description=OTWONO hardware daemon
Documentation=file:/usr/share/doc/otwono/CAPABILITY-TIERS.md
After=otwono-permd.service systemd-tmpfiles-setup.service
Requires=otwono-permd.service

[Service]
Type=exec
ExecStart=/usr/bin/otwono-hwd --socket /run/otwono/hw.sock --perm-socket /run/otwono/perm.sock
Restart=on-failure
RestartSec=2

NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
# Deliberately NOT PrivateNetwork: sysfs's net class is namespaced, and a private netns
# would leave the probe seeing only `lo`, so the profile would report the node offline.
RestrictAddressFamilies=AF_UNIX
ReadWritePaths=/run/otwono
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
ProtectClock=yes
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM
CapabilityBoundingSet=
AmbientCapabilities=

[Install]
WantedBy=multi-user.target
UNIT

log "installing the identity and mesh units"
cat > "$ROOTFS/etc/systemd/system/otwono-idd.service" <<'UNIT'
[Unit]
Description=OTWONO identity daemon
Documentation=file:/usr/share/doc/otwono/NODE-IDENTITY.md
After=otwono-permd.service systemd-tmpfiles-setup.service
Requires=otwono-permd.service
RequiresMountsFor=/var/lib/otwono
Before=otwono-netd.service

[Service]
Type=exec
ExecStart=/usr/bin/otwono-idd --socket /run/otwono/id.sock --perm-socket /run/otwono/perm.sock
Restart=on-failure
RestartSec=2

NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
# The identity daemon holds the node key and has no business on the network.
PrivateNetwork=yes
RestrictAddressFamilies=AF_UNIX
ReadWritePaths=/run/otwono /var/lib/otwono
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
ProtectClock=yes
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM
CapabilityBoundingSet=
AmbientCapabilities=

[Install]
WantedBy=multi-user.target
UNIT

cat > "$ROOTFS/etc/systemd/system/otwono-netd.service" <<'UNIT'
[Unit]
Description=OTWONO node mesh daemon
Documentation=file:/usr/share/doc/otwono/NODE-NETWORK.md
After=otwono-idd.service systemd-networkd.service systemd-tmpfiles-setup.service
Requires=otwono-permd.service
# network.target says "networking has been started", not "an interface has an address".
# mDNS binds its sockets at startup, so a daemon that starts before addressing completes
# announces on nothing. Wants (not Requires) so a node with no usable link still boots and
# still serves its local control plane — an OTWONO node offline is a supported state.
Wants=network-online.target
After=network-online.target
RequiresMountsFor=/var/lib/otwono

[Service]
Type=exec
ExecStart=/usr/bin/otwono-netd --socket /run/otwono/net.sock --perm-socket /run/otwono/perm.sock
Restart=on-failure
RestartSec=2

NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
# This daemon is the hostile-input boundary (Z3), so it faces the network by
# definition. AF_NETLINK is needed to enumerate interfaces for mDNS.
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
ReadWritePaths=/run/otwono /var/lib/otwono
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
ProtectClock=yes
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
AmbientCapabilities=

[Install]
WantedBy=multi-user.target
UNIT

log "installing link-local networking for the mesh"
# The mesh must come up on a segment with no DHCP server — two directly-connected nodes,
# a field deployment, an ad-hoc radio link. IPv4 link-local gives every interface an
# address without one. DHCP is still preferred where it exists; this is the fallback.
install -d -m 0755 "$ROOTFS/etc/systemd/network"
cat > "$ROOTFS/etc/systemd/network/50-otwono-mesh.network" <<'NETWORK'
[Match]
Name=en* eth*

[Network]
DHCP=yes
# Without this a segment with no DHCP server leaves the interface addressless and the
# mesh cannot form at all.
LinkLocalAddressing=yes
IPv6AcceptRA=yes
MulticastDNS=yes

[DHCPv4]
UseDNS=yes
NETWORK
chroot "$ROOTFS" systemctl enable systemd-networkd.service 2>/dev/null \
    || warn "could not enable systemd-networkd"

log "installing the first-boot mesh check"
install -d -m 0755 "$ROOTFS/usr/lib/otwono"
install -m 0755 "$BUILD_DIR/files/otwono-mesh-check" "$ROOTFS/usr/lib/otwono/mesh-check"
cat > "$ROOTFS/etc/systemd/system/otwono-mesh-check.service" <<'UNIT'
[Unit]
Description=OTWONO mesh self check
After=otwono-netd.service
Requires=otwono-netd.service
RequiresMountsFor=/var/lib/otwono
Before=multi-user.target

[Service]
# Deliberately not RemainAfterExit: the timer re-runs this so the console carries a
# current peer count. Piping otwono-netd's own output to the console instead would work
# for a test and be unusable on a real headless node.
Type=oneshot
ExecStart=/usr/lib/otwono/mesh-check
StandardOutput=journal+console
StandardError=journal+console

NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
RestrictSUIDSGID=yes
LockPersonality=yes

[Install]
WantedBy=multi-user.target
UNIT

cat > "$ROOTFS/etc/systemd/system/otwono-mesh-check.timer" <<'UNIT'
[Unit]
Description=Periodic OTWONO mesh status on the console

[Timer]
# Discovery needs a moment after boot, so the first run at multi-user.target always
# reports zero peers. Repeating gives an operator on a serial console a live count, and
# gives the two-node test something to wait on.
OnBootSec=25s
OnUnitActiveSec=20s
AccuracySec=1s

[Install]
WantedBy=timers.target
UNIT

log "installing the first-boot control-plane check"
install -d -m 0755 "$ROOTFS/usr/lib/otwono"
install -m 0755 "$BUILD_DIR/files/otwono-control-plane-check" \
    "$ROOTFS/usr/lib/otwono/control-plane-check"
cat > "$ROOTFS/etc/systemd/system/otwono-control-plane-check.service" <<'UNIT'
[Unit]
Description=OTWONO control-plane self check
After=otwono-hwd.service
Requires=otwono-hwd.service
RequiresMountsFor=/var/lib/otwono /var/log/otwono
Before=multi-user.target

[Service]
Type=oneshot
RemainAfterExit=yes
# Proves the whole path end to end from inside the running system: ask the broker for a
# capability, call the hardware daemon with it, and confirm the audit log recorded it.
ExecStart=/usr/lib/otwono/control-plane-check
StandardOutput=journal+console
StandardError=journal+console

NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
ReadWritePaths=/var/lib/otwono
RestrictSUIDSGID=yes
LockPersonality=yes

[Install]
WantedBy=multi-user.target
UNIT

for unit in otwono-permd otwono-hwd otwono-idd otwono-netd otwono-control-plane-check otwono-mesh-check otwono-mesh-check.timer; do
    # The list carries a .timer as well as services, so only append .service when the
    # entry does not already name a unit type.
    case "$unit" in
        *.timer) target="$unit" ;;
        *)       target="$unit.service" ;;
    esac
    chroot "$ROOTFS" systemctl enable "$target" 2>/dev/null \
        || warn "could not enable $target"
done

log "installing documentation"
install -d -m 0755 "$ROOTFS/usr/share/doc/otwono"
install -m 0644 "$REPO_ROOT/docs/hardware/CAPABILITY-TIERS.md" "$ROOTFS/usr/share/doc/otwono/"
install -m 0644 "$REPO_ROOT/docs/security/SECURITY-MODEL.md" "$ROOTFS/usr/share/doc/otwono/"
install -m 0644 "$REPO_ROOT/docs/network/NODE-IDENTITY.md" "$ROOTFS/usr/share/doc/otwono/"
install -m 0644 "$REPO_ROOT/docs/network/NODE-NETWORK.md" "$ROOTFS/usr/share/doc/otwono/"
install -m 0644 "$REPO_ROOT/README.md" "$ROOTFS/usr/share/doc/otwono/"

manifest_add "otwono-layer" "binaries, schemas, policy, 6 units installed"
stage_mark_complete 30-otwono
stage_done

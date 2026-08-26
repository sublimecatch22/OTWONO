#!/usr/bin/env bash
# Stage 37 — a boot-time check that content crosses a link and PRIVATE content does not.
#
# Network access: none.
# Privileges: root (writes into the rootfs).
#
# Off by default, and it must stay that way: this stage puts a **widened permission policy**
# into the image, granting the two capabilities that are the network boundary itself.
# Enable with MESH_CONTENT_SMOKE=1, and run the result under build/qemu/two-node-test.sh --
# a single node has nobody to fetch from and the check will time out waiting for a peer.
#
# Why it exists: "a PRIVATE object never appears on any link" is Phase 5's exit criterion
# and has until now been proven by a test that constructed daemons inside one process. That
# proves the method. It does not prove the wire, the units, the policy, or the Noise channel
# between two machines that authenticated each other.
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"
stage_begin 37-mesh-content-smoke

ROOTFS="$TARGET_OUT/rootfs"
[ -d "$ROOTFS/usr" ] || die "no rootfs at $ROOTFS; run stage 10 first"

SMOKE="${MESH_CONTENT_SMOKE:-$(recipe_get_opt mesh content_smoke)}"
if [ -z "$SMOKE" ] || [ "$SMOKE" = "0" ]; then
    log "no mesh content smoke requested; this image grants no store.serve and no net.content"
    manifest_add "mesh-content-smoke" "none"
    stage_mark_complete 37-mesh-content-smoke
    stage_done
    exit 0
fi

require_root
[ -x "$ROOTFS/usr/bin/otwono-storectl" ] \
    || die "the mesh content check needs otwono-storectl in the image; stage 30 must run first"
[ -x "$ROOTFS/usr/bin/otwono-netd" ] \
    || die "the mesh content check needs otwono-netd in the image; stage 30 must run first"

# These two are the boundary. The default policy grants them to nobody on purpose
# (ADR-0015, ADR-0017), so the drop-in says loudly what it is.
log "installing the mesh content smoke policy drop-in"
cat > "$ROOTFS/etc/otwono/policy.d/91-mesh-content-smoke.toml" <<'POLICY'
# BUILT FOR TESTING. This file grants the two capabilities that are the network boundary:
# store.serve, which hands an object to a peer, and net.content, which fetches one.
#
# It is installed only by build stage 37 (MESH_CONTENT_SMOKE=1). A release image ships
# neither grant, because serving the street and holding a neighbour's content are costs an
# operator agrees to rather than defaults. If you are reading this on a machine you care
# about, delete it: 10-default.toml grants neither.

[[rule]]
action = "store.serve"
subjects = ["uid:0"]
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "net.content"
subjects = ["uid:0"]
decision = "allow"
ttl_seconds = 300
POLICY
chmod 0644 "$ROOTFS/etc/otwono/policy.d/91-mesh-content-smoke.toml"

# The VMs have about 1.5 GiB of data partition, and classify_storage calls anything under
# 16 GiB Constrained -- correctly, for a real board. A Constrained node's cluster cache
# budget is zero, so otwono-stored opens no cache and the cache has never run on a booted
# node at all. Growing the image past 22 GiB to change that would make every build and boot
# far slower for one axis.
#
# So the test image overrides the axis instead, which is what overrides are for: an operator
# saying they know better than the probe. The detected value is preserved in the profile, so
# the check can still see that the machine really is small. This also gives the override
# mechanism its first boot-time exercise.
log "installing the capability override that gives this test node a cache"
cat > "$ROOTFS/etc/otwono/capability.override.toml" <<'OVERRIDE'
# BUILT FOR TESTING. Installed only by build stage 37 (MESH_CONTENT_SMOKE=1).
#
# These VMs have a small data partition and are correctly classified as storage-constrained,
# which sets the cluster cache budget to zero. That is right for a real board and
# leaves the cache untested on a booted node, so this claims more storage than there is.
#
# The detected value is kept in the capability profile alongside the forced one. If you are
# reading this on a machine you care about, delete it.
[axes]
storage = "standard"
OVERRIDE
chmod 0644 "$ROOTFS/etc/otwono/capability.override.toml"

log "installing the mesh content check"
install -m 0755 "$BUILD_DIR/files/otwono-mesh-content-check" \
    "$ROOTFS/usr/lib/otwono/mesh-content-check"
cat > "$ROOTFS/etc/systemd/system/otwono-mesh-content-check.service" <<'UNIT'
[Unit]
Description=OTWONO mesh content check (TEST IMAGES ONLY)
Documentation=file:/usr/share/doc/otwono/DATA-VISIBILITY.md
After=otwono-netd.service otwono-stored.service
Requires=otwono-netd.service otwono-stored.service
RequiresMountsFor=/var/lib/otwono

[Service]
Type=oneshot
RemainAfterExit=yes
# It waits for a peer, so it must not hold up the boot: a single node would otherwise sit
# at multi-user.target for ten minutes discovering nobody. Deliberately not Before=.
ExecStart=/usr/lib/otwono/mesh-content-check
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

chroot "$ROOTFS" systemctl enable otwono-mesh-content-check.service 2>/dev/null \
    || warn "could not enable otwono-mesh-content-check.service"

log "NOTE: this image grants store.serve and net.content. It is a test image."
manifest_add "mesh-content-smoke" "policy drop-in and boot check installed"
stage_mark_complete 37-mesh-content-smoke
stage_done

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

# The third boundary capability, and the one that acts on its own: with this granted, a node
# takes a copy of what a peer offers when it dials, without anything asking it to (ADR-0026).
# That is precisely what the check below is for, and precisely why a release image does not
# grant it -- holding a stranger's bytes is a cost an operator agrees to, not a default.
[[rule]]
action = "cache.replicate"
subjects = ["uid:0"]
decision = "allow"
ttl_seconds = 300

# Publishing a pointer needs three things this image grants together and a release image
# grants none of: pointer.read to learn the next sequence, id.sign to sign the record with
# the node key, and pointer.publish to store it (ADR-0027).
#
# id.sign is the one worth pausing on. It is a general signing oracle for the application
# domain -- a caller who holds it can have the node sign anything that is not a protocol
# message -- which is why 10-default.toml grants id.sign_session and id.unwrap_shared and
# deliberately not this. An image that hands it to uid:0 is a test image.
[[rule]]
action = "pointer.read"
subjects = ["uid:0"]
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "pointer.publish"
subjects = ["uid:0"]
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "id.sign"
subjects = ["uid:0"]
decision = "allow"
ttl_seconds = 300

# Reading a peer's pointer needs this, and reading one without it is refused rather than
# done blind. The rollback defence is the reader's memory of the highest sequence it has
# seen (ADR-0027 §1), that memory lives in otwono-stored, and otwono-netd reaches it over
# the control plane -- so a node granted net.content but not this cannot read pointers at
# all. That is the intended failure: a reader that cannot remember does not read.
#
# Reversible, unlike pointer.publish: recording what a peer said says nothing to anyone.
[[rule]]
action = "pointer.write"
subjects = ["uid:0"]
decision = "allow"
ttl_seconds = 300

# Carrying other people's mail (ADR-0028 §8). Deliberately NOT implied by cache.replicate:
# holding neighbourhood content an operator can inspect and purge is a different thing to
# agree to than holding opaque ciphertext addressed to a stranger, and a release image grants
# neither. An image that hands this to uid:0 is a test image.
[[rule]]
action = "envelope.carry"
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
# Runs on the first boot only. The second boot belongs to otwono-pointer-reboot-check, and
# a second run of this one would republish a name the peer has already seen at sequence 2
# with different bytes -- equivocation (ADR-0027 section 8), correctly refused, and nothing
# to do with what either check exists to prove.
ConditionPathExists=!/var/lib/otwono/mesh-content-check.done

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

# The mirror image of the check above: it runs only on a boot that follows a completed one.
# ADR-0027's defence is state the reader keeps, so "does that state survive the machine going
# away" is the property the whole design rests on, and it was tested in-process only.
log "installing the pointer reboot check"
install -m 0755 "$BUILD_DIR/files/otwono-pointer-reboot-check" \
    "$ROOTFS/usr/lib/otwono/pointer-reboot-check"
cat > "$ROOTFS/etc/systemd/system/otwono-pointer-reboot-check.service" <<'UNIT'
[Unit]
Description=OTWONO pointer reboot check (TEST IMAGES ONLY)
Documentation=file:/usr/share/doc/otwono/DISTRIBUTED-SERVICES.md
After=otwono-netd.service otwono-stored.service
Requires=otwono-netd.service otwono-stored.service
RequiresMountsFor=/var/lib/otwono
# Only on a boot that follows a completed content check. On a first boot there is no
# remembered sequence to survive anything, and the stamp is what says otherwise.
ConditionPathExists=/var/lib/otwono/mesh-content-check.done

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/lib/otwono/pointer-reboot-check
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

chroot "$ROOTFS" systemctl enable otwono-pointer-reboot-check.service 2>/dev/null \
    || warn "could not enable otwono-pointer-reboot-check.service"

# The store-and-forward check. Three nodes minimum: the property is "the sender is absent
# when the recipient collects", which needs a carrier that is neither party (ADR-0028). On
# fewer nodes the script says so and stops rather than appearing to test something.
log "installing the envelope check"
install -m 0755 "$BUILD_DIR/files/otwono-envelope-check" \
    "$ROOTFS/usr/lib/otwono/envelope-check"
cat > "$ROOTFS/etc/systemd/system/otwono-envelope-check.service" <<'UNIT'
[Unit]
Description=OTWONO envelope check (TEST IMAGES ONLY)
Documentation=file:/usr/share/doc/otwono/DISTRIBUTED-SERVICES.md
After=otwono-netd.service otwono-stored.service otwono-mesh-content-check.service
Requires=otwono-netd.service otwono-stored.service
RequiresMountsFor=/var/lib/otwono

[Service]
Type=oneshot
RemainAfterExit=yes
# It waits for a peer to go away, which is a thing the harness does on its own schedule, so
# it must not hold up the boot. Deliberately not Before=.
ExecStart=/usr/lib/otwono/envelope-check
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

chroot "$ROOTFS" systemctl enable otwono-envelope-check.service 2>/dev/null \
    || warn "could not enable otwono-envelope-check.service"

# The partition phase, after the envelope one. Ordered `After=` rather than `Requires=`: the
# envelope check can legitimately skip — two nodes, or node 3's first boot — and a partition
# check that refused to start because of that would turn a skip into a stall.
install -m 0755 "$BUILD_DIR/files/otwono-partition-check" \
    "$ROOTFS/usr/lib/otwono/partition-check"
cat > "$ROOTFS/etc/systemd/system/otwono-partition-check.service" <<'UNIT'
[Unit]
Description=OTWONO partition check (TEST IMAGES ONLY)
Documentation=file:/usr/share/doc/otwono/DISTRIBUTED-SERVICES.md
After=otwono-netd.service otwono-stored.service otwono-envelope-check.service
Requires=otwono-netd.service otwono-stored.service
RequiresMountsFor=/var/lib/otwono

[Service]
Type=oneshot
RemainAfterExit=yes
# Waits for a link the harness takes down on its own schedule, so it must not hold up the
# boot. Deliberately not Before=.
ExecStart=/usr/lib/otwono/partition-check
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

chroot "$ROOTFS" systemctl enable otwono-partition-check.service 2>/dev/null \
    || warn "could not enable otwono-partition-check.service"

# The wiki service (ADR-0032), which is the first thing in the catalogue built on top of the
# three primitives rather than beside them. Ordered after the content check so the mesh has
# already formed and the two are not racing for the same peers.
install -m 0755 "$BUILD_DIR/files/otwono-wiki-check" "$ROOTFS/usr/lib/otwono/wiki-check"
cat > "$ROOTFS/etc/systemd/system/otwono-wiki-check.service" <<'UNIT'
[Unit]
Description=OTWONO wiki check (TEST IMAGES ONLY)
Documentation=file:/usr/share/doc/otwono/WIKI.md
After=otwono-netd.service otwono-stored.service otwono-idd.service otwono-mesh-content-check.service
Requires=otwono-netd.service otwono-stored.service otwono-idd.service
RequiresMountsFor=/var/lib/otwono

[Service]
Type=oneshot
RemainAfterExit=yes
# Waits for a peer to publish, which happens on that peer's own schedule, so it must not
# hold up the boot. Deliberately not Before=.
ExecStart=/usr/lib/otwono/wiki-check
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

chroot "$ROOTFS" systemctl enable otwono-wiki-check.service 2>/dev/null \
    || warn "could not enable otwono-wiki-check.service"

log "NOTE: this image grants store.serve, net.content, cache.replicate, id.sign, the pointer capabilities and envelope.carry. It is a test image."
manifest_add "mesh-content-smoke" "policy drop-in and boot check installed"
stage_mark_complete 37-mesh-content-smoke
stage_done

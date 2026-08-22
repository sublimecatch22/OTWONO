#!/usr/bin/env bash
# Stage 10 — bootstrap a minimal base rootfs for the target architecture.
#
# Network access: the recipe's [base] mirror.
# Privileges: root (rootfs ownership, device nodes, and the foreign-arch second stage).
#
# Output: $TARGET_OUT/rootfs/
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"
stage_begin 10-bootstrap

ROOTFS="$TARGET_OUT/rootfs"

if stage_is_complete 10-bootstrap && [ -d "$ROOTFS/usr" ]; then
    log "rootfs already bootstrapped at $ROOTFS; skipping (rm -rf it to force)"
    stage_done
    exit 0
fi

require_root
require_tool debootstrap
require_space_gib 3

ARCH="$(recipe_get target arch)"
SUITE="$(recipe_get base suite)"
MIRROR="$(recipe_get base mirror)"
COMPONENTS="$(recipe_get base components main)"
SNAPSHOT="$(recipe_get base snapshot "none")"
HOST_ARCH="$(dpkg --print-architecture)"

[ "$SNAPSHOT" = "none" ] || warn "snapshot pinning is not wired up yet (Phase 1); this build is not reproducible"

log "arch=$ARCH suite=$SUITE mirror=$MIRROR host=$HOST_ARCH"

# Fail early and clearly if the mirror is unreachable — the alternative is a debootstrap
# failure several minutes in with a much worse message.
if ! curl -fsS -o /dev/null -m 25 "$MIRROR/dists/$SUITE/Release"; then
    die "mirror unreachable: $MIRROR/dists/$SUITE/Release
       In the OTWONO Cloud dev environment the egress proxy permits Ubuntu mirrors and
       rejects Debian ones. Try: make TARGET=${TARGET%-ubuntu}-ubuntu rootfs"
fi

FOREIGN=0
if [ "$ARCH" != "$HOST_ARCH" ]; then
    FOREIGN=1
    log "cross-architecture bootstrap ($HOST_ARCH host, $ARCH target)"
    require_tool "qemu-${ARCH/arm64/aarch64}-static" "package: qemu-user-static"

    ensure_foreign_arch_support "$ARCH"
fi

rm -rf "$ROOTFS"
mkdir -p "$ROOTFS"

DEBOOTSTRAP_ARGS=(
    --arch="$ARCH"
    --variant=minbase
    --components="${COMPONENTS// /,}"
    --include=ca-certificates
)
[ "$FOREIGN" = 1 ] && DEBOOTSTRAP_ARGS+=(--foreign)

log "running debootstrap (this takes several minutes)"
debootstrap "${DEBOOTSTRAP_ARGS[@]}" "$SUITE" "$ROOTFS" "$MIRROR" \
    2>&1 | tee "$TARGET_OUT/debootstrap.log" | grep -E '^(I:|W:|E:)' | tail -20

if [ "$FOREIGN" = 1 ]; then
    log "running the foreign second stage under qemu-user"
    cp "/usr/bin/qemu-${ARCH/arm64/aarch64}-static" "$ROOTFS/usr/bin/"
    chroot "$ROOTFS" /debootstrap/debootstrap --second-stage \
        2>&1 | tee -a "$TARGET_OUT/debootstrap.log" | tail -20
fi

[ -x "$ROOTFS/bin/sh" ] || [ -x "$ROOTFS/usr/bin/sh" ] || die "bootstrapped rootfs has no shell"

SIZE=$(du -sm "$ROOTFS" | cut -f1)
log "rootfs is ${SIZE} MiB"
manifest_add "rootfs" "$SUITE/$ARCH from $MIRROR, ${SIZE} MiB"
stage_mark_complete 10-bootstrap
stage_done

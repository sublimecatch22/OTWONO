#!/usr/bin/env bash
# Stage 20 — base OS configuration: apt sources, locale, hostname, users, fstab, presets.
#
# Network access: the recipe's mirror, to install [packages] include and firmware.
# Privileges: root (chroot).
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"
stage_begin 20-base-config

ROOTFS="$TARGET_OUT/rootfs"
[ -d "$ROOTFS/usr" ] || die "no rootfs at $ROOTFS; run stage 10 first"

if stage_is_complete 20-base-config; then
    log "already configured; skipping (make clean to force)"
    stage_done
    exit 0
fi

require_root

ARCH="$(recipe_get target arch)"
SUITE="$(recipe_get base suite)"
MIRROR="$(recipe_get base mirror)"
COMPONENTS="$(recipe_get base components main)"
PACKAGES="$(recipe_get packages include "systemd systemd-sysv udev dbus ca-certificates")"
CONSOLE="$(recipe_get boot console "ttyS0,115200")"

log "writing apt sources"
mkdir -p "$ROOTFS/etc/apt/sources.list.d"
cat > "$ROOTFS/etc/apt/sources.list" <<APT
deb $MIRROR $SUITE $COMPONENTS
APT

log "writing base identity and locale"
echo "otwono" > "$ROOTFS/etc/hostname"
cat > "$ROOTFS/etc/hosts" <<'HOSTS'
127.0.0.1	localhost
127.0.1.1	otwono
::1	localhost ip6-localhost ip6-loopback
HOSTS
mkdir -p "$ROOTFS/etc/default"
echo 'LANG=C.UTF-8' > "$ROOTFS/etc/locale.conf"

# fstab matches the A/B layout stage 50 creates. Labels, not UUIDs: the image is written
# to many devices and a UUID baked at build time would be wrong on all but one of them.
log "writing fstab for the A/B layout"
#
# Root is mounted rw for now. The A/B design wants an immutable root, but that also needs
# /var moved off the root filesystem (tmpfs or the data partition) and the corresponding
# systemd wiring. Doing half of it — ro root with /var still on it — does not boot. The
# immutable root lands with the update work in Phase 8; the partition layout is already
# correct for it.
cat > "$ROOTFS/etc/fstab" <<'FSTAB'
LABEL=OTWONO-ROOT-A  /                ext4  rw,noatime,errors=remount-ro  0 1
LABEL=OTWONO-ESP     /boot/efi        vfat  umask=0077,nofail             0 2
LABEL=OTWONO-DATA    /var/lib/otwono  ext4  rw,noatime,nofail             0 2
FSTAB

log "installing base packages inside the chroot"
mount --bind /dev "$ROOTFS/dev"
mount -t proc proc "$ROOTFS/proc"
mount -t sysfs sys "$ROOTFS/sys"
cleanup() { umount -l "$ROOTFS/dev" "$ROOTFS/proc" "$ROOTFS/sys" 2>/dev/null || true; }
trap cleanup EXIT

chroot "$ROOTFS" /bin/sh -c "
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y --no-install-recommends $PACKAGES
    apt-get clean
    rm -rf /var/lib/apt/lists/*
" 2>&1 | tail -20

log "enabling the serial console for QEMU ($CONSOLE)"
chroot "$ROOTFS" systemctl enable "serial-getty@${CONSOLE%%,*}.service" 2>/dev/null || \
    warn "could not enable the serial getty; boot tests may show no login prompt"

manifest_add "base-config" "$SUITE/$ARCH configured"
stage_mark_complete 20-base-config
stage_done

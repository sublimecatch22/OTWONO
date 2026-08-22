#!/usr/bin/env bash
# Stage 40 — kernel, initramfs, firmware, and the bootloader toolchain.
#
# Network access: the recipe's [base] mirror (kernel, firmware, grub packages).
# Privileges: root (chroot).
#
# Output: a rootfs with /boot/vmlinuz-* and /boot/initrd.img-*, plus the grub modules
# stage 50 needs to build a standalone EFI binary for the target architecture.
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"
stage_begin 40-kernel

ROOTFS="$TARGET_OUT/rootfs"
[ -d "$ROOTFS/usr" ] || die "no rootfs at $ROOTFS; run stage 10 first"
require_root

ARCH="$(recipe_get target arch)"
KERNEL="$(recipe_get packages kernel)"
FIRMWARE="$(recipe_get_opt packages firmware)"
BOOTLOADER="$(recipe_get boot bootloader)"
CONSOLE="$(recipe_get boot console "ttyS0,115200")"

if stage_is_complete 40-kernel && compgen -G "$ROOTFS/boot/vmlinuz-*" > /dev/null; then
    log "kernel already installed; skipping (make clean to force)"
    stage_done
    exit 0
fi

mount --bind /dev "$ROOTFS/dev"
mount -t proc proc "$ROOTFS/proc"
mount -t sysfs sys "$ROOTFS/sys"
cleanup() { umount -l "$ROOTFS/dev" "$ROOTFS/proc" "$ROOTFS/sys" 2>/dev/null || true; }
trap cleanup EXIT

# initramfs-tools must know the console before the kernel postinst builds the initramfs,
# otherwise the first initramfs is built without it and we would have to regenerate.
log "configuring initramfs-tools"
mkdir -p "$ROOTFS/etc/initramfs-tools/conf.d"
cat > "$ROOTFS/etc/initramfs-tools/conf.d/otwono.conf" <<CONF
# MODULES=most keeps virtio, NVMe, MMC and USB storage in the initramfs. A QEMU image and
# an SD-card image then share one initramfs policy, which is what makes the same build
# work on both. Revisit only with measurements: MODULES=dep saves tens of megabytes but
# silently fails to boot on any host whose controller was not present at build time.
MODULES=most
COMPRESS=zstd
CONF

# The kernel package will not configure without a working /proc/mounts view of the root.
# initramfs-tools is listed explicitly: the kernel package only *recommends* it, and this
# build uses --no-install-recommends, so relying on the recommendation silently produces a
# kernel with no initramfs and a dangling /boot/initrd.img symlink.
log "installing kernel ($KERNEL), initramfs-tools and bootloader tooling ($BOOTLOADER)"
[ -n "$FIRMWARE" ] && log "firmware: $FIRMWARE" || log "firmware: none (this target needs no blobs)"
chroot "$ROOTFS" /bin/sh -c "
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y --no-install-recommends \
        $KERNEL initramfs-tools zstd $FIRMWARE $BOOTLOADER grub-common
    apt-get clean
    rm -rf /var/lib/apt/lists/*
" 2>&1 | tail -15

KVER=$(chroot "$ROOTFS" /bin/sh -c 'ls -1 /lib/modules 2>/dev/null | sort -V | tail -1')
[ -n "$KVER" ] || die "no kernel modules directory appeared; the kernel package did not install"
log "kernel version $KVER"

# The postinst usually builds the initramfs, but it is skipped in some chroot conditions.
# Build it explicitly rather than discovering at boot that there isn't one.
if [ ! -f "$ROOTFS/boot/initrd.img-$KVER" ]; then
    log "initramfs missing; generating it"
    chroot "$ROOTFS" update-initramfs -c -k "$KVER" 2>&1 | tail -5
fi

VMLINUZ=$(cd "$ROOTFS/boot" && ls -1 vmlinuz-* 2>/dev/null | sort -V | tail -1)
INITRD=$(cd "$ROOTFS/boot" && ls -1 initrd.img-* 2>/dev/null | sort -V | tail -1)
[ -n "$VMLINUZ" ] || die "no /boot/vmlinuz-* in the rootfs"
[ -n "$INITRD" ]  || die "no /boot/initrd.img-* in the rootfs"

# An initramfs that is implausibly small is usually a truncated or failed build, and it
# fails at boot with an unhelpful message. Catch it here instead.
INITRD_BYTES=$(stat -c %s "$ROOTFS/boot/$INITRD")
[ "$INITRD_BYTES" -gt $((4 * 1024 * 1024)) ] \
    || die "initramfs is only $INITRD_BYTES bytes; that is too small to contain a usable early userspace"

log "kernel  /boot/$VMLINUZ ($(stat -c %s "$ROOTFS/boot/$VMLINUZ") bytes)"
log "initrd  /boot/$INITRD ($INITRD_BYTES bytes)"

# arm64 boards need device trees. QEMU `virt` supplies its own via UEFI, so an empty set
# here is expected for that target and only worth a note.
if [ "$ARCH" = "arm64" ]; then
    if [ -d "$ROOTFS/usr/lib/linux-image-$KVER" ] || [ -d "$ROOTFS/boot/dtbs" ]; then
        DTB_COUNT=$(find "$ROOTFS/usr/lib/linux-image-$KVER" "$ROOTFS/boot/dtbs" -name '*.dtb' 2>/dev/null | wc -l)
        log "device trees available: $DTB_COUNT"
    else
        log "no device trees packaged; QEMU virt provides its own via UEFI"
    fi
fi

printf '%s\n' "$KVER" > "$TARGET_OUT/kernel-version"
manifest_add "kernel" "$KVER, initrd $INITRD_BYTES bytes, console $CONSOLE"
stage_mark_complete 40-kernel
stage_done

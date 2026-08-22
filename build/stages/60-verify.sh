#!/usr/bin/env bash
# Stage 60 — verify the image: structure, checksums, then an actual QEMU boot.
#
# Network access: none.
# Privileges: none (QEMU with user-mode networking).
#
# The boot test is the only thing that makes an image "done". It must reach a login prompt
# AND emit a capability profile from inside the VM — booting to a shell proves the image
# works, and the profile proves hardware detection works on the real target rather than
# only on the build host.
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"
stage_begin 60-verify

ARCH="$(recipe_get target arch)"
IMAGE="$TARGET_OUT/otwono-$TARGET.img"
BOOT_LOG="$TARGET_OUT/boot.log"

[ -f "$IMAGE" ] || die "no image at $IMAGE; run stage 50 first"

log "verifying checksums"
( cd "$TARGET_OUT" && sha256sum -c SHA256SUMS ) || die "checksum mismatch"

log "verifying the partition table"
sgdisk --verify "$IMAGE" > /dev/null || die "the partition table does not verify"
for want in OTWONO-ESP OTWONO-ROOT-A OTWONO-ROOT-B OTWONO-DATA; do
    sgdisk --print "$IMAGE" | grep -q "$want" || die "missing partition: $want"
done
log "  all four partitions present"

log "booting under QEMU (no KVM here, so TCG; this takes minutes)"
"$BUILD_DIR/qemu/run-$ARCH.sh" --image "$IMAGE" --boot-test --log "$BOOT_LOG" \
    || die "boot test failed; see $BOOT_LOG"

# --- Recover the profile the guest wrote to its own data partition ----------------------
#
# Console output proves the report ran. Reading the JSON back out of the disk the guest
# wrote proves the whole path worked: probe, classify, serialise, persist — on the target
# architecture, not the build host. Extracted with debugfs so this still needs no mounts.
log "recovering the capability profile the guest wrote"
WORK="$TARGET_OUT/verify-work"
rm -rf "$WORK"; mkdir -p "$WORK"

DATA_START=$(partx -g -o START -s --nr 4 "$IMAGE" | tr -d ' ')
DATA_SECTORS=$(partx -g -o SECTORS -s --nr 4 "$IMAGE" | tr -d ' ')
dd if="$IMAGE" of="$WORK/data.img" bs=512 skip="$DATA_START" count="$DATA_SECTORS" status=none

PROFILE="$TARGET_OUT/capability-profile.json"
rm -f "$PROFILE"
debugfs -R "dump /capability-profile.json $PROFILE" "$WORK/data.img" 2>/dev/null || true
rm -rf "$WORK"

[ -s "$PROFILE" ] || die "the guest booted but wrote no capability profile to its data partition.
       Console log: $BOOT_LOG"

command -v jq > /dev/null && {
    jq -e '.schema_version and .tier and .axes and .features' "$PROFILE" > /dev/null \
        || die "the recovered profile is not a well-formed capability profile: $PROFILE"
    log "profile recovered from the guest's data partition:"
    log "  tier            $(jq -r .tier "$PROFILE")"
    log "  limiting factor $(jq -r '.limiting_factor // "none"' "$PROFILE")"
    log "  architecture    $(jq -r .hardware.machine.architecture "$PROFILE")"
    log "  axes            $(jq -r '.axes | to_entries | map("\(.key)=\(.value)") | join(" ")' "$PROFILE")"
}

# The architecture in the profile must match the target, or we booted the wrong image.
EXPECT_ARCH=$([ "$ARCH" = "arm64" ] && echo aarch64 || echo x86_64)
GOT_ARCH=$(jq -r .hardware.machine.architecture "$PROFILE" 2>/dev/null || echo "?")
[ "$GOT_ARCH" = "$EXPECT_ARCH" ] \
    || die "profile reports architecture '$GOT_ARCH' but this target is $ARCH ($EXPECT_ARCH)"
log "  architecture matches the target"

log "boot log: $BOOT_LOG"
manifest_add "boot-test" "passed; tier $(jq -r .tier "$PROFILE" 2>/dev/null || echo unknown), log $(basename "$BOOT_LOG")"
stage_mark_complete 60-verify
stage_done

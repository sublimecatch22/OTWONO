#!/usr/bin/env bash
# Stage 60 — verify the image: checksums, then a QEMU boot smoke test.
#
# STATUS: PARTIAL — the QEMU harnesses in build/qemu/ are implemented and runnable, but no
# image exists to boot until stages 40 and 50 land (Phase 1).
#
# Network access: none.
# Privileges: none (QEMU with user-mode networking).
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"
stage_begin 60-verify

ARCH="$(recipe_get target arch)"
IMAGE="$TARGET_OUT/otwono-$TARGET.img"

if [ ! -f "$IMAGE" ]; then
    die "no image at $IMAGE — stages 40 and 50 are not implemented yet (Phase 1).
       The boot harness itself is ready: build/qemu/run-$ARCH.sh --help"
fi

log "verifying checksums"
( cd "$TARGET_OUT" && sha256sum -c SHA256SUMS ) || die "checksum mismatch"

log "booting under QEMU (TCG is slow; be patient)"
"$BUILD_DIR/qemu/run-$ARCH.sh" \
    --image "$IMAGE" \
    --boot-test \
    --log "$TARGET_OUT/boot.log" \
    || die "boot test failed; see $TARGET_OUT/boot.log"

log "boot log: $TARGET_OUT/boot.log"
manifest_add "boot-test" "passed"
stage_mark_complete 60-verify
stage_done

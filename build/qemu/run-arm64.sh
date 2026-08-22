#!/usr/bin/env bash
# Boot an OTWONO arm64 image under QEMU.
#
# Uses the `virt` machine with EDK2/AAVMF UEFI firmware and virtio, matching the
# arm64-qemu recipe. On an x86_64 host every instruction is emulated, so a boot takes
# considerably longer than the amd64 equivalent — the default timeout is doubled.
#
# Usage:
#   build/qemu/run-arm64.sh --image out/arm64-qemu/otwono-arm64-qemu.img
#   build/qemu/run-arm64.sh --image IMG --boot-test --log out/boot.log
set -euo pipefail

IMAGE=""; LOG=""; BOOT_TEST=0; TIMEOUT=1200; MEMORY=4096; SMP=2; CPU="cortex-a72"
EXPECT_PATTERN="${OTWONO_BOOT_EXPECT:-otwono login:|OTWONO capability profile|Reached target}"

usage() { sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --image) IMAGE="$2"; shift 2 ;;
        --log) LOG="$2"; shift 2 ;;
        --boot-test) BOOT_TEST=1; shift ;;
        --timeout) TIMEOUT="$2"; shift 2 ;;
        --memory) MEMORY="$2"; shift 2 ;;
        --smp) SMP="$2"; shift 2 ;;
        --cpu) CPU="$2"; shift 2 ;;
        -h|--help) usage 0 ;;
        *) echo "unknown option: $1" >&2; usage 1 ;;
    esac
done

[ -n "$IMAGE" ] || { echo "--image is required" >&2; usage 1; }
[ -f "$IMAGE" ] || { echo "no such image: $IMAGE" >&2; exit 1; }

AAVMF_CODE=""
for f in /usr/share/AAVMF/AAVMF_CODE.fd /usr/share/qemu-efi-aarch64/QEMU_EFI.fd; do
    [ -f "$f" ] && { AAVMF_CODE="$f"; break; }
done
[ -n "$AAVMF_CODE" ] || { echo "no AAVMF firmware; install 'qemu-efi-aarch64'" >&2; exit 1; }

# The virt machine requires both pflash images to be exactly 64 MiB.
CODE="$(mktemp -t otwono-aavmf-code-XXXXXX.fd)"
VARS="$(mktemp -t otwono-aavmf-vars-XXXXXX.fd)"
trap 'rm -f "$CODE" "$VARS"' EXIT
dd if=/dev/zero of="$CODE" bs=1M count=64 status=none
dd if="$AAVMF_CODE" of="$CODE" conv=notrunc status=none
if [ -f /usr/share/AAVMF/AAVMF_VARS.fd ]; then
    dd if=/dev/zero of="$VARS" bs=1M count=64 status=none
    dd if=/usr/share/AAVMF/AAVMF_VARS.fd of="$VARS" conv=notrunc status=none
else
    dd if=/dev/zero of="$VARS" bs=1M count=64 status=none
fi

ACCEL="tcg"
if [ -r /dev/kvm ] && [ "$(uname -m)" = "aarch64" ]; then
    ACCEL="kvm"; CPU="host"
fi
[ "$ACCEL" = tcg ] && echo "note: emulating aarch64 under TCG; this is slow by design" >&2

# shellcheck disable=SC2054  # commas belong to QEMU option values, not array syntax
QEMU_ARGS=(
    -machine virt,accel="$ACCEL"
    -cpu "$CPU"
    -m "$MEMORY"
    -smp "$SMP"
    -drive if=pflash,format=raw,unit=0,readonly=on,file="$CODE"
    -drive if=pflash,format=raw,unit=1,file="$VARS"
    -drive if=virtio,format=raw,file="$IMAGE"
    -netdev user,id=net0 -device virtio-net-pci,netdev=net0
    -nographic
    -serial mon:stdio
    -no-reboot
)

if [ "$BOOT_TEST" = 1 ]; then
    LOG="${LOG:-$(dirname "$IMAGE")/boot.log}"
    echo "boot test: timeout ${TIMEOUT}s, log $LOG, expecting /$EXPECT_PATTERN/"
    set +e
    timeout --foreground "$TIMEOUT" qemu-system-aarch64 "${QEMU_ARGS[@]}" < /dev/null > "$LOG" 2>&1
    rc=$?
    set -e
    if grep -qE "$EXPECT_PATTERN" "$LOG"; then
        echo "PASS: boot reached the expected state (qemu exit $rc)"
        exit 0
    fi
    echo "FAIL: expected pattern not found in $LOG (qemu exit $rc)" >&2
    tail -40 "$LOG" >&2
    exit 1
fi

exec qemu-system-aarch64 "${QEMU_ARGS[@]}"

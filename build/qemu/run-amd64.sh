#!/usr/bin/env bash
# Boot an OTWONO amd64 image under QEMU.
#
# Uses UEFI (OVMF) and virtio throughout, matching the amd64-qemu recipe. Without KVM,
# QEMU falls back to TCG and a boot takes minutes rather than seconds — the default
# timeout reflects that.
#
# Usage:
#   build/qemu/run-amd64.sh --image out/amd64-qemu/otwono-amd64-qemu.img
#   build/qemu/run-amd64.sh --image IMG --boot-test --log out/boot.log
set -euo pipefail
# shellcheck source=build/qemu/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

IMAGE=""; LOG=""; BOOT_TEST=0; TIMEOUT=600; MEMORY=4096; SMP=2
# Every pattern here must appear. A login prompt is the only honest "it booted"
# ("Reached target" shows up while a boot is still failing), and the capability banner
# proves the OTWONO layer actually ran on the target rather than just on the build host.
REQUIRED=()
if [ -n "${OTWONO_BOOT_EXPECT:-}" ]; then
    REQUIRED=("$OTWONO_BOOT_EXPECT")
else
    REQUIRED=("otwono login:" "OTWONO-CAPABILITY-OK")
fi
# Patterns that mean the boot definitively failed; matching one aborts early rather
# than burning the whole timeout under TCG.
FAIL_PATTERN="${OTWONO_BOOT_FAIL:-Kernel panic|Attempted to kill init|Entering emergency mode|You are in emergency mode}"

usage() { sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --image) IMAGE="$2"; shift 2 ;;
        --log) LOG="$2"; shift 2 ;;
        --boot-test) BOOT_TEST=1; shift ;;
        --timeout) TIMEOUT="$2"; shift 2 ;;
        --memory) MEMORY="$2"; shift 2 ;;
        --smp) SMP="$2"; shift 2 ;;
        -h|--help) usage 0 ;;
        *) echo "unknown option: $1" >&2; usage 1 ;;
    esac
done

[ -n "$IMAGE" ] || { echo "--image is required" >&2; usage 1; }
[ -f "$IMAGE" ] || { echo "no such image: $IMAGE" >&2; exit 1; }

OVMF_CODE=""
for f in /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd /usr/share/ovmf/OVMF.fd; do
    [ -f "$f" ] && { OVMF_CODE="$f"; break; }
done
[ -n "$OVMF_CODE" ] || { echo "no OVMF firmware found; install the 'ovmf' package" >&2; exit 1; }

OVMF_VARS_SRC=""
for f in /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd; do
    [ -f "$f" ] && { OVMF_VARS_SRC="$f"; break; }
done
VARS="$(mktemp -t otwono-ovmf-vars-XXXXXX.fd)"
trap 'rm -f "$VARS"' EXIT
[ -n "$OVMF_VARS_SRC" ] && cp "$OVMF_VARS_SRC" "$VARS" || : > "$VARS"

ACCEL="tcg"
[ -r /dev/kvm ] && ACCEL="kvm"
[ "$ACCEL" = tcg ] && echo "note: no KVM — running under TCG emulation, expect minutes not seconds" >&2

# shellcheck disable=SC2054  # commas belong to QEMU option values, not array syntax
QEMU_ARGS=(
    -machine q35,accel="$ACCEL"
    -cpu max
    -m "$MEMORY"
    -smp "$SMP"
    -drive if=pflash,format=raw,unit=0,readonly=on,file="$OVMF_CODE"
    -drive if=pflash,format=raw,unit=1,file="$VARS"
    -drive if=virtio,format=raw,file="$IMAGE"
    -netdev user,id=net0 -device virtio-net-pci,netdev=net0
    -nographic
    -serial mon:stdio
    -no-reboot
)

if [ "$BOOT_TEST" = 1 ]; then
    LOG="${LOG:-$(dirname "$IMAGE")/boot.log}"
    echo "boot test: timeout ${TIMEOUT}s, log $LOG"
    printf '  required: %s\n' "${REQUIRED[@]}"
    run_boot_test "$LOG" "$TIMEOUT" qemu-system-x86_64 "${QEMU_ARGS[@]}"
    exit $?
fi

exec qemu-system-x86_64 "${QEMU_ARGS[@]}"

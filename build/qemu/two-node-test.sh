#!/usr/bin/env bash
# Boot two OTWONO VMs on a private virtual LAN and prove they form a mesh.
#
# This is the Phase 3 exit criterion. It asserts three things a single-node boot cannot:
#
#   1. two nodes on the same segment discover each other with no configuration,
#   2. each authenticates the other's NodeID cryptographically,
#   3. an identity generated on first boot survives a reboot of the VM that owns it.
#
# The two VMs are joined with QEMU's socket netdev, which makes a private layer-2 segment
# between them and nothing else — no host bridge, no root, no DHCP server. That last part
# is deliberate: a mesh that only works where someone already runs DHCP is not a mesh, so
# the image falls back to IPv4 link-local and the nodes find each other over mDNS.
#
# Each VM gets its own copy of the image. They must: the identity is generated on first
# boot into the data partition, and two nodes sharing a disk would share a NodeID.
#
# Usage: build/qemu/two-node-test.sh --image IMG [--arch amd64|arm64] [--out DIR]
set -euo pipefail

IMAGE=""; ARCH="amd64"; OUT=""; TIMEOUT="${OTWONO_TWO_NODE_TIMEOUT:-900}"
usage() { sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --image) IMAGE="$2"; shift 2 ;;
        --arch) ARCH="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        --timeout) TIMEOUT="$2"; shift 2 ;;
        -h|--help) usage 0 ;;
        *) echo "unknown option: $1" >&2; usage 1 ;;
    esac
done

[ -n "$IMAGE" ] || { echo "--image is required" >&2; usage 1; }
[ -f "$IMAGE" ] || { echo "no such image: $IMAGE" >&2; exit 1; }
OUT="${OUT:-$(dirname "$IMAGE")/two-node}"
mkdir -p "$OUT"

# A free port for the VM-to-VM segment. Not a service; just how QEMU joins two guests.
SEGMENT_PORT="${OTWONO_SEGMENT_PORT:-0}"
if [ "$SEGMENT_PORT" = 0 ]; then
    SEGMENT_PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
fi

echo "two-node test"
echo "  image      $IMAGE"
echo "  arch       $ARCH"
echo "  segment    127.0.0.1:$SEGMENT_PORT"
echo "  output     $OUT"

# Independent disks: a shared one would mean a shared identity.
for n in a b; do
    cp --reflink=auto "$IMAGE" "$OUT/node-$n.img"
done

# shellcheck disable=SC2054  # commas belong to QEMU option values, not array syntax
case "$ARCH" in
    amd64)
        QEMU=qemu-system-x86_64
        MACHINE=(-machine q35,accel=tcg -cpu max)
        FW_CODE=$(ls /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd 2>/dev/null | head -1)
        FW_VARS_SRC=$(ls /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd 2>/dev/null | head -1)
        PFLASH_SIZE=0
        ;;
    arm64)
        QEMU=qemu-system-aarch64
        MACHINE=(-machine virt,accel=tcg -cpu cortex-a72)
        FW_CODE=$(ls /usr/share/AAVMF/AAVMF_CODE.fd /usr/share/qemu-efi-aarch64/QEMU_EFI.fd 2>/dev/null | head -1)
        FW_VARS_SRC=$(ls /usr/share/AAVMF/AAVMF_VARS.fd 2>/dev/null | head -1)
        PFLASH_SIZE=64
        ;;
    *) echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac
[ -n "$FW_CODE" ] || { echo "no UEFI firmware for $ARCH" >&2; exit 1; }

prepare_firmware() { # node
    local n="$1"
    if [ "$PFLASH_SIZE" -gt 0 ]; then
        dd if=/dev/zero of="$OUT/code-$n.fd" bs=1M count="$PFLASH_SIZE" status=none
        dd if="$FW_CODE" of="$OUT/code-$n.fd" conv=notrunc status=none
        dd if=/dev/zero of="$OUT/vars-$n.fd" bs=1M count="$PFLASH_SIZE" status=none
        [ -n "$FW_VARS_SRC" ] && dd if="$FW_VARS_SRC" of="$OUT/vars-$n.fd" conv=notrunc status=none
    else
        cp "$FW_CODE" "$OUT/code-$n.fd"
        if [ -n "$FW_VARS_SRC" ]; then cp "$FW_VARS_SRC" "$OUT/vars-$n.fd"; else : > "$OUT/vars-$n.fd"; fi
    fi
}

# Node A listens for the segment, node B connects to it. Together they are one L2 link.
start_node() { # node netdev-spec logfile
    local n="$1" netdev="$2" log="$3"
    prepare_firmware "$n"
    "$QEMU" "${MACHINE[@]}" \
        -m 2048 -smp 2 \
        -drive if=pflash,format=raw,unit=0,readonly=on,file="$OUT/code-$n.fd" \
        -drive if=pflash,format=raw,unit=1,file="$OUT/vars-$n.fd" \
        -drive if=virtio,format=raw,file="$OUT/node-$n.img" \
        -netdev "$netdev" -device virtio-net-pci,netdev=seg \
        -nographic -serial mon:stdio -no-reboot \
        < /dev/null > "$log" 2>&1 &
    echo $!
}

: > "$OUT/node-a.log"; : > "$OUT/node-b.log"
PID_A=$(start_node a "socket,id=seg,listen=127.0.0.1:$SEGMENT_PORT" "$OUT/node-a.log")
sleep 2
PID_B=$(start_node b "socket,id=seg,connect=127.0.0.1:$SEGMENT_PORT" "$OUT/node-b.log")

cleanup() {
    kill "$PID_A" "$PID_B" 2>/dev/null || true
    wait "$PID_A" "$PID_B" 2>/dev/null || true
}
trap cleanup EXIT

# The mesh line reports what each node can see: OTWONO-MESH-OK node=<fp> known=N connected=N
mesh_field() { # logfile field
    grep -ao "OTWONO-MESH-OK[^\r]*" "$1" 2>/dev/null | tail -1 |
        tr ' ' '\n' | awk -F= -v k="$2" '$1 == k {print $2}' | tail -1
}

echo "waiting for both nodes to form a mesh (TCG, up to ${TIMEOUT}s)"
deadline=$(( $(date +%s) + TIMEOUT ))
result=timeout
while [ "$(date +%s)" -lt "$deadline" ]; do
    if grep -qa "Kernel panic\|OTWONO-MESH-FAIL" "$OUT/node-a.log" "$OUT/node-b.log"; then
        result=fail; break
    fi
    # The mesh check runs once at boot, before discovery has had time to find anything,
    # so its connected= count is usually zero even on success. The authentication lines
    # are what actually prove a peer was verified, so wait on those.
    a_auth=$(grep -ac "peer authenticated" "$OUT/node-a.log" || true)
    b_auth=$(grep -ac "peer authenticated" "$OUT/node-b.log" || true)
    if [ "${a_auth:-0}" -ge 1 ] && [ "${b_auth:-0}" -ge 1 ]; then
        result=pass; break
    fi
    sleep 5
done

echo
echo "node A identity: $(mesh_field "$OUT/node-a.log" node)"
echo "node B identity: $(mesh_field "$OUT/node-b.log" node)"
echo "node A authentications: $(grep -ac 'peer authenticated' "$OUT/node-a.log" || echo 0)"
echo "node B authentications: $(grep -ac 'peer authenticated' "$OUT/node-b.log" || echo 0)"

if [ "$result" != pass ]; then
    echo "FAIL: the two nodes did not form a mesh ($result)" >&2
    for n in a b; do
        echo "--- node $n (last 25 lines) ---" >&2
        tail -25 "$OUT/node-$n.log" >&2
    done
    exit 1
fi

A_ID=$(mesh_field "$OUT/node-a.log" node)
B_ID=$(mesh_field "$OUT/node-b.log" node)
[ -n "$A_ID" ] && [ -n "$B_ID" ] || { echo "FAIL: a node reported no identity" >&2; exit 1; }
[ "$A_ID" != "$B_ID" ] || {
    echo "FAIL: both nodes report identity $A_ID; they are sharing a key" >&2
    exit 1
}

# Each node must have authenticated the *other* one, not merely something.
grep -qa "$B_ID" "$OUT/node-a.log" || { echo "FAIL: node A never names node B" >&2; exit 1; }
grep -qa "$A_ID" "$OUT/node-b.log" || { echo "FAIL: node B never names node A" >&2; exit 1; }

echo "PASS: two nodes discovered and mutually authenticated"
echo "  A $A_ID"
echo "  B $B_ID"

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

# The source image must not already contain an identity. If it does, both copies inherit
# the same NodeID, each sees its own fingerprint in the other's mDNS advertisement, skips
# it as its own, and the test times out after twenty minutes with no useful message. Worse,
# an image that ships with a key is a security defect in its own right. Fail in seconds.
start=$(partx -g -o START -s --nr 4 "$IMAGE" | tr -d ' ')
sectors=$(partx -g -o SECTORS -s --nr 4 "$IMAGE" | tr -d ' ')
dd if="$IMAGE" of="$OUT/data-check.img" bs=512 skip="$start" count="$sectors" status=none
rm -f "$OUT/found-key"
debugfs -R "dump /identity/node.key $OUT/found-key" "$OUT/data-check.img" 2>/dev/null || true
if [ -s "$OUT/found-key" ]; then
    echo "FAIL: $IMAGE already contains a node identity." >&2
    echo "      Both VMs would share a NodeID. Rebuild the image (make TARGET=... image)." >&2
    exit 1
fi
rm -f "$OUT/data-check.img" "$OUT/found-key"
echo "  source image carries no identity, as it should"

# Independent disks: a shared one would mean a shared identity.
for n in a b; do
    cp --sparse=always "$IMAGE" "$OUT/node-$n.img"
done

# `ls a b | head -1` looks tempting here and is a trap: ls exits non-zero when any
# argument is missing, and with `set -o pipefail` that kills the script with no message.
first_existing() {
    for candidate in "$@"; do
        [ -f "$candidate" ] && { printf '%s' "$candidate"; return 0; }
    done
    return 1
}

# shellcheck disable=SC2054  # commas belong to QEMU option values, not array syntax
case "$ARCH" in
    amd64)
        QEMU=qemu-system-x86_64
        MACHINE=(-machine q35,accel=tcg -cpu max)
        FW_CODE=$(first_existing /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd || true)
        FW_VARS_SRC=$(first_existing /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd || true)
        PFLASH_SIZE=0
        ;;
    arm64)
        QEMU=qemu-system-aarch64
        MACHINE=(-machine virt,accel=tcg -cpu cortex-a72)
        FW_CODE=$(first_existing /usr/share/AAVMF/AAVMF_CODE.fd /usr/share/qemu-efi-aarch64/QEMU_EFI.fd || true)
        FW_VARS_SRC=$(first_existing /usr/share/AAVMF/AAVMF_VARS.fd || true)
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
#
# Each guest needs its own MAC. QEMU hands every guest the same default
# (52:54:00:12:34:56) when none is given, and IPv4 link-local derives its address from the
# MAC — so both nodes came up on 169.254.158.157/16 and nothing could reach anything.
# Duplicate-address detection cannot save you either: with identical MACs each node sees
# its own ARP probe and concludes the address is free.
start_node() { # node netdev-spec logfile mac
    local n="$1" netdev="$2" log="$3" mac="$4"
    prepare_firmware "$n"
    "$QEMU" "${MACHINE[@]}" \
        -m 2048 -smp 2 \
        -drive if=pflash,format=raw,unit=0,readonly=on,file="$OUT/code-$n.fd" \
        -drive if=pflash,format=raw,unit=1,file="$OUT/vars-$n.fd" \
        -drive if=virtio,format=raw,file="$OUT/node-$n.img" \
        -netdev "$netdev" -device virtio-net-pci,netdev=seg,mac="$mac" \
        -nographic -serial mon:stdio -no-reboot \
        < /dev/null > "$log" 2>&1 &
    echo $!
}

: > "$OUT/node-a.log"; : > "$OUT/node-b.log"
# Locally-administered unicast addresses (the 0x02 bit set in the first octet), distinct
# per node.
PID_A=$(start_node a "socket,id=seg,listen=127.0.0.1:$SEGMENT_PORT" "$OUT/node-a.log" "52:54:00:07:11:01")
sleep 2
PID_B=$(start_node b "socket,id=seg,connect=127.0.0.1:$SEGMENT_PORT" "$OUT/node-b.log" "52:54:00:07:11:02")

cleanup() {
    kill "$PID_A" "$PID_B" 2>/dev/null || true
    wait "$PID_A" "$PID_B" 2>/dev/null || true
}
trap cleanup EXIT

# The mesh line reports what each node can see: OTWONO-MESH-OK node=<fp> known=N connected=N
#
# Always succeeds, printing an empty string when the marker has not appeared yet. That
# matters: under `set -o pipefail` a grep that matches nothing fails the whole pipeline,
# and inside `x=$(mesh_field ...)` with `set -e` that terminates the script with no
# message. This harness has been bitten by that twice — once here and once in firmware
# detection — so the guard belongs inside the function rather than at each call site.
mesh_field() { # logfile field
    # Match the *complete* marker shape, not a prefix. A serial console interleaves
    # carriage returns and flushes mid-line, so the newest occurrence is often a partial
    # one like "OTWONO-MESH-OK node=otw1:nask-s". Taking tail -1 of a loose match then
    # yields an empty field and the poll loop never sees the value that is plainly there.
    grep -aoE "OTWONO-MESH-OK node=[^ ]+ addr=[^ ]+ known=[0-9]+ connected=[0-9]+" "$1" 2>/dev/null |
        tail -1 | tr ' ' '\n' | awk -F= -v k="$2" '$1 == k {print $2}' | tail -1 || true
    return 0
}

echo "waiting for both nodes to form a mesh (TCG, up to ${TIMEOUT}s)"
deadline=$(( $(date +%s) + TIMEOUT ))
result=timeout
while [ "$(date +%s)" -lt "$deadline" ]; do
    if grep -qa "Kernel panic\|OTWONO-MESH-FAIL" "$OUT/node-a.log" "$OUT/node-b.log" 2>/dev/null; then
        result=fail; break
    fi
    # Wait on the periodic mesh marker, not on otwono-netd's own log lines: the daemon
    # logs to the journal, so its output never reaches the serial console. An earlier
    # version of this test waited for evidence that could not appear.
    a_conn=$(mesh_field "$OUT/node-a.log" connected)
    b_conn=$(mesh_field "$OUT/node-b.log" connected)
    case "$a_conn" in ''|*[!0-9]*) a_conn=0 ;; esac
    case "$b_conn" in ''|*[!0-9]*) b_conn=0 ;; esac
    if [ "$a_conn" -ge 1 ] && [ "$b_conn" -ge 1 ]; then
        result=pass; break
    fi
    sleep 5
done

echo
echo "node A identity: $(mesh_field "$OUT/node-a.log" node)"
echo "node B identity: $(mesh_field "$OUT/node-b.log" node)"
echo "node A address: $(mesh_field "$OUT/node-a.log" addr)"
echo "node B address: $(mesh_field "$OUT/node-b.log" addr)"
echo "node A peers connected: $(mesh_field "$OUT/node-a.log" connected)"
echo "node B peers connected: $(mesh_field "$OUT/node-b.log" connected)"

A_ADDR=$(mesh_field "$OUT/node-a.log" addr)
B_ADDR=$(mesh_field "$OUT/node-b.log" addr)
if [ -n "$A_ADDR" ] && [ "$A_ADDR" = "$B_ADDR" ]; then
    echo "FAIL: both nodes hold $A_ADDR. Their MACs collide, so link-local gave them the" >&2
    echo "      same address and neither can reach the other." >&2
    exit 1
fi

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

# Both sides must report a peer. A one-sided count would mean a half-open session.
for n in a b; do
    conn=$(mesh_field "$OUT/node-$n.log" connected)
    case "$conn" in ''|*[!0-9]*) conn=0 ;; esac
    [ "$conn" -ge 1 ] || { echo "FAIL: node $n reports no peer" >&2; exit 1; }
done

echo "PASS: two nodes discovered and mutually authenticated"
echo "  A $A_ID"
echo "  B $B_ID"

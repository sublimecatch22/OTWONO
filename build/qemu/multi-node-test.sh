#!/usr/bin/env bash
# Boot N OTWONO VMs on one private virtual LAN and prove content moves between them.
#
# The two-node test (build/qemu/two-node-test.sh) is the Phase 3 exit criterion and stays
# as it is: a point-to-point socket segment is a different network shape and worth keeping.
# This one exists for the questions two nodes cannot answer:
#
#   1. does a fetch draw chunks from *several* peers, which is ADR-0015's whole claim and
#      has until now been proven only host-side over in-memory links,
#   2. does the mesh form at all when more than one neighbour is announcing.
#
# The segment is UDP multicast on the loopback, which is how QEMU joins three or more
# guests to one L2 link. `socket,listen=`/`socket,connect=` is point to point and does not
# generalise.
#
# Each VM gets its own copy of the image. They must: the identity is generated on first boot
# into the data partition, and nodes sharing a disk would share a NodeID.
#
# Usage: build/qemu/multi-node-test.sh --image IMG [--nodes N] [--arch amd64|arm64] [--out DIR]
set -euo pipefail

IMAGE=""; ARCH="amd64"; OUT=""; NODES=3
TIMEOUT="${OTWONO_MULTI_NODE_TIMEOUT:-1200}"
CONTENT_TIMEOUT="${OTWONO_CONTENT_TIMEOUT:-420}"
usage() { sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --image) IMAGE="$2"; shift 2 ;;
        --nodes) NODES="$2"; shift 2 ;;
        --arch) ARCH="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        --timeout) TIMEOUT="$2"; shift 2 ;;
        -h|--help) usage 0 ;;
        *) echo "unknown option: $1" >&2; usage 1 ;;
    esac
done

[ -n "$IMAGE" ] || { echo "--image is required" >&2; usage 1; }
[ -f "$IMAGE" ] || { echo "no such image: $IMAGE" >&2; exit 1; }
case "$NODES" in ''|*[!0-9]*) echo "--nodes needs a number" >&2; exit 1 ;; esac
[ "$NODES" -ge 2 ] || { echo "--nodes must be at least 2" >&2; exit 1; }
[ "$NODES" -le 8 ] || { echo "--nodes above 8 is more TCG than this is worth" >&2; exit 1; }
OUT="${OUT:-$(dirname "$IMAGE")/multi-node}"
mkdir -p "$OUT"

# A multicast group and port for this run. Not a service; just how QEMU joins the guests.
MCAST_GROUP="${OTWONO_MCAST_GROUP:-230.7.11.1}"
MCAST_PORT="${OTWONO_MCAST_PORT:-0}"
if [ "$MCAST_PORT" = 0 ]; then
    MCAST_PORT=$(python3 -c 'import socket; s=socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
fi

echo "multi-node test"
echo "  image      $IMAGE"
echo "  nodes      $NODES"
echo "  arch       $ARCH"
echo "  segment    $MCAST_GROUP:$MCAST_PORT (multicast)"
echo "  output     $OUT"

if tar -tf /dev/null >/dev/null 2>&1; then :; fi

case "$ARCH" in
    amd64)
        QEMU=qemu-system-x86_64
        MACHINE=(-machine q35 -cpu max)
        FW_CODE=$(ls /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd 2>/dev/null | head -1 || true)
        FW_VARS_SRC=$(ls /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd 2>/dev/null | head -1 || true)
        PFLASH_SIZE=0
        ;;
    arm64)
        QEMU=qemu-system-aarch64
        MACHINE=(-machine virt -cpu cortex-a72)
        FW_CODE=$(ls /usr/share/AAVMF/AAVMF_CODE.fd 2>/dev/null | head -1 || true)
        FW_VARS_SRC=$(ls /usr/share/AAVMF/AAVMF_VARS.fd 2>/dev/null | head -1 || true)
        PFLASH_SIZE=64
        ;;
    *) echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac
[ -n "$FW_CODE" ] || { echo "no UEFI firmware for $ARCH" >&2; exit 1; }

PIDS=()
cleanup() {
    for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
    wait 2>/dev/null || true
}
trap cleanup EXIT

for i in $(seq 1 "$NODES"); do
    n="n$i"
    cp --reflink=auto "$IMAGE" "$OUT/node-$n.img"
    if [ "$PFLASH_SIZE" -gt 0 ]; then
        dd if=/dev/zero of="$OUT/code-$n.fd" bs=1M count="$PFLASH_SIZE" status=none
        dd if="$FW_CODE" of="$OUT/code-$n.fd" conv=notrunc status=none
        dd if=/dev/zero of="$OUT/vars-$n.fd" bs=1M count="$PFLASH_SIZE" status=none
        [ -n "$FW_VARS_SRC" ] && dd if="$FW_VARS_SRC" of="$OUT/vars-$n.fd" conv=notrunc status=none
    else
        cp "$FW_CODE" "$OUT/code-$n.fd"
        if [ -n "$FW_VARS_SRC" ]; then cp "$FW_VARS_SRC" "$OUT/vars-$n.fd"; else : > "$OUT/vars-$n.fd"; fi
    fi
    : > "$OUT/node-$n.log"
    # Each guest needs its own MAC. QEMU gives every guest the same default when none is
    # given, and IPv4 link-local derives its address from the MAC -- so identical MACs put
    # every node on one address and nothing can reach anything. Locally-administered
    # unicast (the 0x02 bit in the first octet), distinct per node.
    mac=$(printf '52:54:00:07:12:%02x' "$i")
    "$QEMU" "${MACHINE[@]}" \
        -m 2048 -smp 2 \
        -drive if=pflash,format=raw,unit=0,readonly=on,file="$OUT/code-$n.fd" \
        -drive if=pflash,format=raw,unit=1,file="$OUT/vars-$n.fd" \
        -drive if=virtio,format=raw,file="$OUT/node-$n.img" \
        -netdev "socket,id=seg,mcast=$MCAST_GROUP:$MCAST_PORT" \
        -device virtio-net-pci,netdev=seg,mac="$mac" \
        -nographic -serial mon:stdio -no-reboot \
        < /dev/null > "$OUT/node-$n.log" 2>&1 &
    PIDS+=($!)
    sleep 2
done

mesh_field() { # logfile field
    # The complete marker shape, not a prefix: a serial console flushes mid-line, so the
    # newest loose match is often a partial one and yields an empty field.
    grep -aoE "OTWONO-MESH-OK node=[^ ]+ addr=[^ ]+ known=[0-9]+ connected=[0-9]+" "$1" 2>/dev/null |
        tail -1 | tr ' ' '\n' | awk -F= -v k="$2" '$1 == k {print $2}' | tail -1 || true
    return 0
}

want=$(( NODES - 1 ))
echo "waiting for every node to see $want peer(s) (TCG, up to ${TIMEOUT}s)"
deadline=$(( $(date +%s) + TIMEOUT ))
result=timeout
while [ "$(date +%s)" -lt "$deadline" ]; do
    if grep -qa "Kernel panic\|OTWONO-MESH-FAIL\|OTWONO-MESH-CONTENT-FAIL" "$OUT"/node-*.log 2>/dev/null; then
        result=fail; break
    fi
    ok=0
    for i in $(seq 1 "$NODES"); do
        c=$(mesh_field "$OUT/node-n$i.log" connected)
        case "$c" in ''|*[!0-9]*) c=0 ;; esac
        [ "$c" -ge "$want" ] && ok=$(( ok + 1 ))
    done
    [ "$ok" -eq "$NODES" ] && { result=pass; break; }
    sleep 5
done

echo
for i in $(seq 1 "$NODES"); do
    echo "node n$i: $(mesh_field "$OUT/node-n$i.log" node) at $(mesh_field "$OUT/node-n$i.log" addr), $(mesh_field "$OUT/node-n$i.log" connected) peer(s)"
done

if [ "$result" != pass ]; then
    echo "FAIL: the nodes did not all reach $want peer(s) ($result)" >&2
    for i in $(seq 1 "$NODES"); do
        echo "--- node n$i (last 20 lines) ---" >&2
        tail -20 "$OUT/node-n$i.log" >&2
    done
    exit 1
fi

# Every identity must be distinct, or nodes are sharing a key.
ids=$(for i in $(seq 1 "$NODES"); do mesh_field "$OUT/node-n$i.log" node; done | sort)
distinct=$(echo "$ids" | sort -u | grep -c . || true)
[ "$distinct" -eq "$NODES" ] || {
    echo "FAIL: only $distinct distinct identities across $NODES nodes" >&2
    echo "$ids" >&2
    exit 1
}

# The content markers have to be *waited* for: the check polls for peers and only then
# fetches, so it finishes seconds after the mesh forms -- and this harness tears the VMs
# down the moment it does. Asserting at that instant is a race (defect 44).
if [ "${MESH_CONTENT_SMOKE:-0}" != 0 ]; then
    echo "waiting for every node to exchange content (up to ${CONTENT_TIMEOUT}s)"
    deadline=$(( $(date +%s) + CONTENT_TIMEOUT ))
    content=timeout
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if grep -qa "OTWONO-MESH-CONTENT-FAIL" "$OUT"/node-*.log 2>/dev/null; then
            content=fail; break
        fi
        ok=0
        for i in $(seq 1 "$NODES"); do
            grep -qa "OTWONO-MESH-CONTENT-OK" "$OUT/node-n$i.log" 2>/dev/null && ok=$(( ok + 1 ))
        done
        [ "$ok" -eq "$NODES" ] && { content=pass; break; }
        sleep 5
    done
    if [ "$content" != pass ]; then
        echo "FAIL: the nodes did not all exchange content ($content)" >&2
        grep -ha "OTWONO-MESH-CONTENT-" "$OUT"/node-*.log >&2 || true
        exit 1
    fi
    echo
    for i in $(seq 1 "$NODES"); do
        grep -hoa "OTWONO-MESH-CONTENT-OK.*" "$OUT/node-n$i.log" | tail -1 | tr -d '\r'
    done

    # ADR-0015's claim, on real links: with several peers holding an object, more than one
    # of them serves. Asserted on at least one node rather than all, because which peer
    # answers a given chunk first is a race by design and a node may legitimately get
    # everything from one fast neighbour.
    if [ "$NODES" -ge 3 ]; then
        spread=0
        for i in $(seq 1 "$NODES"); do
            s=$(grep -hoa "large_served=[0-9]*" "$OUT/node-n$i.log" | tail -1 | cut -d= -f2)
            case "$s" in ''|*[!0-9]*) s=0 ;; esac
            [ "$s" -ge 2 ] && spread=$(( spread + 1 ))
        done
        if [ "$spread" -eq 0 ]; then
            echo "FAIL: no node drew a multi-chunk object from more than one peer;" >&2
            echo "      fan-out did not happen on any of them" >&2
            grep -hoa "large_served=[0-9]*" "$OUT"/node-*.log >&2 || true
            exit 1
        fi
        echo "fan-out: $spread of $NODES node(s) drew the large object from several peers"
    fi
fi

echo "PASS: $NODES nodes discovered and mutually authenticated"

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
#        [--timeout S] [--allow-stale-image]
set -euo pipefail

# shellcheck source=build/qemu/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

IMAGE=""; ARCH="amd64"; OUT=""; NODES=3
TIMEOUT="${OTWONO_MULTI_NODE_TIMEOUT:-1200}"
CONTENT_TIMEOUT="${OTWONO_CONTENT_TIMEOUT:-1200}"
usage() { sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --image) IMAGE="$2"; shift 2 ;;
        --nodes) NODES="$2"; shift 2 ;;
        --arch) ARCH="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        --timeout) TIMEOUT="$2"; shift 2 ;;
        --allow-stale-image) ALLOW_STALE_IMAGE=1; shift ;;
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
        QEMU="qemu-system-x86_64"
        MACHINE=(-machine q35 -cpu max)
        FW_CODE=$(ls /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd 2>/dev/null | head -1 || true)
        FW_VARS_SRC=$(ls /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd 2>/dev/null | head -1 || true)
        PFLASH_SIZE=0
        ;;
    arm64)
        QEMU="qemu-system-aarch64"
        MACHINE=(-machine virt -cpu cortex-a72)
        FW_CODE=$(ls /usr/share/AAVMF/AAVMF_CODE.fd 2>/dev/null | head -1 || true)
        FW_VARS_SRC=$(ls /usr/share/AAVMF/AAVMF_VARS.fd 2>/dev/null | head -1 || true)
        PFLASH_SIZE=64
        ;;
    *) echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac
[ -n "$FW_CODE" ] || { echo "no UEFI firmware for $ARCH" >&2; exit 1; }

# How many vCPUs each guest gets, sized to the host rather than assumed.
#
# Every guest here is TCG -- there is no /dev/kvm in this environment -- so a vCPU is a host
# thread spinning at emulation speed, and oversubscribing them does not merely slow the run
# down. Two of three guests at `-smp 2` on a four-core host produced a guest that stopped
# executing about one second into its kernel and never resumed: no panic, no QEMU error, a
# complete last line, and an empty journal because it never reached userspace.
#
# TCG barely parallelises a single guest anyway, so dividing the host's cores among the
# guests costs almost nothing and keeps a core for the host. At least one, always.
HOST_CORES=$(nproc 2>/dev/null || echo 2)
SMP="${OTWONO_MULTI_NODE_SMP:-$(( HOST_CORES / NODES ))}"
[ "$SMP" -ge 1 ] || SMP=1
echo "  vcpus      $SMP per guest ($NODES guests on $HOST_CORES host cores)"

PIDS=()

# Start one guest. Separated out so a node can be booted again after being powered off,
# which is what the store-and-forward check needs: its whole property is that a recipient
# absent at send time collects after the sender has gone (ADR-0028).
#
# The QMP socket is how the harness reaches the power button. `-no-reboot` means QEMU exits
# on shutdown, so waiting for the process to end is waiting for the guest to be off.
boot_guest() { # ordinal logfile
    local i="$1" log="$2" n="n$1" mac
    # The MAC carries how many nodes there are and which one this is: octet 5 is N, octet 6
    # the 1-based ordinal. A guest has no other way to know -- every node boots the same
    # image -- and it needs both to take a distinct share of an object's chunks, to know how
    # many peers to wait for, and to know its role in the envelope check.
    mac=$(segment_mac "$NODES" "$i")
    "$QEMU" "${MACHINE[@]}" \
        -m 2048 -smp "$SMP" \
        -drive if=pflash,format=raw,unit=0,readonly=on,file="$OUT/code-$n.fd" \
        -drive if=pflash,format=raw,unit=1,file="$OUT/vars-$n.fd" \
        -drive if=virtio,format=raw,file="$OUT/node-$n.img" \
        -netdev "socket,id=seg,mcast=$MCAST_GROUP:$MCAST_PORT" \
        -device virtio-net-pci,netdev=seg,mac="$mac" \
        -qmp "unix:$OUT/qmp-$n.sock,server=on,wait=off" \
        -nographic -serial mon:stdio -no-reboot \
        < /dev/null >> "$log" 2>&1 &
    GUEST_PID[$i]=$!
    PIDS+=($!)
}

# Every guest's current pid, by ordinal. A node booted a second time gets a new one.
declare -A GUEST_PID
cleanup() {
    for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
    wait 2>/dev/null || true
}
trap cleanup EXIT

for i in $(seq 1 "$NODES"); do
    n="n$i"
    cp --reflink=auto "$IMAGE" "$OUT/node-$n.img"
    assert_data_filesystem "$OUT/node-$n.img" || exit 1
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
    # The MAC carries how many nodes there are and which one this is: octet 5 is N, octet 6
    # is the 1-based ordinal. A guest has no other way to know -- every node boots the same
    # image -- and it needs both to take a distinct share of an object's chunks and to know
    # how many peers to wait for. Reading its own MAC costs nothing and adds no interface.
    boot_guest "$i" "$OUT/node-$n.log"
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
#
# Which image this is comes from the image's own manifest, as in two-node-test.sh: the
# variable had to be set once for the build and again for the run, and forgetting the second
# made the harness print PASS having tested nothing. The variable is still honoured as an
# override; it is just no longer how the answer is normally reached.
MANIFEST="$(dirname "$IMAGE")/manifest.tsv"

# Refuse an image built from a different tree than the one being tested.
#
# `make multi-node-test` does not depend on `image` — it boots whatever is at the output
# path. So committing a fix and running the harness tests the *previous* build, passes or
# hangs for reasons that have nothing to do with the change, and never says which binaries it
# ran. Two three-node runs went that way before anyone looked at the image's mtime.
#
# An unknown revision on either side is not a mismatch: an image built before this stamp
# existed, or a tree that is not a git checkout, should still be runnable. Only a definite
# disagreement stops the run, and `--allow-stale-image` is there for deliberately re-running
# an older build.
IMAGE_REVISION="$(awk -F'\t' '$2 == "otwono-revision" { print $3 }' "$MANIFEST" 2>/dev/null | tail -1)"
TREE_REVISION="$(git -C "$(dirname "$0")/../.." describe --always --dirty --abbrev=12 2>/dev/null || echo unknown)"
if [ "${ALLOW_STALE_IMAGE:-0}" = 0 ] \
    && [ -n "$IMAGE_REVISION" ] && [ "$IMAGE_REVISION" != unknown ] \
    && [ "$TREE_REVISION" != unknown ] && [ "$IMAGE_REVISION" != "$TREE_REVISION" ]; then
    echo "the image at $IMAGE was built from $IMAGE_REVISION; this tree is $TREE_REVISION" >&2
    echo "rebuild it (make TARGET=... image) or pass --allow-stale-image to run it anyway" >&2
    exit 2
fi
echo "image built from ${IMAGE_REVISION:-an unstamped tree}"

if [ "${MESH_CONTENT_SMOKE:-0}" = 0 ] && [ -f "$MANIFEST" ] \
    && awk -F'\t' '$2 == "mesh-content-smoke" && $3 != "none" { found = 1 } END { exit !found }' "$MANIFEST"; then
    echo "image manifest says it carries the content check; asserting it"
    MESH_CONTENT_SMOKE=1
fi

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

    # --- store-and-forward: does a message survive its sender going away? (ADR-0028) -----
    #
    # The property that needs three machines. Node 1 seals an envelope to node 3 *while node
    # 3 is off*, node 2 takes custody of it, node 1 is then shut down, and node 3 comes back
    # and collects from node 2 with the sender gone for the whole collection.
    #
    # The order is what makes it a test rather than a demonstration. Each power-down is
    # waited for — `powerdown_guest` returns only once the process is gone — so "node 1 was
    # down when node 3 collected" is established by the sequence and not inferred from a log.
    if [ "$NODES" -ge 3 ]; then
        echo
        echo "store-and-forward: powering off node 3, the recipient"
        powerdown_guest "$OUT/qmp-n3.sock" "${GUEST_PID[3]}" "node n3" || exit 1

        echo "waiting for node 1 to seal and node 2 to take custody (up to ${CONTENT_TIMEOUT}s)"
        deadline=$(( $(date +%s) + CONTENT_TIMEOUT ))
        envelope_result=timeout
        while [ "$(date +%s)" -lt "$deadline" ]; do
            if grep -qa "OTWONO-ENVELOPE-FAIL" "$OUT"/node-n*.log 2>/dev/null; then
                envelope_result=fail; break
            fi
            if grep -qa "OTWONO-ENVELOPE-SEALED" "$OUT/node-n1.log" 2>/dev/null \
                && grep -qa "OTWONO-ENVELOPE-CARRIED" "$OUT/node-n2.log" 2>/dev/null; then
                envelope_result=pass; break
            fi
            sleep 5
        done
        if [ "$envelope_result" != pass ]; then
            echo "FAIL: no envelope was sealed and taken into custody ($envelope_result)" >&2
            grep -ha "OTWONO-ENVELOPE-" "$OUT"/node-n*.log >&2 || true
            exit 1
        fi
        sealed=$(grep -hoa "OTWONO-ENVELOPE-SEALED.*" "$OUT/node-n1.log" | tail -1 | tr -d '\r')
        carried=$(grep -hoa "OTWONO-ENVELOPE-CARRIED.*" "$OUT/node-n2.log" | tail -1 | tr -d '\r')
        echo "  $sealed"
        echo "  $carried"

        # The same envelope, or the carrier took something else and the test proves nothing.
        sealed_id=$(echo "$sealed" | grep -o 'envelope=[0-9a-f]*' | cut -d= -f2)
        carried_id=$(echo "$carried" | grep -o 'envelope=[0-9a-f]*' | cut -d= -f2)
        if [ -z "$sealed_id" ] || [ "$sealed_id" != "$carried_id" ]; then
            echo "FAIL: node 2 took custody of $carried_id, not the sealed $sealed_id" >&2
            exit 1
        fi

        echo "store-and-forward: powering off node 1, the sender"
        powerdown_guest "$OUT/qmp-n1.sock" "${GUEST_PID[1]}" "node n1" || exit 1
        echo "  node 1 is off; nothing it holds can serve node 3 from here on"

        echo "store-and-forward: booting node 3 again to collect"
        : > "$OUT/node-n3-boot2.log"
        boot_guest 3 "$OUT/node-n3-boot2.log"

        deadline=$(( $(date +%s) + CONTENT_TIMEOUT ))
        collect_result=timeout
        while [ "$(date +%s)" -lt "$deadline" ]; do
            if grep -qa "OTWONO-ENVELOPE-FAIL\|Kernel panic" "$OUT/node-n3-boot2.log" 2>/dev/null; then
                collect_result=fail; break
            fi
            if grep -qa "OTWONO-ENVELOPE-COLLECTED" "$OUT/node-n3-boot2.log" 2>/dev/null; then
                collect_result=pass; break
            fi
            # If node 1 came back from the dead the test is void, so say so rather than
            # passing on a technicality.
            if kill -0 "${GUEST_PID[1]}" 2>/dev/null; then
                echo "FAIL: node 1 is running again; the collection was not made with the sender absent" >&2
                exit 1
            fi
            sleep 5
        done
        if [ "$collect_result" != pass ]; then
            echo "FAIL: node 3 collected nothing with the sender absent ($collect_result)" >&2
            grep -ha "OTWONO-ENVELOPE-" "$OUT/node-n3-boot2.log" >&2 || true
            exit 1
        fi
        collected=$(grep -hoa "OTWONO-ENVELOPE-COLLECTED.*" "$OUT/node-n3-boot2.log" | tail -1 | tr -d '\r')
        echo "  $collected"
        collected_id=$(echo "$collected" | grep -o 'envelope=[0-9a-f]*' | cut -d= -f2)
        if [ "$collected_id" != "$sealed_id" ]; then
            echo "FAIL: node 3 collected $collected_id, not the sealed $sealed_id" >&2
            exit 1
        fi
        kill -0 "${GUEST_PID[1]}" 2>/dev/null \
            && { echo "FAIL: node 1 was running at the end of the collection" >&2; exit 1; }
        echo "store-and-forward: node 3 collected and opened its envelope from node 2, with node 1 off"

        # And node 2 gave up custody (ADR-0028 §7). The recipient reports delivery once the
        # bytes are on its own disk; the carrier drops what it was holding. Asserted here
        # because this is the only place it can be seen happening between real nodes — the
        # unit tests cover the rule, not the round trip.
        echo "store-and-forward: waiting for node 2 to drop what it delivered"
        deadline=$(( $(date +%s) + CONTENT_TIMEOUT ))
        dropped=timeout
        while [ "$(date +%s)" -lt "$deadline" ]; do
            if grep -qa "OTWONO-ENVELOPE-DROPPED" "$OUT/node-n2.log" 2>/dev/null; then
                dropped=pass; break
            fi
            if grep -qa "OTWONO-ENVELOPE-FAIL" "$OUT/node-n2.log" 2>/dev/null; then
                dropped=fail; break
            fi
            sleep 5
        done
        if [ "$dropped" != pass ]; then
            echo "FAIL: node 2 is still carrying an envelope it delivered ($dropped)" >&2
            grep -ha "OTWONO-ENVELOPE-" "$OUT/node-n2.log" >&2 || true
            exit 1
        fi
        echo "  $(grep -hoa "OTWONO-ENVELOPE-DROPPED.*" "$OUT/node-n2.log" | tail -1 | tr -d "\r")"
    fi
fi

echo "PASS: $NODES nodes discovered and mutually authenticated"

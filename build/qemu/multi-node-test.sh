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
MANIFEST="$(dirname "$IMAGE")/manifest.tsv"
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

[ "$NODES" -le 8 ] || { echo "--nodes above 8 is more TCG than this is worth" >&2; exit 1; }
OUT="${OUT:-$(dirname "$IMAGE")/multi-node}"
mkdir -p "$OUT"

# Whether a node has printed a marker, on either of its boots.
#
# A node that is powered off and booted again writes to `node-nN-boot2.log`, and its first log
# stops changing the moment it shut down. A phase that greps `node-nN.log` is therefore asking
# a file that can no longer answer — which has now caused two harness failures that looked
# like product failures: the partition phase waited 240s for a count that could not move, and
# the wiki phase waited for a marker node 3 had already printed in the other file.
#
# So no phase names a node's log directly. Both files, in order, and the caller almost never
# cares which one answered.
node_said() { # ordinal pattern
    for f in "$OUT/node-n$1.log" "$OUT/node-n$1-boot2.log"; do
        [ -f "$f" ] || continue
        grep -qa "$2" "$f" 2>/dev/null && return 0
    done
    return 1
}

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
        -device virtio-net-pci,id=net0,netdev=seg,mac="$mac" \
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
# override; it is just no longer how the answer is normally reached. `MANIFEST` is set with
# the other image checks at the top.
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

# --- a wiki page, read on another node ---------------------------------------------------
#
# Phase 6's *first* exit clause, and the one thing here that is a service rather than a
# primitive. The content check already resolves a peer's `wiki/Getting-Started` — but what it
# names there is an opaque blob. This asserts a real page: a revision verified on its own
# signature and the body behind it, on every node.
#
# Every node, not one: each writes and each reads somebody else's, and a page that only ever
# travelled in one direction would leave half the nodes' writing untested.
if [ "${MESH_CONTENT_SMOKE:-0}" != 0 ]; then
    echo "waiting for every node to read a peer's wiki page (up to ${CONTENT_TIMEOUT}s)"
    deadline=$(( $(date +%s) + CONTENT_TIMEOUT ))
    wiki=timeout
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if grep -qa "OTWONO-WIKI-FAIL" "$OUT"/node-*.log 2>/dev/null; then wiki=fail; break; fi
        ok=0
        for i in $(seq 1 "$NODES"); do
            node_said "$i" "OTWONO-WIKI-OK" && ok=$(( ok + 1 ))
        done
        [ "$ok" -eq "$NODES" ] && { wiki=pass; break; }
        sleep 5
    done
    if [ "$wiki" != pass ]; then
        echo "FAIL: not every node read a peer's wiki page ($wiki)" >&2
        grep -ha "OTWONO-WIKI-" "$OUT"/node-*.log >&2 || true
        exit 1
    fi
    grep -hoa "OTWONO-WIKI-OK.*" "$OUT"/node-*.log | sed 's/^/  /' | tr -d '\r'
fi

# --- a partition, and the healing of it ------------------------------------------------
#
# The third clause of Phase 6's exit criterion, in the half that can be asserted without any
# service being built: the segment breaks, the nodes notice, and they find each other again.
# What *converges* across the break is content, and that needs a guest-side check to write
# during the partition — this asserts the partition itself, which nothing did before and
# which everything else about a partition depends on.
#
# Nodes 2 and 3 are the survivors of the store-and-forward phase, so this uses them. Only
# node 3's link goes down; a partition is one-sided from each node's point of view and taking
# both would be indistinguishable from stopping the segment.
#
# `set_link` and not a power-off. The guest keeps running and keeps its state, which is what
# makes this a partition rather than the restart the phase above already tests.
#
# Gated on the drop marker rather than on a variable: this runs only when the store-and-forward
# phase actually got to the end, and reading that off the log is one fact instead of two that
# can disagree.
if [ "${MESH_CONTENT_SMOKE:-0}" != 0 ] && [ "$NODES" -ge 3 ] \
    && grep -qa "OTWONO-ENVELOPE-DROPPED" "$OUT/node-n2.log" 2>/dev/null; then

    # Every count here is taken *before* the thing that should change it, and the assertion is
    # that it went up. Both of these lines are already in the logs — node 2 spent the whole
    # store-and-forward phase failing to reach the powered-off node 1, and the nodes
    # re-authenticate constantly — so grepping for either would have passed before the link
    # was ever touched.
    #
    # `grep -c` prints 0 and exits 1 with no matches, which is fine, but prints *nothing* and
    # exits 2 if the file is missing — and an empty string in an arithmetic test is a fatal
    # error under `set -e`, so the count is forced to a number.
    unreachable() { # log
        local n; n=$(grep -ca "failed: connect to\|No route to host\|Network is unreachable" "$1" 2>/dev/null || true)
        printf '%s' "${n:-0}"
    }
    authenticated() { # log
        local n; n=$(grep -ca "inbound peer authenticated" "$1" 2>/dev/null || true)
        printf '%s' "${n:-0}"
    }
    # Node 3's *current* log, which is its second boot's. It was powered off and booted again
    # for the collection, so `node-n3.log` stopped changing when it shut down — counting
    # against that file is counting a number that can never move, and the first run of this
    # phase timed out waiting for exactly that while node 3 was logging
    # `Network is unreachable` twice a minute into the other file.
    n3_log="$OUT/node-n3-boot2.log"
    [ -f "$n3_log" ] || n3_log="$OUT/node-n3.log"
    lost_before=$(unreachable "$n3_log")

    echo "partition: taking node 3's link down"
    set_guest_link "$OUT/qmp-n3.sock" down "node n3" || exit 1

    # Node 3 is the one whose link went, so node 3 is where the failures appear on its own
    # discovery thread. Two sweeps plus room: the sweep is every thirty seconds and a connect
    # has to time out inside it.
    deadline=$(( $(date +%s) + 240 ))
    parted=timeout
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if [ "$(unreachable "$n3_log")" -gt "$lost_before" ]; then parted=pass; break; fi
        sleep 5
    done
    if [ "$parted" != pass ]; then
        echo "FAIL: node 3's link went down and it did not notice within 240s ($parted)" >&2
        exit 1
    fi
    echo "  node 3 noticed the break"

    # Node 2 writes only once it has no peers at all, which is this moment — so what it
    # publishes cannot have reached node 3 before the break. Waiting for it *here*, before
    # healing, is what makes that an assertion instead of a hope: heal first and the write
    # could cross on an unbroken link and the convergence below would prove nothing.
    echo "partition: waiting for node 2 to write while it is alone"
    deadline=$(( $(date +%s) + 600 ))
    wrote=timeout
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if grep -qa "OTWONO-PARTITION-FAIL" "$OUT/node-n2.log" 2>/dev/null; then wrote=fail; break; fi
        if grep -qa "OTWONO-PARTITION-WROTE" "$OUT/node-n2.log" 2>/dev/null; then wrote=pass; break; fi
        sleep 5
    done
    if [ "$wrote" != pass ]; then
        echo "FAIL: node 2 did not write during the partition ($wrote)" >&2
        grep -ha "OTWONO-PARTITION-" "$OUT/node-n2.log" >&2 || true
        exit 1
    fi
    echo "  $(grep -hoa "OTWONO-PARTITION-WROTE.*" "$OUT/node-n2.log" | tail -1 | tr -d "\r")"

    # Counted now rather than before the partition: node 3 is provably cut off at this point,
    # so any inbound handshake node 2 logs from here can only have followed the heal. Taken
    # earlier it would have raced the link actually going down, and a handshake from the
    # moment before the break would have read as the mesh healing.
    auth_before=$(authenticated "$OUT/node-n2.log")

    echo "partition: putting node 3's link back"
    set_guest_link "$OUT/qmp-n3.sock" up "node n3" || exit 1
    deadline=$(( $(date +%s) + 300 ))
    healed=timeout
    while [ "$(date +%s)" -lt "$deadline" ]; do
        # A *new* handshake after the heal. "They are still connected" and "they found each
        # other again" look identical in a log that only appends, so this counts.
        if [ "$(authenticated "$OUT/node-n2.log")" -gt "$auth_before" ]; then healed=pass; break; fi
        sleep 5
    done
    if [ "$healed" != pass ]; then
        echo "FAIL: node 3's link came back and the nodes did not re-authenticate ($healed)" >&2
        exit 1
    fi
    echo "  the mesh healed: node 2 authenticated an inbound peer again"

    # And the point of the whole phase: what was written on the other side of the break is
    # here. Node 3 resolves the name and fetches what it points at — a name that crosses
    # without its content is not convergence, and they are separate mechanisms.
    echo "partition: waiting for node 3 to converge on what it missed"
    deadline=$(( $(date +%s) + 900 ))
    converged=timeout
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if grep -qa "OTWONO-PARTITION-FAIL" "$n3_log" 2>/dev/null; then converged=fail; break; fi
        if grep -qa "OTWONO-PARTITION-CONVERGED" "$n3_log" 2>/dev/null; then converged=pass; break; fi
        sleep 5
    done
    if [ "$converged" != pass ]; then
        echo "FAIL: node 3 did not converge after the partition healed ($converged)" >&2
        grep -ha "OTWONO-PARTITION-" "$n3_log" >&2 || true
        exit 1
    fi
    echo "  $(grep -hoa "OTWONO-PARTITION-CONVERGED.*" "$n3_log" | tail -1 | tr -d "\r")"

    # The id node 2 published and the id node 3 fetched are the same object, compared rather
    # than each being asserted to exist. Two nodes each succeeding at something is not the
    # same as them agreeing, and this phase is about agreement.
    wrote_id=$(grep -hoa "OTWONO-PARTITION-WROTE id=[0-9a-f]*" "$OUT/node-n2.log" | tail -1 | cut -d= -f2)
    got_id=$(grep -hoa "OTWONO-PARTITION-CONVERGED id=[0-9a-f]*" "$n3_log" | tail -1 | cut -d= -f2)
    if [ -z "$wrote_id" ] || [ "$wrote_id" != "$got_id" ]; then
        echo "FAIL: node 2 wrote '$wrote_id' and node 3 converged on '$got_id'" >&2
        exit 1
    fi
    echo "  both nodes name the same object after the heal"
fi

echo "PASS: $NODES nodes discovered and mutually authenticated"
# And say plainly when that is *all* that was asserted.
#
# An image built without MESH_CONTENT_SMOKE=1 carries no content or envelope check, so the
# harness boots three nodes, watches them find each other, and prints PASS. That is a true
# statement about a real result and it is one line away from looking like the whole suite
# passed — which is how a rebuild that dropped the flag went unnoticed until someone read the
# manifest. A release image legitimately has no checks in it, so this is a warning and not a
# failure; it just has to be impossible to miss.
if [ "${MESH_CONTENT_SMOKE:-0}" = 0 ]; then
    echo
    echo "NOTE: this image carries no content or envelope check, so nothing above tested"
    echo "      content, replication, pointers or store-and-forward. Rebuild with"
    echo "      MESH_CONTENT_SMOKE=1 to assert those."
fi

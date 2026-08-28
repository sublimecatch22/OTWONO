#!/usr/bin/env bash
# Boot two OTWONO VMs on a private virtual LAN and prove they form a mesh.
#
# This is the Phase 3 exit criterion. It asserts three things a single-node boot cannot:
#
#   1. two nodes on the same segment discover each other with no configuration,
#   2. each authenticates the other's NodeID cryptographically,
#   3. an identity generated on first boot survives a reboot of the VM that owns it,
#   4. and, on a content-check image, that the pointer rollback defence survives it too.
#
# Claims 3 and 4 need a second boot, which this script did not do until the pointer work
# needed it: the header promised claim 3 and nothing here rebooted anything. What stood in
# for it was `otwono-mesh-check` asserting the key files are on disk, which is a proxy for
# the property and not the property.
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

# shellcheck source=build/qemu/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

IMAGE=""; ARCH="amd64"; OUT=""; TIMEOUT="${OTWONO_TWO_NODE_TIMEOUT:-900}"
# Separate from the mesh timeout: the content check only starts working once a peer exists,
# so this is measured from mesh formation and not from boot.
# The content check waits for a mesh, then fetches several objects with generous retries;
# under TCG it can legitimately take well past five minutes, and a timeout shorter than the
# work reports a failure that is only slowness. Raised after a run reached 363s with the
# check still running and nothing in the log to say so -- which is also why the check now
# prints a line per section.
CONTENT_TIMEOUT="${OTWONO_CONTENT_TIMEOUT:-1200}"
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
for key in node.key agreement.key; do
    rm -f "$OUT/found-key"
    debugfs -R "dump /identity/$key $OUT/found-key" "$OUT/data-check.img" 2>/dev/null || true
    if [ -s "$OUT/found-key" ]; then
        echo "FAIL: $IMAGE already contains /identity/$key." >&2
        echo "      Both VMs would share it. Rebuild the image (make TARGET=... image)." >&2
        exit 1
    fi
done
rm -f "$OUT/data-check.img" "$OUT/found-key"
echo "  source image carries no identity, as it should"

# Independent disks: a shared one would mean a shared identity.
#
# Each copy is checked for a data filesystem before anything boots. Every otwono daemon has
# `RequiresMountsFor=/var/lib/otwono`, so a copy whose data partition did not survive gives
# two VMs that reach a login prompt with *no otwono service running at all* — and this
# harness then reports "the two nodes did not form a mesh", which is true and useless. That
# cost a full twenty-minute timeout and a disk forensics session to say "the disk was
# broken". Two seconds of blkid says it instead.
for n in a b; do
    cp --sparse=always "$IMAGE" "$OUT/node-$n.img"
    assert_data_filesystem "$OUT/node-$n.img" || exit 1
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
        QEMU="qemu-system-x86_64"
        MACHINE=(-machine q35,accel=tcg -cpu max)
        FW_CODE=$(first_existing /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd || true)
        FW_VARS_SRC=$(first_existing /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd || true)
        PFLASH_SIZE=0
        ;;
    arm64)
        QEMU="qemu-system-aarch64"
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
        -qmp "unix:$OUT/qmp-$n.sock,server=on,wait=off" \
        -nographic -serial mon:stdio -no-reboot \
        < /dev/null > "$log" 2>&1 &
    echo $!
}

: > "$OUT/node-a.log"; : > "$OUT/node-b.log"
# Locally-administered unicast addresses (the 0x02 bit set in the first octet), distinct
# per node.
PID_A=$(start_node a "socket,id=seg,listen=127.0.0.1:$SEGMENT_PORT" "$OUT/node-a.log" "$(segment_mac 2 1)")
sleep 2
PID_B=$(start_node b "socket,id=seg,connect=127.0.0.1:$SEGMENT_PORT" "$OUT/node-b.log" "$(segment_mac 2 2)")

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
    if grep -qa "Kernel panic\|OTWONO-MESH-FAIL\|OTWONO-MESH-CONTENT-FAIL" \
        "$OUT/node-a.log" "$OUT/node-b.log" 2>/dev/null; then
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

# A content check that failed must never be a passing run. This harness reported success
# once while a check inside a guest printed FAIL, on the single-node side (defect 40), and
# the lesson is not specific to that harness: an assertion nobody looks at is not one.
if grep -qa "OTWONO-MESH-CONTENT-FAIL" "$OUT/node-a.log" "$OUT/node-b.log" 2>/dev/null; then
    echo "FAIL: a node's content check failed" >&2
    grep -ha "OTWONO-MESH-CONTENT-FAIL" "$OUT/node-a.log" "$OUT/node-b.log" >&2 || true
    exit 1
fi

# On an image built with MESH_CONTENT_SMOKE=1 the check is present, so its success marker is
# required rather than merely welcome. On a plain image it is absent and this is skipped:
# requiring it everywhere would fail every release-image run for the wrong reason.
#
# It has to be *waited* for. The content check polls for a peer and only then does its
# fetches, so it finishes seconds after the mesh forms -- and this harness tears the VMs
# down the moment it does. Asserting at that instant passed once by timing luck and failed
# the next run with the check still mid-retry, having printed nothing at all.
#
# Which image this is comes from the image's own manifest, not from the environment. It used
# to come only from MESH_CONTENT_SMOKE, which had to be set twice -- once for the build and
# again for the run -- and forgetting the second made this harness print PASS having tested
# nothing at all. A green result that checked nothing is worse than a red one. The variable
# is still honoured as an override, but it is no longer how the answer is normally reached.
MANIFEST="$(dirname "$IMAGE")/manifest.tsv"
if [ "${MESH_CONTENT_SMOKE:-0}" = 0 ] && [ -f "$MANIFEST" ] \
    && awk -F'\t' '$2 == "mesh-content-smoke" && $3 != "none" { found = 1 } END { exit !found }' "$MANIFEST"; then
    echo "image manifest says it carries the content check; asserting it"
    MESH_CONTENT_SMOKE=1
fi

if [ "${MESH_CONTENT_SMOKE:-0}" != 0 ]; then
    echo "waiting for both nodes to exchange content (up to ${CONTENT_TIMEOUT}s)"
    deadline=$(( $(date +%s) + CONTENT_TIMEOUT ))
    content=timeout
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if grep -qa "OTWONO-MESH-CONTENT-FAIL" "$OUT/node-a.log" "$OUT/node-b.log" 2>/dev/null; then
            content=fail; break
        fi
        if grep -qa "OTWONO-MESH-CONTENT-OK" "$OUT/node-a.log" 2>/dev/null \
            && grep -qa "OTWONO-MESH-CONTENT-OK" "$OUT/node-b.log" 2>/dev/null; then
            content=pass; break
        fi
        sleep 5
    done
    if [ "$content" != pass ]; then
        echo "FAIL: the two nodes did not exchange content ($content)" >&2
        grep -ha "OTWONO-MESH-CONTENT-" "$OUT/node-a.log" "$OUT/node-b.log" >&2 || true
        exit 1
    fi
    echo "content: both nodes served a public object and refused a private one"
    grep -hoa "OTWONO-MESH-CONTENT-OK.*" "$OUT/node-a.log" | tail -1 | tr -d '\r'

    # Replication is asserted here rather than inside the check, because neither node can
    # tell "the pass is broken" from "my peer had not published yet" on its own (ADR-0026).
    # Across both, it is unambiguous: each published a REPLICATED object, so each had
    # something to take, and a node reporting none took none.
    for node in a b; do
        line=$(grep -hoa "OTWONO-MESH-CONTENT-OK.*" "$OUT/node-$node.log" | tail -1 | tr -d '\r')
        taken=$(echo "$line" | grep -o 'replica_taken=[^ ]*' | cut -d= -f2)
        if [ -z "$taken" ] || [ "$taken" = none ]; then
            echo "FAIL: node $node held no replica from its peer (replica_taken=${taken:-absent})" >&2
            echo "  $line" >&2
            exit 1
        fi
    done
    echo "content: each node took a replica of the other's REPLICATED object, unprompted"

    # Each node must have resolved the *other's* pointer. Asserted across both for the same
    # reason replication is: one node cannot tell "my peer had not published yet" from "the
    # path is broken", and both having published makes the distinction unnecessary.
    for node in a b; do
        line=$(grep -hoa "OTWONO-MESH-CONTENT-OK.*" "$OUT/node-$node.log" | tail -1 | tr -d '\r')
        resolved=$(echo "$line" | grep -o 'pointer_resolved=[^ ]*' | cut -d= -f2)
        published=$(echo "$line" | grep -o 'pointer_published=[^ ]*' | cut -d= -f2)
        if [ -z "$resolved" ] || [ "$resolved" = none ]; then
            echo "FAIL: node $node resolved no pointer (pointer_resolved=${resolved:-absent})" >&2
            echo "  $line" >&2
            exit 1
        fi
        if [ "$resolved" = "$published" ]; then
            echo "FAIL: node $node resolved its own pointer, not its peer's" >&2
            exit 1
        fi
    done
    echo "content: each node resolved the other's pointer and fetched what it names"

    # The pointer advanced, and then a regressed one was refused (ADR-0027 §1). Asserted
    # here as well as inside the check, because these are the two claims most worth being
    # sure of: a green run that skipped them would be a run that proved the design's central
    # property by not testing it. `pointer_advanced` must also differ from the first read,
    # or the sequence moved without the name meaning anything new.
    for node in a b; do
        line=$(grep -hoa "OTWONO-MESH-CONTENT-OK.*" "$OUT/node-$node.log" | tail -1 | tr -d '\r')
        resolved=$(echo "$line" | grep -o 'pointer_resolved=[^ ]*' | cut -d= -f2)
        advanced=$(echo "$line" | grep -o 'pointer_advanced=[^ ]*' | cut -d= -f2)
        refused=$(echo "$line" | grep -o 'pointer_rollback_refused=[^ ]*' | cut -d= -f2)
        if [ -z "$advanced" ] || [ "$advanced" = none ] || [ "$advanced" = "$resolved" ]; then
            echo "FAIL: node $node never saw its peer's pointer advance (pointer_advanced=${advanced:-absent})" >&2
            echo "  $line" >&2
            exit 1
        fi
        if [ "$refused" != yes ]; then
            echo "FAIL: node $node did not refuse a regressed pointer (pointer_rollback_refused=${refused:-absent})" >&2
            echo "  $line" >&2
            exit 1
        fi
    done
    echo "content: each node took its peer's update, then refused the peer's rolled-back record"

    # --- second boot: does any of it survive the machine going away? ---------------------
    #
    # ADR-0027's defence is state the reader keeps, and state that does not outlive a reboot
    # is protection an attacker gets by waiting. It had been tested by opening a PointerStore,
    # closing it, and opening it again -- same process, same kernel, same page cache.
    #
    # Nothing is republished on this boot. The previous one ended with each node's own pointer
    # regressed to sequence 1 and each node's memory of its peer at sequence 2, both on disk,
    # so each peer is *still serving* the record the other must *still* refuse. The only thing
    # that can refuse it is what came back off the disk.
    echo
    echo "shutting both nodes down and booting them again from the same disks"
    powerdown_guest "$OUT/qmp-a.sock" "$PID_A" "node a" || exit 1
    powerdown_guest "$OUT/qmp-b.sock" "$PID_B" "node b" || exit 1
    echo "  both nodes powered off cleanly"

    # Separate logs. Overwriting the first boot's would destroy the evidence for everything
    # asserted above, and would let a marker from boot one be read as proof about boot two.
    : > "$OUT/node-a-boot2.log"; : > "$OUT/node-b-boot2.log"
    PID_A=$(start_node a "socket,id=seg,listen=127.0.0.1:$SEGMENT_PORT" \
        "$OUT/node-a-boot2.log" "$(segment_mac 2 1)")
    sleep 2
    PID_B=$(start_node b "socket,id=seg,connect=127.0.0.1:$SEGMENT_PORT" \
        "$OUT/node-b-boot2.log" "$(segment_mac 2 2)")

    echo "waiting for both nodes to answer for what they remember (up to ${CONTENT_TIMEOUT}s)"
    deadline=$(( $(date +%s) + CONTENT_TIMEOUT ))
    reboot_result=timeout
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if grep -qa "OTWONO-POINTER-REBOOT-FAIL\|Kernel panic" \
            "$OUT/node-a-boot2.log" "$OUT/node-b-boot2.log" 2>/dev/null; then
            reboot_result=fail; break
        fi
        if grep -qa "OTWONO-POINTER-REBOOT-OK" "$OUT/node-a-boot2.log" 2>/dev/null \
            && grep -qa "OTWONO-POINTER-REBOOT-OK" "$OUT/node-b-boot2.log" 2>/dev/null; then
            reboot_result=pass; break
        fi
        sleep 5
    done
    if [ "$reboot_result" != pass ]; then
        echo "FAIL: the rollback defence did not survive the reboot ($reboot_result)" >&2
        grep -ha "OTWONO-POINTER-REBOOT-" "$OUT"/node-*-boot2.log >&2 || true
        exit 1
    fi

    for node in a b; do
        line=$(grep -hoa "OTWONO-POINTER-REBOOT-OK.*" "$OUT/node-$node-boot2.log" | tail -1 | tr -d '\r')
        echo "  $line"
        remembered=$(echo "$line" | grep -o 'remembered=[0-9]*' | cut -d= -f2)
        came_back_as=$(echo "$line" | grep -o 'fingerprint=[^ ]*' | cut -d= -f2)
        was=$(mesh_field "$OUT/node-$node.log" node)
        case "$remembered" in ''|*[!0-9]*) remembered=0 ;; esac
        # Asserted on the *number*, not on the fact of a refusal. A log that came back empty
        # would make the node accept the record as a first read, and a log that came back
        # wrong would refuse while naming something other than what it saw.
        if [ "$remembered" -lt 2 ]; then
            echo "FAIL: node $node refused naming sequence $remembered; it had seen 2" >&2
            exit 1
        fi
        # Independent of the guest's own check: this compares against what the *first boot's*
        # console said, which the second boot cannot have written.
        if [ -n "$was" ] && [ "$came_back_as" != "$was" ]; then
            echo "FAIL: node $node booted as $came_back_as, having been $was" >&2
            exit 1
        fi
    done
    echo "reboot: each node came back as itself and still refused its peer's rolled-back pointer"
fi

echo "PASS: two nodes discovered and mutually authenticated"
echo "  A $A_ID"
echo "  B $B_ID"

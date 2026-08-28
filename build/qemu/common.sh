#!/usr/bin/env bash
# Shared boot-test logic for the QEMU harnesses. Sourced, never executed.

# Run QEMU and watch its serial log, stopping as soon as the outcome is known.
#
# The naive version — run to completion, then grep — burns the entire timeout on every
# successful boot, because a guest sitting at a login prompt never exits. Under TCG that is
# ten wasted minutes per test, which is enough to discourage running the test at all.
#
# Success requires *every* pattern in REQUIRED to appear. A boot that reaches a login
# prompt but never emits a capability profile is not a working OTWONO image.
#
# The login prompt also bounds the wait. Once the guest is sitting at a getty, systemd has
# finished; a marker that has not appeared by then is not going to. Waiting the full
# timeout in that case turned a broken one-line unit into a ten-minute feedback loop, so
# after the prompt appears the harness allows a short grace period and then gives up.
# The MAC a guest reads its position on the segment from.
#
# Every node boots the same image, so a node has no way to know how many others are on the
# segment or which one it is. Both are encoded here: octet 5 is the node count, octet 6 the
# 1-based ordinal, both hex. `build/files/otwono-mesh-content-check` parses it back.
#
# One definition, in one place, because there were two: the two-node harness had been
# writing a literal 52:54:00:07:11:01 since before the encoding existed, so a guest read it
# as "node 1 of 17" and waited twelve minutes for sixteen neighbours that were never coming.
# It looked like slowness rather than a wrong answer, which is why it survived.
segment_mac() { # total ordinal
    printf '52:54:00:07:%02x:%02x' "$1" "$2"
}

# A node's disk must carry a data filesystem before it is worth booting.
#
# Every otwono daemon has `RequiresMountsFor=/var/lib/otwono`, so a copy whose data
# partition did not survive gives a VM that reaches a login prompt with *no otwono service
# running at all* -- and a harness then reports "the nodes did not form a mesh", which is
# true and useless. That cost a full twenty-minute timeout and a session mounting the VM
# disk to read its journal. Two seconds of blkid says it instead.
#
# Here rather than in each harness, and not because it is tidier: `segment_mac` above is in
# this file precisely because two copies of it drifted and a guest spent twelve minutes
# waiting for sixteen neighbours that were never coming.
assert_data_filesystem() { # image-path
    local img="$1" start
    start=$(partx -g -o START -s --nr 4 "$img" 2>/dev/null | tr -d ' ')
    if [ -z "$start" ]; then
        echo "FAIL: $img has no fourth partition to hold /var/lib/otwono." >&2
        return 1
    fi
    if ! blkid -p -o value -s LABEL -O "$(( start * 512 ))" "$img" 2>/dev/null \
        | grep -q OTWONO-DATA; then
        echo "FAIL: $img has no OTWONO-DATA filesystem." >&2
        echo "      Nothing would mount /var/lib/otwono, so no otwono daemon would start." >&2
        echo "      Check the source image is complete and that no other run is writing here." >&2
        return 1
    fi
    return 0
}

# Shut a guest down the way its own power button would, and wait for it to stop.
#
# A clean shutdown and not a kill. Where a test is about state written on one boot being
# there on the next, killing the VM would *also* be testing whether every daemon's writes
# survive losing power — a different question, and one this repo has audited for two files.
# Conflating them would mean a failure could be either and the log would say neither.
#
# Needs the guest to have been started with a QMP socket at $OUT/qmp-<node>.sock.
powerdown_guest() { # qmp-socket pid label
    local sock="$1" pid="$2" label="$3" waited=0
    printf '%s\n%s\n' \
        '{"execute":"qmp_capabilities"}' \
        '{"execute":"system_powerdown"}' \
        | socat -t 5 - "UNIX-CONNECT:$sock" > /dev/null 2>&1 || true
    while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 240 ]; do
        sleep 2; waited=$(( waited + 2 ))
    done
    if kill -0 "$pid" 2>/dev/null; then
        echo "FAIL: $label did not shut down within ${waited}s of the power button" >&2
        return 1
    fi
    rm -f "$sock"
    return 0
}

run_boot_test() { # log-file timeout qemu-binary args...
    local log="$1" timeout_s="$2" qemu="$3"; shift 3
    local settle="${OTWONO_BOOT_SETTLE:-15}"
    local grace="${OTWONO_BOOT_GRACE:-60}"
    local login_re="${OTWONO_BOOT_LOGIN:-login:}"
    local pid deadline result missing login_seen_at now

    : > "$log"
    "$qemu" "$@" < /dev/null > "$log" 2>&1 &
    pid=$!
    deadline=$(( $(date +%s) + timeout_s ))
    result=timeout
    login_seen_at=0

    while kill -0 "$pid" 2>/dev/null; do
        if grep -qaE "$FAIL_PATTERN" "$log"; then result=fail; break; fi

        missing=0
        for pat in "${REQUIRED[@]}"; do
            grep -qaE "$pat" "$log" || { missing=1; break; }
        done
        if [ "$missing" = 0 ]; then result=pass; break; fi

        now=$(date +%s)
        if [ "$login_seen_at" = 0 ] && grep -qaE "$login_re" "$log"; then
            login_seen_at=$now
        fi
        if [ "$login_seen_at" != 0 ] && [ $(( now - login_seen_at )) -ge "$grace" ]; then
            result=stalled
            break
        fi

        if [ "$now" -ge "$deadline" ]; then result=timeout; break; fi
        sleep 2
    done

    # Let the guest flush whatever was mid-write, and let the capability unit finish
    # writing its JSON to the data partition before the disk image is snatched away.
    [ "$result" = pass ] && sleep "$settle"

    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true

    case "$result" in
        pass)
            echo "PASS: every required pattern appeared"
            for pat in "${REQUIRED[@]}"; do echo "  matched: $pat"; done
            return 0 ;;
        fail)
            echo "FAIL: the boot failed outright" >&2
            grep -naE "$FAIL_PATTERN" "$log" | head -5 >&2
            tail -40 "$log" >&2
            return 1 ;;
        stalled)
            echo "FAIL: the guest reached a login prompt but never emitted every marker" >&2
            echo "      (waited ${grace}s after the prompt; set OTWONO_BOOT_GRACE to extend)" >&2
            for pat in "${REQUIRED[@]}"; do
                grep -qaE "$pat" "$log" && echo "  matched: $pat" >&2 || echo "  MISSING: $pat" >&2
            done
            # A failed OTWONO unit is almost always the cause, so surface it directly
            # rather than making the reader hunt through the console dump.
            grep -naE "Failed to start .*otwono|otwono.*Syntax error|otwono.*not found" "$log" | head -5 >&2
            tail -30 "$log" >&2
            return 1 ;;
        *)
            echo "FAIL: timed out after ${timeout_s}s" >&2
            for pat in "${REQUIRED[@]}"; do
                grep -qaE "$pat" "$log" && echo "  matched: $pat" >&2 || echo "  MISSING: $pat" >&2
            done
            tail -40 "$log" >&2
            return 1 ;;
    esac
}

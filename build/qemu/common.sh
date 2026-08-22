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
run_boot_test() { # log-file timeout qemu-binary args...
    local log="$1" timeout_s="$2" qemu="$3"; shift 3
    local settle="${OTWONO_BOOT_SETTLE:-15}"
    local pid deadline result missing

    : > "$log"
    "$qemu" "$@" < /dev/null > "$log" 2>&1 &
    pid=$!
    deadline=$(( $(date +%s) + timeout_s ))
    result=timeout

    while kill -0 "$pid" 2>/dev/null; do
        if grep -qaE "$FAIL_PATTERN" "$log"; then result=fail; break; fi

        missing=0
        for pat in "${REQUIRED[@]}"; do
            grep -qaE "$pat" "$log" || { missing=1; break; }
        done
        if [ "$missing" = 0 ]; then result=pass; break; fi

        if [ "$(date +%s)" -ge "$deadline" ]; then result=timeout; break; fi
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
        *)
            echo "FAIL: timed out after ${timeout_s}s" >&2
            for pat in "${REQUIRED[@]}"; do
                grep -qaE "$pat" "$log" && echo "  matched: $pat" >&2 || echo "  MISSING: $pat" >&2
            done
            tail -40 "$log" >&2
            return 1 ;;
    esac
}

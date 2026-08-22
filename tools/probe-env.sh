#!/usr/bin/env bash
# OTWONO development-environment probe.
#
# Reports what this host can actually do, so a build fails early with a clear message
# rather than halfway through a rootfs bootstrap. Run it before anything else in a new
# environment, and paste the output into a bug report.
#
# Network access: HEAD requests to the configured package mirrors only.
# Privileges: none required; some checks report "needs root" instead of failing.
#
# Usage: tools/probe-env.sh [--json]
set -uo pipefail

JSON=0
[ "${1:-}" = "--json" ] && JSON=1

pass=0; warn=0; fail=0
declare -a ROWS=()

row() { # status name detail
    ROWS+=("$1|$2|$3")
    case "$1" in
        ok)   pass=$((pass+1)) ;;
        warn) warn=$((warn+1)) ;;
        fail) fail=$((fail+1)) ;;
    esac
}

have() { command -v "$1" >/dev/null 2>&1; }

check_tool() { # name  status-if-missing  purpose
    local t="$1" missing_status="$2" purpose="$3"
    if have "$t"; then
        row ok "$t" "$($t --version 2>/dev/null | head -1 | cut -c1-60)"
    else
        row "$missing_status" "$t" "MISSING — $purpose"
    fi
}

# --- Host basics -------------------------------------------------------------------
row ok "host-arch" "$(uname -m)"
row ok "host-kernel" "$(uname -r)"
row ok "host-os" "$( (. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME") || echo unknown)"
row ok "cpus" "$(nproc 2>/dev/null || echo '?')"
row ok "memory" "$(free -h 2>/dev/null | awk '/^Mem:/{print $2" total, "$7" available"}')"

avail_kb=$(df -Pk . | awk 'NR==2{print $4}')
avail_gb=$(( avail_kb / 1024 / 1024 ))
if [ "$avail_gb" -lt 10 ]; then
    row fail "disk-free" "${avail_gb} GiB — a rootfs plus image needs ~10 GiB"
elif [ "$avail_gb" -lt 25 ]; then
    row warn "disk-free" "${avail_gb} GiB — tight; clean out/ between targets"
else
    row ok "disk-free" "${avail_gb} GiB"
fi

# --- Build toolchain ---------------------------------------------------------------
check_tool cargo         fail "Rust toolchain: all OTWONO daemons"
check_tool rustc         fail "Rust compiler"
check_tool gcc           fail "native C toolchain"
check_tool make          fail "build driver"
check_tool debootstrap   fail "rootfs bootstrap (stage 10)"
check_tool xorriso       warn "hybrid ISO assembly (amd64)"
check_tool grub-mkrescue warn "amd64 bootloader install"
check_tool jq            warn "manifest post-processing"
check_tool sgdisk        warn "GPT partitioning (stage 50); package: gdisk"
check_tool mkfs.ext4     warn "root filesystem creation (stage 50)"

# --- Cross-compilation -------------------------------------------------------------
if have aarch64-linux-gnu-gcc; then
    row ok "cross-gcc-arm64" "$(aarch64-linux-gnu-gcc -dumpversion 2>/dev/null)"
else
    row warn "cross-gcc-arm64" "MISSING — package: gcc-aarch64-linux-gnu"
fi
if have rustup; then
    targets=$(rustup target list --installed 2>/dev/null | tr '\n' ' ')
    case "$targets" in
        *aarch64-unknown-linux*) row ok "rust-target-arm64" "installed" ;;
        *) row warn "rust-target-arm64" "run: rustup target add aarch64-unknown-linux-gnu" ;;
    esac
fi

# --- Virtualization ------------------------------------------------------------------
check_tool qemu-system-x86_64  fail "amd64 boot tests"
check_tool qemu-system-aarch64 fail "arm64 boot tests"
check_tool qemu-img            warn "image conversion"

if [ -e /dev/kvm ] && [ -r /dev/kvm ]; then
    row ok "kvm" "available — boot tests run at native speed"
else
    row warn "kvm" "unavailable — QEMU falls back to TCG; boot tests take minutes, use long timeouts"
fi

for fw in /usr/share/OVMF/OVMF_CODE.fd /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/ovmf/OVMF.fd; do
    [ -f "$fw" ] && { row ok "uefi-amd64" "$fw"; found_ovmf=1; break; }
done
[ "${found_ovmf:-0}" = 1 ] || row warn "uefi-amd64" "no OVMF firmware — package: ovmf"

for fw in /usr/share/AAVMF/AAVMF_CODE.fd /usr/share/qemu-efi-aarch64/QEMU_EFI.fd; do
    [ -f "$fw" ] && { row ok "uefi-arm64" "$fw"; found_aavmf=1; break; }
done
[ "${found_aavmf:-0}" = 1 ] || row warn "uefi-arm64" "no AAVMF firmware — package: qemu-efi-aarch64"

# --- Foreign-architecture chroot -----------------------------------------------------
if [ -n "$(ls /usr/bin/qemu-aarch64-static 2>/dev/null)" ]; then
    row ok "qemu-user-arm64" "/usr/bin/qemu-aarch64-static"
else
    row warn "qemu-user-arm64" "MISSING — package: qemu-user-static"
fi

if [ -d /proc/sys/fs/binfmt_misc ] && [ -f /proc/sys/fs/binfmt_misc/register ]; then
    if compgen -G '/proc/sys/fs/binfmt_misc/*aarch64*' > /dev/null; then
        row ok "binfmt-arm64" "registered"
    else
        row warn "binfmt-arm64" "binfmt_misc mounted but no aarch64 handler; stage 10 registers one"
    fi
elif [ "$(id -u)" = 0 ]; then
    row warn "binfmt-arm64" "binfmt_misc not mounted; stage 10 will mount it (running as root)"
else
    row warn "binfmt-arm64" "binfmt_misc not mounted and not root — arm64 second stage will fail"
fi

# --- Containers ----------------------------------------------------------------------
if have docker && docker info >/dev/null 2>&1; then
    row ok "container" "docker daemon reachable"
elif have podman; then
    row ok "container" "podman (no docker daemon)"
else
    row warn "container" "no container runtime; stages fall back to plain chroot"
fi

# --- Mirrors -------------------------------------------------------------------------
probe_mirror() { # label url
    local label="$1" url="$2" code
    # curl prints the http_code itself (000 on a failed CONNECT); do not append another.
    code=$(curl -s -o /dev/null -m 20 -w '%{http_code}' "$url" 2>/dev/null)
    [ -n "$code" ] || code=000
    if [ "$code" = 200 ]; then
        row ok "mirror:$label" "reachable"
    else
        row warn "mirror:$label" "unreachable (HTTP $code) — recipes using it cannot bootstrap here"
    fi
}
if have curl; then
    probe_mirror debian        "https://deb.debian.org/debian/dists/trixie/Release"
    probe_mirror ubuntu-amd64  "http://archive.ubuntu.com/ubuntu/dists/noble/Release"
    probe_mirror ubuntu-ports  "http://ports.ubuntu.com/ubuntu-ports/dists/noble/Release"
else
    row warn "mirror" "curl missing; cannot probe mirrors"
fi

# --- Output --------------------------------------------------------------------------
if [ "$JSON" = 1 ]; then
    printf '{\n  "summary": { "ok": %d, "warn": %d, "fail": %d },\n  "checks": [\n' "$pass" "$warn" "$fail"
    first=1
    for r in "${ROWS[@]}"; do
        IFS='|' read -r st name detail <<< "$r"
        [ $first = 1 ] || printf ',\n'
        first=0
        printf '    { "status": "%s", "name": "%s", "detail": "%s" }' \
            "$st" "$name" "$(printf '%s' "$detail" | sed 's/\\/\\\\/g; s/"/\\"/g')"
    done
    printf '\n  ]\n}\n'
else
    echo "OTWONO environment probe"
    echo "========================"
    for r in "${ROWS[@]}"; do
        IFS='|' read -r st name detail <<< "$r"
        case "$st" in
            ok)   mark="  ok  " ;;
            warn) mark=" WARN " ;;
            fail) mark=" FAIL " ;;
        esac
        printf '[%s] %-22s %s\n' "$mark" "$name" "$detail"
    done
    echo
    printf 'summary: %d ok, %d warnings, %d failures\n' "$pass" "$warn" "$fail"
    [ "$fail" -gt 0 ] && echo "Failures block an image build. Warnings limit which targets can be built here."
fi

[ "$fail" -gt 0 ] && exit 1
exit 0

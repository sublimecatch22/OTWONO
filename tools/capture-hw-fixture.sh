#!/usr/bin/env bash
# Capture a hardware fixture for otwono-hal tests.
#
# Copies exactly the /proc and /sys files the probes read into a directory that can be
# passed to `SystemProbe::from_root()`. Run this on real hardware — a Raspberry Pi, an
# RK3588 board, a GPU workstation — and commit the result so detection for that machine is
# testable from a CI runner that is none of those things.
#
# Network access: none.
# Privileges: none required; unreadable files are skipped with a note.
#
# Usage: tools/capture-hw-fixture.sh <output-dir> [--label "Raspberry Pi 5 8GB"]
set -euo pipefail

OUT="${1:-}"
shift || true
LABEL=""
while [ $# -gt 0 ]; do
    case "$1" in
        --label) LABEL="${2:-}"; shift 2 ;;
        *) echo "unknown option: $1" >&2; exit 1 ;;
    esac
done

if [ -z "$OUT" ]; then
    echo "usage: $0 <output-dir> [--label <name>]" >&2
    exit 1
fi

mkdir -p "$OUT"
skipped=0

copy_file() {
    local src="$1"
    [ -f "$src" ] || return 0
    local dst="$OUT/${src#/}"
    mkdir -p "$(dirname "$dst")"
    if ! cat "$src" > "$dst" 2>/dev/null; then
        rm -f "$dst"
        skipped=$((skipped + 1))
    fi
}

mark_present() {
    # Device nodes are probed for existence only; a zero-length regular file is enough.
    local src="$1"
    [ -e "$src" ] || return 0
    local dst="$OUT/${src#/}"
    mkdir -p "$(dirname "$dst")"
    : > "$dst"
}

mark_dir() {
    # Some sysfs markers are directories (net/*/wireless, thermal zones, device-tree
    # nodes). The probe only checks that they exist. Git cannot track an empty directory,
    # so drop a .gitkeep inside — without it the marker silently vanishes on a fresh
    # clone and the fixture quietly describes different hardware.
    local src="$1"
    [ -e "$src" ] || return 0
    local dst="$OUT/${src#/}"
    mkdir -p "$dst"
    printf '%s\n' "# Marker directory. Its presence is what the probe checks; git cannot track an empty directory." > "$dst/.gitkeep"
}

echo "capturing to $OUT"

# --- CPU -------------------------------------------------------------------------------
copy_file /proc/cpuinfo
copy_file /sys/devices/system/cpu/online
for d in /sys/devices/system/cpu/cpu[0-9]*; do
    [ -d "$d" ] || continue
    copy_file "$d/topology/core_id"
    copy_file "$d/topology/physical_package_id"
    copy_file "$d/cpufreq/cpuinfo_max_freq"
done

# --- Memory ----------------------------------------------------------------------------
copy_file /proc/meminfo

# --- Machine identity ------------------------------------------------------------------
copy_file /proc/device-tree/model
copy_file /sys/firmware/devicetree/base/model
copy_file /sys/class/dmi/id/product_name
copy_file /sys/class/dmi/id/chassis_type

# --- Accelerators ----------------------------------------------------------------------
for d in /sys/class/drm/card[0-9]*; do
    [ -d "$d" ] || continue
    copy_file "$d/device/uevent"
    copy_file "$d/device/vendor"
    copy_file "$d/device/device"
    copy_file "$d/device/mem_info_vram_total"
    copy_file "$d/device/mem_info_vis_vram_total"
done
for d in /sys/class/accel/*; do
    [ -e "$d" ] || continue
    copy_file "$d/device/uevent"
    copy_file "$d/device/vendor"
done
for n in /dev/nvidiactl /dev/nvidia0 /dev/kfd /dev/rknpu /dev/hailo0 /dev/apex_0; do
    mark_present "$n"
done
for d in /proc/device-tree/npu*; do
    mark_dir "$d"
done

# --- Storage ---------------------------------------------------------------------------
for d in /sys/block/*; do
    [ -d "$d" ] || continue
    copy_file "$d/size"
    copy_file "$d/removable"
    copy_file "$d/queue/rotational"
done

# --- Network ---------------------------------------------------------------------------
for d in /sys/class/net/*; do
    [ -e "$d" ] || continue
    copy_file "$d/type"
    copy_file "$d/operstate"
    copy_file "$d/speed"
    mark_dir "$d/wireless"
    mark_dir "$d/phy80211"
done
copy_file /proc/net/route

# --- Power -----------------------------------------------------------------------------
for d in /sys/class/power_supply/*; do
    [ -e "$d" ] || continue
    copy_file "$d/type"
    copy_file "$d/online"
done
for d in /sys/class/thermal/thermal_zone*; do
    mark_dir "$d"
done

# --- Filesystem capacity ---------------------------------------------------------------
# statvfs(2) cannot be captured as a /sys file, so record it explicitly.
DATA_PATH=/var/lib/otwono
probe_path="$DATA_PATH"
while [ ! -e "$probe_path" ] && [ "$probe_path" != "/" ]; do
    probe_path="$(dirname "$probe_path")"
done
mkdir -p "$OUT/.otwono-probe"
total=$(( $(stat -f -c %b "$probe_path") * $(stat -f -c %S "$probe_path") ))
free=$(( $(stat -f -c %a "$probe_path") * $(stat -f -c %S "$probe_path") ))
cat > "$OUT/.otwono-probe/filesystem.json" <<JSON
{
  "data_path": "$DATA_PATH",
  "measured_at_path": "$probe_path",
  "total_bytes": $total,
  "free_bytes": $free
}
JSON

# --- Provenance ------------------------------------------------------------------------
cat > "$OUT/.otwono-probe/capture.json" <<JSON
{
  "synthetic": false,
  "label": "${LABEL:-$(uname -m) $(uname -r)}",
  "kernel": "$(uname -r)",
  "arch": "$(uname -m)",
  "captured_by": "tools/capture-hw-fixture.sh"
}
JSON

echo "captured. skipped $skipped unreadable file(s)."
echo "test with: cargo run -p otwono-hwctl -- profile --root $OUT --no-overrides"

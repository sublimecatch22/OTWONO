#!/usr/bin/env bash
# Shared helpers for OTWONO build stages. Sourced, never executed.
#
# Every stage sources this, then calls `stage_begin <name>`. Stages must be idempotent:
# re-running one is always safe, and a stage that has already produced its output skips
# with a note rather than redoing the work.

set -euo pipefail

: "${REPO_ROOT:?REPO_ROOT must be set by the Makefile}"
: "${TARGET:?TARGET must be set}"
: "${RECIPE:?RECIPE must be set}"
: "${TARGET_OUT:?TARGET_OUT must be set}"
: "${SOURCE_DATE_EPOCH:?SOURCE_DATE_EPOCH must be set}"

# FAT timestamps start at 1980-01-01. An earlier epoch (this dev environment exports
# SOURCE_DATE_EPOCH=0) makes mtools write nonsense dates into the ESP, so clamp it. Every
# other stage still honours the caller's value; only the floor moves.
readonly FAT_EPOCH_FLOOR=315532800
if [ "$SOURCE_DATE_EPOCH" -lt "$FAT_EPOCH_FLOOR" ]; then
    printf '[build] SOURCE_DATE_EPOCH=%s predates the FAT epoch; clamping to %s (1980-01-01)\n' \
        "$SOURCE_DATE_EPOCH" "$FAT_EPOCH_FLOOR" >&2
    SOURCE_DATE_EPOCH="$FAT_EPOCH_FLOOR"
    export SOURCE_DATE_EPOCH
fi

STAGE_NAME="${STAGE_NAME:-unknown}"

log()  { printf '[%s] %s\n' "$STAGE_NAME" "$*"; }
warn() { printf '[%s] WARNING: %s\n' "$STAGE_NAME" "$*" >&2; }
die()  { printf '[%s] ERROR: %s\n' "$STAGE_NAME" "$*" >&2; exit 1; }

stage_begin() {
    STAGE_NAME="$1"
    mkdir -p "$TARGET_OUT"
    log "start (target=$TARGET, epoch=$SOURCE_DATE_EPOCH)"
}

stage_done() {
    log "done"
    manifest_add "stage:$STAGE_NAME" "completed"
}

# --- Recipe access -------------------------------------------------------------------
# Deliberately a small TOML subset reader rather than a dependency: stages must run on a
# bare host before anything is built. Supports `key = "value"` and `key = 123` inside
# `[section]` blocks, which is the whole recipe grammar.
recipe_get() { # section key [default]
    local section="$1" key="$2" default="${3:-}" value
    value=$(awk -v sect="$section" -v k="$key" '
        /^[[:space:]]*\[/ {
            gsub(/^[[:space:]]*\[|\][[:space:]]*$/, "")
            cur = $0
            next
        }
        {
            line = $0
            sub(/#.*$/, "", line)
            if (cur != sect) next
            split(line, kv, "=")
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", kv[1])
            if (kv[1] != k) next
            val = line
            sub(/^[^=]*=[[:space:]]*/, "", val)
            gsub(/^"|"[[:space:]]*$/, "", val)
            gsub(/[[:space:]]+$/, "", val)
            print val
            exit
        }
    ' "$RECIPE")
    if [ -z "$value" ]; then
        [ -n "$default" ] || die "recipe $RECIPE has no [$section] $key and no default"
        value="$default"
    fi
    printf '%s' "$value"
}

# Like recipe_get, but an absent or explicitly empty value is a legitimate answer rather
# than a build failure. Use for genuinely optional keys ("this target needs no firmware").
recipe_get_opt() { # section key
    awk -v sect="$1" -v k="$2" '
        /^[[:space:]]*\[/ { gsub(/^[[:space:]]*\[|\][[:space:]]*$/, ""); cur = $0; next }
        {
            line = $0
            sub(/#.*$/, "", line)
            if (cur != sect) next
            split(line, kv, "=")
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", kv[1])
            if (kv[1] != k) next
            val = line
            sub(/^[^=]*=[[:space:]]*/, "", val)
            gsub(/^"|"[[:space:]]*$/, "", val)
            gsub(/[[:space:]]+$/, "", val)
            print val
            exit
        }
    ' "$RECIPE"
}

# --- Manifest ------------------------------------------------------------------------
manifest_path() { printf '%s/manifest.tsv' "$TARGET_OUT"; }

manifest_add() { # key value
    # Replace any existing row for this key so a re-run updates the manifest instead of
    # appending a duplicate. The manifest describes the current output, not the history.
    mkdir -p "$TARGET_OUT"
    local path row
    path="$(manifest_path)"
    row=$(printf '%s\t%s\t%s' "$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y-%m-%dT%H:%M:%SZ)" "$1" "$2")
    if [ -f "$path" ]; then
        grep -v -P "^[^\t]*\t\Q$1\E\t" "$path" > "$path.tmp" 2>/dev/null || : > "$path.tmp"
        mv "$path.tmp" "$path"
    fi
    printf '%s\n' "$row" >> "$path"
}

# --- Idempotence ---------------------------------------------------------------------
stage_marker() { printf '%s/.stamp-%s' "$TARGET_OUT" "$1"; }

stage_is_complete() { [ -f "$(stage_marker "$1")" ]; }

stage_mark_complete() { mkdir -p "$TARGET_OUT"; : > "$(stage_marker "$1")"; }

# --- Foreign-architecture execution ----------------------------------------------------
# Every stage that chroots into a foreign-arch rootfs needs binfmt_misc registered, not
# just the one that bootstrapped it. Registration lives in the kernel, not in the rootfs,
# so it does not survive a container restart — and a partial rebuild that skips stage 10
# would otherwise fail deep inside the next chroot with a bare "Exec format error".
ensure_foreign_arch_support() { # target-arch
    local arch="$1" host qemu
    host="$(dpkg --print-architecture)"
    [ "$arch" != "$host" ] || return 0

    case "$arch" in
        arm64) qemu=/usr/bin/qemu-aarch64-static ;;
        amd64) qemu=/usr/bin/qemu-x86_64-static ;;
        *) die "no qemu-user interpreter known for arch $arch" ;;
    esac
    [ -x "$qemu" ] || die "missing $qemu (package: qemu-user-static); cannot run $arch binaries"

    if [ ! -f /proc/sys/fs/binfmt_misc/register ]; then
        log "mounting binfmt_misc for $arch execution"
        mount -t binfmt_misc binfmt_misc /proc/sys/fs/binfmt_misc \
            || die "cannot mount binfmt_misc; $arch binaries cannot run on this $host host"
    fi

    if compgen -G "/proc/sys/fs/binfmt_misc/*${arch}*" > /dev/null \
        || compgen -G '/proc/sys/fs/binfmt_misc/*aarch64*' > /dev/null; then
        return 0
    fi

    log "registering the $arch binfmt handler"
    # The trailing F flag opens the interpreter at registration time, so it works without
    # copying qemu-user into every rootfs.
    case "$arch" in
        arm64)
            printf ':otwono-aarch64:M::\\x7fELF\\x02\\x01\\x01\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x02\\x00\\xb7\\x00:\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\x00\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xfe\\xff\\xff\\xff:%s:F\n' "$qemu" \
                > /proc/sys/fs/binfmt_misc/register \
                || die "could not register the arm64 binfmt handler"
            ;;
        *) die "no binfmt magic defined for arch $arch" ;;
    esac
}

# --- Guards --------------------------------------------------------------------------
require_root() {
    [ "$(id -u)" = 0 ] || die "this stage needs root (rootfs ownership and mounts)"
}

require_tool() {
    command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1${2:+ ($2)}"
}

require_space_gib() { # gib
    local need="$1" avail
    avail=$(( $(df -Pk "$TARGET_OUT" 2>/dev/null | awk 'NR==2{print $4}' || echo 0) / 1024 / 1024 ))
    [ "$avail" -ge "$need" ] || die "need ${need} GiB free under $TARGET_OUT, have ${avail} GiB"
}

# --- Sizes ---------------------------------------------------------------------------
# Convert a recipe size ("512M", "3G", "8192") to whole MiB.
size_to_mib() {
    local v="$1" n unit
    n="${v%[!0-9]}"; unit="${v#"$n"}"
    case "$unit" in
        M|MiB|m|"") printf '%s' "$n" ;;
        G|GiB|g)    printf '%s' "$((n * 1024))" ;;
        K|KiB|k)    printf '%s' "$((n / 1024))" ;;
        *) die "cannot parse size: $v" ;;
    esac
}

# --- Deterministic identifiers ---------------------------------------------------------
# Filesystem UUIDs must be stable across builds or the image is not reproducible, and they
# must differ per role or the kernel picks the wrong partition. Derive both properties
# from the recipe id, the role, and the build epoch.
derive_uuid() { # role
    local h
    h=$(printf '%s|%s|%s' "$TARGET" "$1" "$SOURCE_DATE_EPOCH" | sha256sum | cut -c1-32)
    printf '%s-%s-%s-%s-%s' "${h:0:8}" "${h:8:4}" "${h:12:4}" "${h:16:4}" "${h:20:12}"
}

# FAT volume ids are 32 bits, written as eight hex digits.
derive_fat_id() { # role
    printf '%s|%s|%s' "$TARGET" "$1" "$SOURCE_DATE_EPOCH" | sha256sum | cut -c1-8 | tr 'a-f' 'A-F'
}

# --- Safety ----------------------------------------------------------------------------
# Imaging a rootfs that still has /dev, /proc or /sys bind-mounted would copy the build
# host's kernel interfaces into the image. Refuse rather than produce a corrupt image.
assert_rootfs_unmounted() { # rootfs
    local rootfs mounted
    rootfs="$(readlink -f "$1")"
    mounted=$(awk -v r="$rootfs/" '$2 ~ "^"r {print $2}' /proc/mounts)
    if [ -n "$mounted" ]; then
        die "these paths are still mounted under the rootfs; unmount them before imaging:
$mounted"
    fi
}

# Reproducible tar: fixed mtime, sorted entries, no owner names.
tar_reproducible() { # output-file source-dir
    tar --sort=name \
        --mtime="@$SOURCE_DATE_EPOCH" \
        --owner=0 --group=0 --numeric-owner \
        --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime \
        -C "$2" -cf "$1" .
}

checksum_artifact() { # file
    ( cd "$(dirname "$1")" && sha256sum "$(basename "$1")" >> SHA256SUMS )
}

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

#!/usr/bin/env bash
# Stage 05 — build the OTWONO binaries for the target architecture.
#
# Network access: crates.io on a cold cargo cache.
# Privileges: none.
#
# Output: $TARGET_OUT/host-tools/{bin,manifest}
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"
stage_begin 05-host-tools

ARCH="$(recipe_get target arch)"
case "$ARCH" in
    amd64) RUST_TARGET="x86_64-unknown-linux-gnu"; LINKER_ENV="" ;;
    arm64) RUST_TARGET="aarch64-unknown-linux-gnu"
           LINKER_ENV="CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc" ;;
    *) die "unsupported arch: $ARCH" ;;
esac

require_tool cargo
[ "$ARCH" = "arm64" ] && require_tool aarch64-linux-gnu-gcc "package: gcc-aarch64-linux-gnu"

OUTDIR="$TARGET_OUT/host-tools"
mkdir -p "$OUTDIR/bin" "$OUTDIR/libexec"

log "building workspace for $RUST_TARGET"
( cd "$REPO_ROOT" && env ${LINKER_ENV:-} cargo build --release --workspace --target "$RUST_TARGET" )

BIN_SRC="$REPO_ROOT/target/$RUST_TARGET/release"
BINARIES=(otwono-hwctl otwono-permd otwono-hwd otwono-idd otwono-netd otwono-aid)
# AI backend adapters. Not in /usr/bin: nothing invokes them by hand, they are spawned by
# otwono-aid, and discovery finds them by path (otwono_ai::discovery).
ADAPTERS=(otwono-llama-backend)

: > "$OUTDIR/manifest"
stage_binary() { # source-name destination-dir
    local b="$1" dest="$2"
    [ -f "$BIN_SRC/$b" ] || die "expected binary not produced: $BIN_SRC/$b"
    install -m 0755 "$BIN_SRC/$b" "$dest/$b"
    printf '%s\t%s\t%s\n' "$b" "$(sha256sum "$dest/$b" | cut -d' ' -f1)" \
        "$(stat -c %s "$dest/$b")" >> "$OUTDIR/manifest"
    log "staged $b ($(stat -c %s "$dest/$b") bytes)"
}

for b in "${BINARIES[@]}"; do stage_binary "$b" "$OUTDIR/bin"; done
for b in "${ADAPTERS[@]}"; do stage_binary "$b" "$OUTDIR/libexec"; done

# Confirm we actually produced binaries for the target architecture rather than the host's.
EXPECT_MACHINE=$([ "$ARCH" = arm64 ] && echo "ARM aarch64" || echo "x86-64")
for b in "$OUTDIR/bin"/* "$OUTDIR/libexec"/*; do
    file "$b" | grep -q "$EXPECT_MACHINE" \
        || die "$(basename "$b") is not a $EXPECT_MACHINE binary: $(file -b "$b")"
done
log "verified all binaries are $EXPECT_MACHINE"

manifest_add "host-tools" "$(wc -l < "$OUTDIR/manifest") binaries for $RUST_TARGET"
stage_mark_complete 05-host-tools
stage_done

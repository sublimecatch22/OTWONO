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
mkdir -p "$OUTDIR/bin"

log "building workspace for $RUST_TARGET"
( cd "$REPO_ROOT" && env ${LINKER_ENV:-} cargo build --release --workspace --target "$RUST_TARGET" )

BIN_SRC="$REPO_ROOT/target/$RUST_TARGET/release"
BINARIES=(otwono-hwctl otwono-permd otwono-hwd otwono-idd otwono-netd)

: > "$OUTDIR/manifest"
for b in "${BINARIES[@]}"; do
    [ -f "$BIN_SRC/$b" ] || die "expected binary not produced: $BIN_SRC/$b"
    install -m 0755 "$BIN_SRC/$b" "$OUTDIR/bin/$b"
    printf '%s\t%s\t%s\n' "$b" "$(sha256sum "$OUTDIR/bin/$b" | cut -d' ' -f1)" \
        "$(stat -c %s "$OUTDIR/bin/$b")" >> "$OUTDIR/manifest"
    log "staged $b ($(stat -c %s "$OUTDIR/bin/$b") bytes)"
done

# Confirm we actually produced binaries for the target architecture rather than the host's.
EXPECT_MACHINE=$([ "$ARCH" = arm64 ] && echo "ARM aarch64" || echo "x86-64")
for b in "${BINARIES[@]}"; do
    file "$OUTDIR/bin/$b" | grep -q "$EXPECT_MACHINE" \
        || die "$b is not a $EXPECT_MACHINE binary: $(file -b "$OUTDIR/bin/$b")"
done
log "verified all binaries are $EXPECT_MACHINE"

manifest_add "host-tools" "$(wc -l < "$OUTDIR/manifest") binaries for $RUST_TARGET"
stage_mark_complete 05-host-tools
stage_done

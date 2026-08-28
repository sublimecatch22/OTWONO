#!/usr/bin/env bash
# Does build stage 35 produce the same engine binary twice?
#
# CLAUDE.md §7 requires reproducible builds. This is the check that turns that from an
# intention into a fact, and it is deliberately not part of the ordinary image build: it
# clones the pinned engine source twice and compiles it twice, which takes roughly fifteen
# minutes and needs network access.
#
#     tools/check-engine-reproducibility.sh              amd64
#     tools/check-engine-reproducibility.sh --arch arm64
#
# Why two *independent clones* rather than two builds from one checkout: a clone is how
# anyone else obtains the source, and it is the harder test. `git clone` sets every file's
# mtime to checkout time and lays the tree out in fresh directories, so a build that
# depends on either will differ between clones while happily reproducing from a single
# working tree. Testing the easy case would have passed while the real property failed.
#
# The clones go to a temporary directory and are removed afterwards, including on failure.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

# Kept in step with build/stages/35-ai-engine.sh. A drift here would make this check
# validate something other than what ships, so the flags are read from the stage where
# they can be, and the pin is asserted against it.
LLAMA_REF="${AI_ENGINE_REF:-b10588}"
LLAMA_COMMIT="${AI_ENGINE_COMMIT:-70adb1b4cea5ee39f867792c78dc59320921eda7}"
LLAMA_REPO="${AI_ENGINE_REPO:-https://github.com/ggml-org/llama.cpp}"
ARCH="amd64"

while [ $# -gt 0 ]; do
    case "$1" in
        --arch) ARCH="${2:?--arch needs a value}"; shift 2 ;;
        --ref)  LLAMA_REF="${2:?--ref needs a value}"; shift 2 ;;
        -h|--help) sed -n '2,/^set -euo/p' "$0" | sed 's/^# \{0,1\}//;$d'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

case "$ARCH" in
    amd64) CROSS_ARGS=() ;;
    arm64) CROSS_ARGS=(-DCMAKE_TOOLCHAIN_FILE="$REPO_ROOT/build/cmake/aarch64-linux-gnu.cmake") ;;
    *) echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac

for tool in cmake ninja git sha256sum; do
    command -v "$tool" >/dev/null || { echo "missing required tool: $tool" >&2; exit 1; }
done

# The stage must still be building what this checks. If someone adds a flag there and not
# here, this check silently stops meaning anything, so fail loudly instead.
STAGE="$REPO_ROOT/build/stages/35-ai-engine.sh"
for flag in GGML_NATIVE=OFF LLAMA_BUILD_UI=OFF LLAMA_USE_PREBUILT_UI=OFF ffile-prefix-map; do
    grep -q -- "$flag" "$STAGE" \
        || { echo "stage 35 no longer sets $flag; update this check to match" >&2; exit 1; }
done

WORK="$(mktemp -d -t otwono-repro-XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

build_once() { # label -> prints the sha256 of the resulting binary
    local label="$1"
    local src="$WORK/src-$label" build="$WORK/build-$label"

    git clone --depth 1 --branch "$LLAMA_REF" "$LLAMA_REPO" "$src" >/dev/null 2>&1 \
        || { echo "cannot clone $LLAMA_REPO at $LLAMA_REF" >&2; exit 1; }
    local have
    have="$(git -C "$src" rev-parse HEAD)"
    [ "$have" = "$LLAMA_COMMIT" ] \
        || { echo "$LLAMA_REF is at $have, expected $LLAMA_COMMIT; the tag moved" >&2; exit 1; }

    local pm="-ffile-prefix-map=$src=/build/llama.cpp -ffile-prefix-map=$build=/build/out"
    cmake -S "$src" -B "$build" -G Ninja \
        "${CROSS_ARGS[@]}" \
        -DCMAKE_C_FLAGS="$pm" \
        -DCMAKE_CXX_FLAGS="$pm" \
        -DCMAKE_BUILD_TYPE=Release \
        -DBUILD_SHARED_LIBS=OFF \
        -DGGML_NATIVE=OFF \
        -DLLAMA_CURL=OFF \
        -DLLAMA_BUILD_UI=OFF \
        -DLLAMA_USE_PREBUILT_UI=OFF \
        -DLLAMA_BUILD_TESTS=OFF \
        -DLLAMA_BUILD_EXAMPLES=OFF \
        -DLLAMA_BUILD_TOOLS=ON \
        -DLLAMA_BUILD_SERVER=ON >"$WORK/cmake-$label.log" 2>&1 \
        || { tail -20 "$WORK/cmake-$label.log" >&2; echo "cmake configure failed" >&2; exit 1; }
    cmake --build "$build" --target llama-server -j"$(nproc)" >"$WORK/build-$label.log" 2>&1 \
        || { tail -30 "$WORK/build-$label.log" >&2; echo "build failed" >&2; exit 1; }

    sha256sum "$build/bin/llama-server" | cut -d' ' -f1
    # Freed immediately: two engine build trees are several gigabytes, and disk is a fixed
    # allowance in the dev environment (CLAUDE.md §11).
    rm -rf "$src" "$build"
}

echo "checking llama.cpp $LLAMA_REF ($ARCH) reproduces across two independent clones"
echo "  this compiles the engine twice and takes several minutes"

FIRST="$(build_once one)"
echo "  build one: $FIRST"
SECOND="$(build_once two)"
echo "  build two: $SECOND"

if [ "$FIRST" = "$SECOND" ]; then
    echo "REPRODUCIBLE: both clones produced $FIRST"
    exit 0
fi

echo "NOT REPRODUCIBLE: $FIRST != $SECOND" >&2
echo "  the engine binary depends on something outside the pinned source." >&2
echo "  usual suspects: an embedded build path, a timestamp, or a generated asset" >&2
echo "  whose name is a content hash. See docs/build/VERIFICATION-LOG.md." >&2
exit 1

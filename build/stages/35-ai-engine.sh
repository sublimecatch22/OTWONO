#!/usr/bin/env bash
# Stage 35 — build and install the local inference engine.
#
# Network access: git clone of the pinned llama.cpp tag, when the engine is enabled and
#                 not already cached. None otherwise.
# Privileges: root (writes into the rootfs). The engine build itself runs unprivileged.
#
# Off by default. A recipe opts in with:
#
#     [ai]
#     engine = "llama.cpp"
#
# or a build overrides it with AI_ENGINE=llama.cpp. With no engine the stage installs
# nothing and the node reports no local inference, which is the honest state of a stock
# image: llama-server is 17 MiB of third-party C++ per architecture and a ten-minute
# build, and that is not a cost every image should pay silently.
#
# Why a from-source build and not a package: llama.cpp is not in Debian or Ubuntu, and its
# release cadence is measured in hours. It is pinned to a tag *and* verified against the
# commit that tag pointed at, so this stage is reproducible in the way that matters —
# re-running it produces the same engine, and a moved tag is a hard failure rather than a
# silent substitution.
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"
stage_begin 35-ai-engine

ROOTFS="$TARGET_OUT/rootfs"
[ -d "$ROOTFS/usr" ] || die "no rootfs at $ROOTFS; run stage 10 first"

ARCH="$(recipe_get target arch)"
ENGINE="${AI_ENGINE:-$(recipe_get_opt ai engine)}"

if [ -z "$ENGINE" ]; then
    log "no inference engine requested for this target"
    log "  the image will report local_inference=unavailable, which is accurate"
    log "  enable with AI_ENGINE=llama.cpp or [ai] engine = \"llama.cpp\" in the recipe"
    manifest_add "ai-engine" "none"
    stage_mark_complete 35-ai-engine
    stage_done
    exit 0
fi

[ "$ENGINE" = "llama.cpp" ] || die "unknown [ai] engine: $ENGINE (only llama.cpp is integrated)"

# Pinned. Bumping these is a deliberate commit that also re-runs the end-to-end tests
# against the new engine — see docs/ai/AI-RUNTIME.md.
LLAMA_REF="${AI_ENGINE_REF:-$(recipe_get_opt ai llama_cpp_ref)}"
LLAMA_REF="${LLAMA_REF:-b10588}"
LLAMA_COMMIT="${AI_ENGINE_COMMIT:-$(recipe_get_opt ai llama_cpp_commit)}"
LLAMA_COMMIT="${LLAMA_COMMIT:-70adb1b4cea5ee39f867792c78dc59320921eda7}"
LLAMA_REPO="${AI_ENGINE_REPO:-https://github.com/ggml-org/llama.cpp}"

require_tool cmake
require_tool ninja "package: ninja-build"
require_tool git
case "$ARCH" in
    amd64) CROSS_ARGS=(); EXPECT_MACHINE="x86-64" ;;
    arm64) require_tool aarch64-linux-gnu-g++ "package: g++-aarch64-linux-gnu"
           CROSS_ARGS=(-DCMAKE_TOOLCHAIN_FILE="$BUILD_DIR/cmake/aarch64-linux-gnu.cmake")
           EXPECT_MACHINE="ARM aarch64" ;;
    *) die "unsupported arch: $ARCH" ;;
esac

# Cached outside the per-target directory: the same engine build serves every recipe for an
# architecture, and rebuilding it per image would make an otherwise two-minute rebuild take
# twelve.
CACHE="$OUT/engines/llama.cpp/$LLAMA_REF/$ARCH"
SERVER="$CACHE/bin/llama-server"

if [ -x "$SERVER" ]; then
    log "using the cached engine at $SERVER"
else
    SRC="$OUT/engines/llama.cpp/$LLAMA_REF/src"
    if [ ! -d "$SRC/.git" ]; then
        log "cloning llama.cpp $LLAMA_REF (network)"
        rm -rf "$SRC"
        mkdir -p "$(dirname "$SRC")"
        git clone --depth 1 --branch "$LLAMA_REF" "$LLAMA_REPO" "$SRC" >/dev/null 2>&1 \
            || die "cannot clone $LLAMA_REPO at $LLAMA_REF"
    fi

    # A tag is a mutable pointer. Checking the commit is what makes the pin mean something:
    # without it, an upstream retag would change what we ship with no diff anywhere.
    HAVE="$(git -C "$SRC" rev-parse HEAD)"
    [ "$HAVE" = "$LLAMA_COMMIT" ] \
        || die "llama.cpp $LLAMA_REF is at $HAVE, expected $LLAMA_COMMIT; the tag moved or the pin is stale"
    log "llama.cpp $LLAMA_REF at $HAVE"

    BUILD="$OUT/engines/llama.cpp/$LLAMA_REF/build-$ARCH"
    # Logs go beside the build tree, not inside it: the tree does not exist yet, and a
    # redirect through a path component that is not there fails before cmake ever runs.
    LOGDIR="$OUT/engines/llama.cpp/$LLAMA_REF"
    log "building llama-server for $ARCH (this takes several minutes; log: $LOGDIR)"
    # GGML_NATIVE=OFF matters for a distribution: the default tunes for the *build* machine,
    # which would produce an image that crashes with SIGILL on any older CPU.
    # LLAMA_CURL=OFF drops a libcurl dependency and the engine's own model downloader; model
    # fetching is a brokered OTWONO action, not something the engine does behind our back.
    #
    # -ffile-prefix-map rewrites the source and build paths the compiler bakes into the
    # binary. Without it the artifact depends on where it happened to be built, which
    # defeats the point of pinning a commit: two builds of the same source produce
    # different bytes and nobody can tell a rebuild from a substitution (CLAUDE.md §7).
    #
    # LLAMA_BUILD_UI=OFF drops the bundled browser chat UI. Three reasons, in order of
    # weight: we already start the engine with --no-webui, so it is dead weight; it is an
    # HTTP surface on a process that has no authentication; and its assets are named by a
    # hash that changes between builds, which was — measurably — the *only* thing making
    # this binary unreproducible. Turning it off removed 2.6 MB and closed that gap.
    PREFIX_MAP="-ffile-prefix-map=$SRC=/build/llama.cpp -ffile-prefix-map=$BUILD=/build/out"
    cmake -S "$SRC" -B "$BUILD" -G Ninja \
        "${CROSS_ARGS[@]}" \
        -DCMAKE_C_FLAGS="$PREFIX_MAP" \
        -DCMAKE_CXX_FLAGS="$PREFIX_MAP" \
        -DCMAKE_BUILD_TYPE=Release \
        -DBUILD_SHARED_LIBS=OFF \
        -DGGML_NATIVE=OFF \
        -DLLAMA_CURL=OFF \
        -DLLAMA_BUILD_UI=OFF \
        -DLLAMA_USE_PREBUILT_UI=OFF \
        -DLLAMA_BUILD_TESTS=OFF \
        -DLLAMA_BUILD_EXAMPLES=OFF \
        -DLLAMA_BUILD_TOOLS=ON \
        -DLLAMA_BUILD_SERVER=ON >"$LOGDIR/cmake-$ARCH.log" 2>&1 \
        || { tail -20 "$LOGDIR/cmake-$ARCH.log" >&2; die "cmake configure failed"; }
    cmake --build "$BUILD" --target llama-server -j"$(nproc)" >"$LOGDIR/build-$ARCH.log" 2>&1 \
        || { tail -30 "$LOGDIR/build-$ARCH.log" >&2; die "llama-server build failed"; }

    mkdir -p "$CACHE/bin"
    install -m 0755 "$BUILD/bin/llama-server" "$SERVER"
    # The build tree is gigabytes and the artifact is megabytes. Disk is a fixed allowance
    # in the dev environment (CLAUDE.md §11).
    rm -rf "$BUILD"
fi

file "$SERVER" | grep -q "$EXPECT_MACHINE" \
    || die "llama-server is not a $EXPECT_MACHINE binary: $(file -b "$SERVER")"
log "engine is $EXPECT_MACHINE, $(stat -c %s "$SERVER") bytes"

require_root
# The layout otwono_ai::discovery probes. `cpu` is a variant directory, not a suffix: a
# Vulkan or CUDA build of the same engine installs alongside it and is discovered the same
# way, with the model manifest deciding which is preferred.
DEST="$ROOTFS/usr/lib/otwono/ai/llama.cpp/cpu/bin"
install -d -m 0755 "$DEST"
install -m 0755 "$SERVER" "$DEST/llama-server"
log "installed /usr/lib/otwono/ai/llama.cpp/cpu/bin/llama-server"

# The adapter has to be there too, or discovery reports nothing — deliberately, because an
# engine with no way to drive it would make ai.capabilities promise inference that always
# fails. Stage 30 installs it; fail here rather than shipping a half-install.
[ -x "$ROOTFS/usr/libexec/otwono/ai-backends/otwono-llama-backend" ] \
    || die "the llama.cpp adapter is missing from the rootfs; stage 30 must run before this one"

printf '%s\t%s\t%s\n' "llama-server" "$(sha256sum "$SERVER" | cut -d' ' -f1)" \
    "$(stat -c %s "$SERVER")" > "$TARGET_OUT/ai-engine.manifest"
manifest_add "ai-engine" "llama.cpp $LLAMA_REF ($LLAMA_COMMIT) for $ARCH"

stage_mark_complete 35-ai-engine
stage_done

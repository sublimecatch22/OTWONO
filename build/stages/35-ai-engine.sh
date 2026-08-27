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

# Which builds of that engine to install. One image can carry several: `otwono_ai::discovery`
# probes for each variant directory, and `otwono_ai::select_backend` picks between them per
# model from the machine's capability profile. So this is "what is on the disk", never "what
# this machine will use" -- a node with no GPU that ships a Vulkan build simply never selects
# it, which is asserted in otwono-ai's tests rather than assumed here.
#
# Defaults to cpu alone, and that default is load-bearing: every existing recipe and every
# AI_ENGINE=llama.cpp build keeps producing exactly the image it produced before this stage
# learned about variants.
VARIANTS="${AI_ENGINE_VARIANTS:-$(recipe_get_opt ai engine_variants)}"
VARIANTS="${VARIANTS:-cpu}"

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

# What each variant adds to the cmake line, and what it needs on the build host.
#
# Kept as one function per question rather than a table, because the two answers are
# consumed at different times: the tool check must fail *before* a ten-minute build starts,
# and an operator who asked for a variant this host cannot build deserves the package name
# rather than a cmake backtrace.
variant_cmake_args() {
    case "$1" in
        cpu) ;;  # the portable build; GGML_NATIVE=OFF below is what makes it portable
        vulkan) printf '%s\n' -DGGML_VULKAN=ON ;;
        *) die "unknown llama.cpp variant: $1 (known: cpu vulkan)" ;;
    esac
}

variant_require_tools() {
    case "$1" in
        cpu) ;;
        vulkan)
            # llama.cpp compiles its Vulkan compute shaders at build time, so the SDK
            # headers and a shader compiler are both build-host requirements -- neither is
            # implied by having a GPU, and a machine with a GPU and no SDK fails just the
            # same. Named here so the failure arrives in a second rather than in a cmake log.
            [ -f /usr/include/vulkan/vulkan.h ] \
                || die "the vulkan variant needs the Vulkan headers (package: libvulkan-dev)"
            require_tool glslc "package: glslc, or shaderc from the Vulkan SDK"
            ;;
    esac
}

# Cached outside the per-target directory: the same engine build serves every recipe for an
# architecture, and rebuilding it per image would make an otherwise two-minute rebuild take
# twelve. Keyed by variant as well as architecture, because a Vulkan build and a CPU build
# of the same commit are different binaries.
build_one_variant() { # variant
    local VARIANT="$1"
    local CACHE="$OUT/engines/llama.cpp/$LLAMA_REF/$ARCH/$VARIANT"
    local SERVER="$CACHE/bin/llama-server"

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

        BUILD="$OUT/engines/llama.cpp/$LLAMA_REF/build-$ARCH-$VARIANT"
        # Logs go beside the build tree, not inside it: the tree does not exist yet, and a
        # redirect through a path component that is not there fails before cmake ever runs.
        LOGDIR="$OUT/engines/llama.cpp/$LLAMA_REF"
        log "building llama-server for $ARCH/$VARIANT (several minutes; log: $LOGDIR)"
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
        # Deliberate word splitting below: variant_cmake_args prints zero or more separate
        # cmake flags, and quoting it would pass a single empty argument for cpu.
        # shellcheck disable=SC2046
        cmake -S "$SRC" -B "$BUILD" -G Ninja \
            "${CROSS_ARGS[@]}" \
            $(variant_cmake_args "$VARIANT") \
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
            -DLLAMA_BUILD_SERVER=ON >"$LOGDIR/cmake-$ARCH-$VARIANT.log" 2>&1 \
            || { tail -20 "$LOGDIR/cmake-$ARCH-$VARIANT.log" >&2; die "cmake configure failed for $VARIANT"; }
        cmake --build "$BUILD" --target llama-server -j"$(nproc)" >"$LOGDIR/build-$ARCH-$VARIANT.log" 2>&1 \
            || { tail -30 "$LOGDIR/build-$ARCH-$VARIANT.log" >&2; die "llama-server build failed for $VARIANT"; }

        mkdir -p "$CACHE/bin"
        install -m 0755 "$BUILD/bin/llama-server" "$SERVER"
        # The build tree is gigabytes and the artifact is megabytes. Disk is a fixed allowance
        # in the dev environment (CLAUDE.md §11).
        rm -rf "$BUILD"
    fi

    file "$SERVER" | grep -q "$EXPECT_MACHINE" \
        || die "llama-server is not a $EXPECT_MACHINE binary: $(file -b "$SERVER")"
    log "  $VARIANT engine is $EXPECT_MACHINE, $(stat -c %s "$SERVER") bytes"

    # The layout otwono_ai::discovery probes: one directory per variant, all of them beside
    # each other, discovered identically. Which one a given model *uses* is decided at
    # runtime by select_backend from the capability profile, never by what got installed.
    local DEST="$ROOTFS/usr/lib/otwono/ai/llama.cpp/$VARIANT/bin"
    install -d -m 0755 "$DEST"
    install -m 0755 "$SERVER" "$DEST/llama-server"
    log "  installed /usr/lib/otwono/ai/llama.cpp/$VARIANT/bin/llama-server"

    # Every shared library the engine needs must already be in the image. Checked here
    # because the alternative is finding out at boot: the first version of this stage
    # shipped an engine that could not start because libgomp.so.1 was absent, and the only
    # symptom was a failed inference twenty minutes into a QEMU run. `readelf` reads any
    # architecture's ELF, which matters for the cross-built arm64 engine.
    #
    # Per variant, not once: a Vulkan build needs libvulkan.so.1 that a CPU build does not,
    # and that library is the difference between an engine that starts and one that dies on
    # exec. This check is the reason a variant cannot be added to an image without also
    # adding its runtime dependencies to the recipe.
    local MISSING=""
    for lib in $(readelf -d "$SERVER" | awk '/NEEDED/{gsub(/[\[\]]/, "", $NF); print $NF}'); do
        if ! find "$ROOTFS/usr/lib" "$ROOTFS/lib" -name "$lib" 2>/dev/null | grep -q .; then
            MISSING="$MISSING $lib"
        fi
    done
    [ -z "$MISSING" ] || die "the $VARIANT engine needs libraries this image does not have:$MISSING
  add the providing package to the recipe's [packages] include list"
    log "  all $VARIANT engine libraries resolve inside the image"

    printf '%s\t%s\t%s\t%s\n' "llama-server" "$VARIANT" \
        "$(sha256sum "$SERVER" | cut -d' ' -f1)" "$(stat -c %s "$SERVER")" \
        >> "$TARGET_OUT/ai-engine.manifest"
}

require_root

# The adapter has to be there, or discovery reports nothing — deliberately, because an
# engine with no way to drive it would make ai.capabilities promise inference that always
# fails. Stage 30 installs it; fail here rather than shipping a half-install. Checked once
# for all variants: one adapter drives every llama.cpp build.
[ -x "$ROOTFS/usr/libexec/otwono/ai-backends/otwono-llama-backend" ] \
    || die "the llama.cpp adapter is missing from the rootfs; stage 30 must run before this one"

# Tools first, for every variant, before any build starts. An operator who asked for two
# variants and can build only the first should learn that now and not after ten minutes of
# compiling the one that was going to work anyway.
for VARIANT in $VARIANTS; do
    variant_cmake_args "$VARIANT" >/dev/null   # rejects an unknown name
    variant_require_tools "$VARIANT"
done

: > "$TARGET_OUT/ai-engine.manifest"
for VARIANT in $VARIANTS; do
    log "variant: $VARIANT"
    build_one_variant "$VARIANT"
done

manifest_add "ai-engine" "llama.cpp $LLAMA_REF ($LLAMA_COMMIT) for $ARCH [$VARIANTS]"

stage_mark_complete 35-ai-engine
stage_done

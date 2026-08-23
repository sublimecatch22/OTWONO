#!/usr/bin/env bash
# Stage 36 — bundle a synthetic model and a boot-time inference check.
#
# Network access: PyPI, to build the venv that generates the model (numpy, blake3, pyyaml).
# Privileges: root (writes into the rootfs).
#
# Off by default, and it must stay that way: this stage puts a **test model** and a
# **widened permission policy** into the image. Enable with AI_SMOKE_MODEL=1, which also
# requires AI_ENGINE=llama.cpp — an inference check with no engine would only ever fail.
#
# Why bundle a model at all: until now inference had only ever been proven on a build host.
# The daemons, the broker, capability tokens, Landlock, the systemd hardening and the engine
# had never run together on a booted node on either architecture, and each of those is a
# place where something works in a test harness and not in an image.
#
# The model is generated, not downloaded and not committed (CLAUDE.md §9): random weights,
# ~400 KB, whose output is gibberish. That is enough to prove the path and is honest about
# proving nothing else.
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"
stage_begin 36-ai-smoke-model

ROOTFS="$TARGET_OUT/rootfs"
[ -d "$ROOTFS/usr" ] || die "no rootfs at $ROOTFS; run stage 10 first"

SMOKE="${AI_SMOKE_MODEL:-$(recipe_get_opt ai smoke_model)}"
if [ -z "$SMOKE" ] || [ "$SMOKE" = "0" ]; then
    log "no smoke model requested; this image ships no model and no ai.admin policy"
    manifest_add "ai-smoke-model" "none"
    stage_mark_complete 36-ai-smoke-model
    stage_done
    exit 0
fi

ENGINE_BIN="$ROOTFS/usr/lib/otwono/ai/llama.cpp/cpu/bin/llama-server"
[ -x "$ENGINE_BIN" ] \
    || die "AI_SMOKE_MODEL needs an engine in the image; build with AI_ENGINE=llama.cpp"
[ -x "$ROOTFS/usr/bin/otwono-aictl" ] \
    || die "the smoke check needs otwono-aictl in the image; stage 30 must run before this one"

LLAMA_REF="${AI_ENGINE_REF:-$(recipe_get_opt ai llama_cpp_ref)}"
LLAMA_REF="${LLAMA_REF:-b10588}"
GGUF_PY="$OUT/engines/llama.cpp/$LLAMA_REF/src/gguf-py"
[ -d "$GGUF_PY" ] || die "no gguf-py at $GGUF_PY; stage 35 keeps the engine source there"

# A venv, cached across builds. gguf-py needs numpy and pyyaml; the manifest needs blake3,
# because the digest has to be computed from the bytes that were actually written — a
# guessed digest would only ever produce a refusal from ai.models.install.
VENV="$OUT/engines/pyvenv"
if [ ! -x "$VENV/bin/python" ]; then
    log "creating the model-generation venv (network: PyPI)"
    require_tool python3
    python3 -m venv "$VENV" >/dev/null || die "cannot create a venv at $VENV"
    "$VENV/bin/pip" install --quiet numpy pyyaml blake3 \
        || die "cannot install numpy, pyyaml and blake3 into $VENV"
fi

DEST="$ROOTFS/usr/share/otwono/smoke-model"
require_root
install -d -m 0755 "$DEST"

log "generating the synthetic model"
"$VENV/bin/python" "$REPO_ROOT/tools/make-tiny-gguf.py" \
    --gguf-py "$GGUF_PY" \
    --out "$DEST/model.gguf" \
    --manifest "$DEST/manifest.json" \
    --model-id otwono-smoke-test \
    | while read -r line; do log "  $line"; done
[ -s "$DEST/model.gguf" ] || die "the model generator produced nothing"
chmod 0644 "$DEST/model.gguf" "$DEST/manifest.json"

# The default policy grants none of the AI actions, and it must not start: this drop-in is
# the widening, it exists only in a smoke image, and it says so at the top so an operator
# who finds it on a real machine knows immediately what they are looking at.
log "installing the smoke-test policy drop-in"
cat > "$ROOTFS/etc/otwono/policy.d/90-ai-smoke.toml" <<'POLICY'
# BUILT FOR TESTING. This file grants the AI actions the boot-time inference check needs,
# including ai.admin, which lets a caller change what this node will run.
#
# It is installed only by build stage 36 (AI_SMOKE_MODEL=1) alongside a synthetic model.
# A release image ships neither. If you are reading this on a machine you care about,
# delete it: the default policy in 10-default.toml grants no AI action at all.

[[rule]]
action = "ai.read"
subjects = ["uid:0"]
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "ai.infer"
subjects = ["uid:0"]
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "ai.admin"
subjects = ["uid:0"]
decision = "allow"
ttl_seconds = 300
POLICY
chmod 0644 "$ROOTFS/etc/otwono/policy.d/90-ai-smoke.toml"

log "installing the boot-time inference check"
install -m 0755 "$BUILD_DIR/files/otwono-ai-infer-check" "$ROOTFS/usr/lib/otwono/ai-infer-check"
cat > "$ROOTFS/etc/systemd/system/otwono-ai-infer-check.service" <<'UNIT'
[Unit]
Description=OTWONO boot-time inference check
Documentation=file:/usr/share/doc/otwono/AI-RUNTIME.md
# After the AI self check, so the socket and catalog layout are known good before this
# spends time loading a model.
After=otwono-ai-check.service
Requires=otwono-ai-check.service
RequiresMountsFor=/var/lib/otwono
Before=multi-user.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/lib/otwono/ai-infer-check
StandardOutput=journal+console
StandardError=journal+console
# Generous: this runs under TCG emulation in CI, where everything is a hundred times
# slower than the hardware it is pretending to be.
TimeoutStartSec=600

NoNewPrivileges=yes
ProtectHome=yes
PrivateTmp=yes
RestrictSUIDSGID=yes
LockPersonality=yes

[Install]
WantedBy=multi-user.target
UNIT

chroot "$ROOTFS" systemctl enable otwono-ai-infer-check.service 2>/dev/null \
    || warn "could not enable otwono-ai-infer-check.service"

log ""
log "NOTE: this image contains a synthetic test model and a policy granting ai.admin."
log "      It is not a release image."
manifest_add "ai-smoke-model" "otwono-smoke-test ($(stat -c %s "$DEST/model.gguf") bytes, unsigned)"

stage_mark_complete 36-ai-smoke-model
stage_done

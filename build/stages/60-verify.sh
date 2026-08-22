#!/usr/bin/env bash
# Stage 60 — verify the image: structure, checksums, then an actual QEMU boot.
#
# Network access: none.
# Privileges: none (QEMU with user-mode networking).
#
# The boot test is the only thing that makes an image "done". It must reach a login prompt
# AND emit a capability profile from inside the VM — booting to a shell proves the image
# works, and the profile proves hardware detection works on the real target rather than
# only on the build host.
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"
stage_begin 60-verify

ARCH="$(recipe_get target arch)"
IMAGE="$TARGET_OUT/otwono-$TARGET.img"
BOOT_LOG="$TARGET_OUT/boot.log"

[ -f "$IMAGE" ] || die "no image at $IMAGE; run stage 50 first"

log "verifying checksums"
( cd "$TARGET_OUT" && sha256sum -c SHA256SUMS ) || die "checksum mismatch"

log "verifying the partition table"
sgdisk --verify "$IMAGE" > /dev/null || die "the partition table does not verify"
for want in OTWONO-ESP OTWONO-ROOT-A OTWONO-ROOT-B OTWONO-DATA; do
    sgdisk --print "$IMAGE" | grep -q "$want" || die "missing partition: $want"
done
log "  all four partitions present"

# Boot a copy, never the artifact. A guest writes its first-boot state — a generated node
# identity, profiles, an audit log — into its own disk. Booting the release image in place
# therefore bakes one node's private key into the file every device is flashed from, and
# invalidates the SHA256SUMS computed in stage 50. Both of those happened before this
# changed; see docs/build/VERIFICATION-LOG.md.
BOOT_IMAGE="$TARGET_OUT/boot-under-test.img"
log "copying the image so the boot cannot mutate the artifact"
rm -f "$BOOT_IMAGE"
cp --sparse=always "$IMAGE" "$BOOT_IMAGE"

log "booting under QEMU (no KVM here, so TCG; this takes minutes)"
"$BUILD_DIR/qemu/run-$ARCH.sh" --image "$BOOT_IMAGE" --boot-test --log "$BOOT_LOG" \
    || die "boot test failed; see $BOOT_LOG"

# --- Recover the profile the guest wrote to its own data partition ----------------------
#
# Console output proves the report ran. Reading the JSON back out of the disk the guest
# wrote proves the whole path worked: probe, classify, serialise, persist — on the target
# architecture, not the build host. Extracted with debugfs so this still needs no mounts.
log "recovering artifacts the guest wrote"
WORK="$TARGET_OUT/verify-work"
rm -rf "$WORK"; mkdir -p "$WORK"

# Pull a partition out of the image and dump one file from it. debugfs reads ext4 directly,
# so this needs no loop device and no mount — the same constraint stage 50 builds under.
dump_guest_file() { # partition-number guest-path output-path
    local part="$1" guest="$2" out="$3" start sectors img
    img="$WORK/p$part.img"
    if [ ! -f "$img" ]; then
        start=$(partx -g -o START -s --nr "$part" "$BOOT_IMAGE" | tr -d ' ')
        sectors=$(partx -g -o SECTORS -s --nr "$part" "$BOOT_IMAGE" | tr -d ' ')
        dd if="$BOOT_IMAGE" of="$img" bs=512 skip="$start" count="$sectors" status=none
    fi
    rm -f "$out"
    debugfs -R "dump $guest $out" "$img" 2>/dev/null || true
    [ -s "$out" ]
}

PROFILE="$TARGET_OUT/capability-profile.json"
dump_guest_file 4 "/capability-profile.json" "$PROFILE" \
    || die "the guest booted but wrote no capability profile to its data partition.
       Console log: $BOOT_LOG"

command -v jq > /dev/null && {
    jq -e '.schema_version and .tier and .axes and .features' "$PROFILE" > /dev/null \
        || die "the recovered profile is not a well-formed capability profile: $PROFILE"
    log "profile recovered from the guest's data partition:"
    log "  tier            $(jq -r .tier "$PROFILE")"
    log "  limiting factor $(jq -r '.limiting_factor // "none"' "$PROFILE")"
    log "  architecture    $(jq -r .hardware.machine.architecture "$PROFILE")"
    log "  axes            $(jq -r '.axes | to_entries | map("\(.key)=\(.value)") | join(" ")' "$PROFILE")"
}

# The architecture in the profile must match the target, or we booted the wrong image.
EXPECT_ARCH=$([ "$ARCH" = "arm64" ] && echo aarch64 || echo x86_64)
GOT_ARCH=$(jq -r .hardware.machine.architecture "$PROFILE" 2>/dev/null || echo "?")
[ "$GOT_ARCH" = "$EXPECT_ARCH" ] \
    || die "profile reports architecture '$GOT_ARCH' but this target is $ARCH ($EXPECT_ARCH)"
log "  architecture matches the target"

# --- Control plane -------------------------------------------------------------------
# The guest fetched this one through otwono-permd and otwono-hwd. Recovering it proves the
# daemons, the policy, the token path and the audit log all worked at boot, rather than
# just that a binary ran.
CP_PROFILE="$TARGET_OUT/control-plane-profile.json"
dump_guest_file 4 "/control-plane-profile.json" "$CP_PROFILE" \
    || die "the guest never wrote the control-plane profile; the daemons did not serve it.
       Console log: $BOOT_LOG"

CP_TIER=$(jq -r .tier "$CP_PROFILE" 2>/dev/null || echo "<unreadable>")
LOCAL_TIER=$(jq -r .tier "$PROFILE" 2>/dev/null || echo "")
log "control-plane profile recovered:"
log "  tier via daemons  $CP_TIER"
[ "$CP_TIER" = "$LOCAL_TIER" ] \
    || die "the daemon reported tier $CP_TIER but the local probe reported $LOCAL_TIER"
log "  matches the locally probed tier"
grep -a -o "OTWONO-CONTROL-PLANE-OK.*" "$BOOT_LOG" | head -1 | sed 's/^/[60-verify]   console: /'

# --- Audit chain ----------------------------------------------------------------------
# Verified on the host, against the log the guest actually wrote, using the same code the
# broker uses. A hash chain nobody checks is decoration.
AUDIT="$TARGET_OUT/guest-audit.jsonl"
if dump_guest_file 2 "/var/log/otwono/audit.jsonl" "$AUDIT"; then
    VERIFIER="$REPO_ROOT/target/release/otwono-permd"
    if [ ! -x "$VERIFIER" ]; then
        log "building a host-native verifier"
        ( cd "$REPO_ROOT" && cargo build --release --quiet -p otwono-permd )
    fi
    "$VERIFIER" --verify-audit "$AUDIT" | sed 's/^/[60-verify]   /' \
        || die "the guest's audit chain does not verify"
else
    die "the guest wrote no audit log; the broker did not run"
fi

# --- The shipped artifact must carry no first-boot state -------------------------------
# Regression guard for the defect above: a node key inside a distributable image means
# every device flashed from it shares one identity and can impersonate the others.
log "verifying the release image carries no first-boot state"
PRISTINE_WORK="$TARGET_OUT/pristine-check"
rm -rf "$PRISTINE_WORK"; mkdir -p "$PRISTINE_WORK"
check_absent() { # partition-number guest-path description
    local part="$1" guest="$2" what="$3" start sectors img out
    img="$PRISTINE_WORK/p$part.img"
    if [ ! -f "$img" ]; then
        start=$(partx -g -o START -s --nr "$part" "$IMAGE" | tr -d ' ')
        sectors=$(partx -g -o SECTORS -s --nr "$part" "$IMAGE" | tr -d ' ')
        dd if="$IMAGE" of="$img" bs=512 skip="$start" count="$sectors" status=none
    fi
    out="$PRISTINE_WORK/found"
    rm -f "$out"
    debugfs -R "dump $guest $out" "$img" 2>/dev/null || true
    if [ -s "$out" ]; then
        die "the release image contains $what ($guest); a boot mutated the artifact"
    fi
}
check_absent 4 "/identity/node.key" "a private node key"
check_absent 4 "/identity/agreement.key" "a private agreement key"
check_absent 4 "/capability-profile.json" "a first-boot capability profile"
check_absent 4 "/control-plane-profile.json" "a first-boot control-plane profile"
check_absent 2 "/var/log/otwono/audit.jsonl" "an audit log from a previous boot"

# A seeded machine-id is the same class of defect as a seeded node key: one value shared by
# every device flashed from the image. systemd derives per-host secrets from it, the IPv4
# link-local address among them, so two nodes from one image collide on a DHCP-less segment.
# It must be present (systemd needs the file) and empty (its "generate one" marker).
MACHINE_ID_PART="$PRISTINE_WORK/p2.img"
if [ ! -f "$MACHINE_ID_PART" ]; then
    mi_start=$(partx -g -o START -s --nr 2 "$IMAGE" | tr -d ' ')
    mi_sectors=$(partx -g -o SECTORS -s --nr 2 "$IMAGE" | tr -d ' ')
    dd if="$IMAGE" of="$MACHINE_ID_PART" bs=512 skip="$mi_start" count="$mi_sectors" status=none
fi
rm -f "$PRISTINE_WORK/machine-id"
debugfs -R "dump /etc/machine-id $PRISTINE_WORK/machine-id" "$MACHINE_ID_PART" 2>/dev/null || true
[ -f "$PRISTINE_WORK/machine-id" ] \
    || die "the image has no /etc/machine-id; systemd needs the file to exist"
[ ! -s "$PRISTINE_WORK/machine-id" ] \
    || die "the image ships a seeded /etc/machine-id ($(tr -d '\n' < "$PRISTINE_WORK/machine-id")); every device flashed from it would be the same machine"

rm -rf "$PRISTINE_WORK"
log "  no identity, profiles, audit log or seeded machine-id in the artifact"

log "re-verifying checksums against the untouched artifact"
( cd "$TARGET_OUT" && sha256sum -c SHA256SUMS ) \
    || die "the image no longer matches the checksum stage 50 recorded"

rm -rf "$WORK"
rm -f "$BOOT_IMAGE"

log "boot log: $BOOT_LOG"
manifest_add "boot-test" "passed; tier $(jq -r .tier "$PROFILE" 2>/dev/null || echo unknown), log $(basename "$BOOT_LOG")"
stage_mark_complete 60-verify
stage_done

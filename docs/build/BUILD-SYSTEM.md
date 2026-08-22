# Reproducible Build System

**Status:** `VERIFIED` on **amd64** — the full pipeline builds an image that boots under
QEMU to a login prompt and produces a capability profile from inside the VM.
**arm64 is not yet verified.**

See `docs/build/VERIFICATION-LOG.md` for exactly what has been executed and what has not.

## 1. Strategy

A Debian-derived rootfs assembled by numbered, idempotent stages driven by a declarative
recipe. Rationale and the rejected alternatives (Yocto, Buildroot, NixOS, Arch/Alpine) are
in `docs/architecture/OTWONO-ARCHITECTURE.md` §3 and ADR-0002.

## 2. Stages

| Stage | Does | Network |
|---|---|---|
| `00-probe-env.sh` | Verify host tools, mirror reachability, disk, binfmt, KVM. Fail early and loudly. | Yes (HEAD requests only) |
| `10-bootstrap.sh` | `debootstrap`/`mmdebstrap` a minimal base rootfs for `$ARCH` | Yes |
| `20-base-config.sh` | Locale, hostname, users, fstab, systemd presets, apt pinning | No |
| `30-otwono.sh` | Install OTWONO daemons, units, schemas, default policy | No |
| `40-kernel.sh` | Kernel, initramfs, firmware, arm64 device trees / U-Boot | Yes |
| `50-image.sh` | GPT (ESP + root_a + root_b + data), populate, bootloader | No |
| `60-verify.sh` | Checksums, manifest, QEMU boot smoke test | No |

Every stage: idempotent, declares its network needs in a header comment, writes to
`out/<target>/<stage>/`, appends to the build manifest.

## 3. Recipes

`build/recipes/<target>.toml` is the only place a target's parameters live:

```toml
[target]
id = "amd64-qemu"
arch = "amd64"
[base]
distro = "debian"; suite = "trixie"
mirror = "https://deb.debian.org/debian"
snapshot = "20260801T000000Z"
[image]
size = "8G"
layout = "ab"        # ESP + root_a + root_b + data
```

`BASE_DISTRO`, `BASE_SUITE`, `BASE_MIRROR`, and `ARCH` are never hardcoded in a stage.

## 4. Reproducibility

| Mechanism | Purpose |
|---|---|
| `SOURCE_DATE_EPOCH` | Deterministic timestamps in tar, ext4, generated files |
| Snapshot-pinned mirror | Identical package versions across runs |
| `manifest.lock` | Package, version, arch, sha256 — an auditable bill of materials |
| Deterministic UUIDs derived from recipe + epoch | Bit-identical images |
| `SHA256SUMS` per artifact | Verification, and a basis for update deltas |

0.x target: byte-identical rootfs tarball across two runs on the same host with the same
snapshot pin. Cross-host bit-reproducibility is a 1.x goal.

## 5. Usage

```bash
make -C build probe                       # environment probe, always run this first
make -C build TARGET=amd64-qemu rootfs
make -C build TARGET=amd64-qemu image
make -C build TARGET=amd64-qemu boot-test
make -C build TARGET=arm64-qemu image
make -C build clean
```

## 6. Environment constraints observed in OTWONO Cloud

Recorded from a real probe (`tools/probe-env.sh`), 2026-08-22:

| Fact | Consequence |
|---|---|
| No `/dev/kvm` | QEMU under TCG. Boot tests take minutes; harnesses use long timeouts and stream serial to a log. |
| Egress proxy allow-lists mirrors: `archive.ubuntu.com` and `ports.ubuntu.com` reachable; `deb.debian.org` and other Debian/Alpine mirrors rejected (403 on CONNECT) | Debian stays the canonical product base; CI in this environment uses the `ubuntu-noble` recipe. Both must keep working. |
| No Docker daemon; `podman` present | Container stages use podman or plain chroot |
| `binfmt_misc` not mounted at boot, mountable by root | `10-bootstrap` mounts it before a foreign-arch second stage |
| `aarch64-linux-gnu-gcc` and Rust `aarch64-unknown-linux-{gnu,musl}` present | Native cross-compilation works |
| Writable disk is a fixed allowance | Clean `out/` aggressively; a full rootfs plus image is several GiB |

## 7. Non-negotiables

- Artifacts go to `out/`, which is git-ignored. **Never commit an image.**
- No stage may perform undeclared network access.
- A build that cannot be reproduced is a build that cannot be audited, and an OS that
  cannot be audited has no business holding someone's private keys.

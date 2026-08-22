# OTWONO build system

See [`docs/build/BUILD-SYSTEM.md`](../docs/build/BUILD-SYSTEM.md) for the design.

## Status of each stage

| Stage | Status | Notes |
|---|---|---|
| `probe` (`tools/probe-env.sh`) | **VERIFIED** | Run in the OTWONO Cloud dev environment; output recorded |
| `05-host-tools` | **VERIFIED** | Cross-built for amd64 and arm64; the arm64 binary was executed under `qemu-aarch64-static` |
| `10-bootstrap` | **VERIFIED** | amd64 native and arm64 cross-bootstrap (binfmt_misc + qemu-user second stage) both completed |
| `20-base-config` | **VERIFIED** | Packages installed in the chroot; serial getty enabled |
| `30-otwono` | **VERIFIED** | Binaries, schemas, units installed; `otwono-hwctl` runs inside the rootfs |
| `40-kernel` | **NOT IMPLEMENTED** | Phase 1. Fails loudly with the intended plan rather than producing a kernel-less image |
| `50-image` | **NOT IMPLEMENTED** | Phase 1. Same |
| `60-verify` | **PARTIAL** | The QEMU harnesses are implemented and runnable; there is no image to boot yet |

## Targets

| Target | Base | Bootstraps in OTWONO Cloud? |
|---|---|---|
| `amd64-qemu` | Debian trixie | No — the egress proxy rejects Debian mirrors |
| `amd64-qemu-ubuntu` | Ubuntu noble | Yes |
| `arm64-qemu` | Debian trixie | No — same reason |
| `arm64-qemu-ubuntu` | Ubuntu noble ports | Yes |

Debian is the canonical product base (ADR-0002). The Ubuntu recipes exist because that is
what CI in this environment can actually reach. Both must keep working.

## Adding a board

Create `build/bsp/<vendor>-<board>/bsp.toml` rather than forking a recipe. Mainline kernel
first; a vendor kernel is a documented exception with an exit plan.

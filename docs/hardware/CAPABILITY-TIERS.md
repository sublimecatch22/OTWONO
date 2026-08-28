# Hardware Detection and Capability Tiers

**Status:** `IMPLEMENTED` (probes + classifier + CLI, unit and fixture tested)
· `SPECIFIED` (hotplug events, NPU probing beyond sysfs presence, `otwono-hwd` daemon)

## 1. Purpose

One component decides what this machine can do. Everything else reads its answer.

`otwono-hal` probes the hardware. `otwono-capability` classifies the probe into a
capability profile. `otwono-hwctl` prints it. Later, `otwono-hwd` will publish it on the
Local Control Plane and emit change events on hotplug.

No other subsystem may re-derive "is this machine big enough".

## 2. The probe is injectable

Every probe reads from a **root path**, not from a hardcoded `/proc` or `/sys`.

```rust
let probe = SystemProbe::from_root(Path::new("/"));                 // production
let probe = SystemProbe::from_root(Path::new("tests/fixtures/pi5")); // test
```

This is the single most important design decision in this subsystem: it is what makes it
possible to test detection for a Raspberry Pi 5, an RK3588 board, and a CUDA workstation
from a CI runner that is none of those things.

Fixtures are captured with `tools/capture-hw-fixture.sh`, which copies the exact set of
`/proc` and `/sys` files the probes read.

## 3. Axes

Each axis is classified independently. See the schema at
`schemas/capability-profile.schema.json` for the authoritative field list.

### 3.1 `compute`

Sources: `/proc/cpuinfo`, `/sys/devices/system/cpu/`, `/proc/device-tree/model`.

Collected: logical CPUs, physical cores, architecture, vendor/model, max frequency,
ISA extension flags (x86: `avx2`, `avx512f`, `f16c`, `amx_*`; arm64: `asimd`, `sve`,
`asimddp`, `i8mm`, `bf16`), and big.LITTLE topology.

`asimddp` (dot product) and `i8mm` matter far more than core count for quantized LLM
inference on arm64, which is why the flags are collected rather than just the core count.

| Class | Rule of thumb |
|---|---|
| `minimal` | ≤2 cores |
| `low` | 3–4 cores |
| `medium` | 5–8 cores, or 4 cores with modern vector ISA |
| `high` | 9–16 cores with modern vector ISA |
| `extreme` | >16 cores with modern vector ISA |

### 3.2 `memory`

Sources: `/proc/meminfo`.

Collected: `MemTotal`, `MemAvailable`, `SwapTotal`.

| Class | Total RAM |
|---|---|
| `minimal` | <2 GiB |
| `low` | 2–<6 GiB |
| `medium` | 6–<14 GiB |
| `high` | 14–<30 GiB |
| `extreme` | ≥30 GiB |

Thresholds sit slightly below the marketing number (14 GiB for a "16 GB" machine) because
firmware and the GPU aperture always take a bite.

### 3.3 `accelerator`

Sources: `/sys/class/drm/`, `/sys/bus/pci/devices/`, `/proc/device-tree/`,
`/sys/class/accel/`, vendor nodes (`/dev/nvidia*`, `/dev/kfd`, `/dev/dri/renderD*`,
`/dev/rknpu`, `/dev/accel/accel*`).

Collected: GPU vendor/device, driver, VRAM bytes where discoverable, discrete vs
integrated, compute APIs, and NPU presence with a TOPS estimate where the device is known.

| Class | Meaning |
|---|---|
| `none` | No usable accelerator |
| `npu_small` | An NPU only (RKNN, Hailo, Coral, Intel NPU, AMD XDNA) |
| `igpu` | Integrated GPU with a usable compute API |
| `gpu_small` | Discrete GPU, <12 GiB VRAM |
| `gpu_large` | Discrete GPU, ≥12 GiB VRAM |
| `gpu_multi` | More than one discrete GPU |

VRAM is genuinely hard to read portably. Current sources, in order: amdgpu
`mem_info_vram_total`, NVIDIA via `nvidia-smi` when present, PCI BAR size as a weak
fallback. When VRAM is unknown the profile says `null` and the classifier does **not**
guess upward — an unknown accelerator never unlocks a tier it cannot sustain.

### 3.4 `storage`

Sources: `/sys/block/*/` (`size`, `queue/rotational`), `statvfs` on the data path.

| Class | Rule |
|---|---|
| `constrained` | <16 GiB free |
| `standard` | 16–<128 GiB free |
| `fast` | ≥128 GiB free on non-rotational |
| `bulk` | ≥1 TiB free |

Storage gates model downloads and replication roles. A T3 GPU with 8 GiB free disk cannot
host a 32B model, and the profile must say so before the download starts, not after.

### 3.5 `network`

Sources: `/sys/class/net/*/` (`type`, `operstate`, `speed`, `wireless/`), plus a
non-blocking uplink probe.

Collected: interfaces with link type and state, presence of an uplink, and radio hardware
relevant to ONM (Wi-Fi with AP/mesh capability, LoRa SPI/USB modules, 802.15.4, BLE).

| Class | Meaning |
|---|---|
| `offline` | No usable link |
| `intermittent` | Links present, no reliable uplink |
| `lan` | Local network, no uplink |
| `broadband` | Reliable uplink |
| `gateway` | Reliable uplink and capable of bridging for other nodes |

### 3.6 `power`

Sources: `/sys/class/power_supply/`, thermal zones.

| Class | Meaning |
|---|---|
| `constrained` | Battery or a strict power budget (PoE, USB-powered SBC) |
| `managed` | AC with thermal limits worth respecting (laptops, passively-cooled SBCs) |
| `unconstrained` | Desktop/server power and cooling |

Power gates *sustained* workloads. A laptop on battery is not the machine it is on AC, and
running a 32B model on a passively-cooled SBC until it throttles is a bad experience, not
a feature.

## 4. Overall tier composition

The overall tier is the **highest tier whose every requirement is met** — the weakest
binding axis wins.

| Tier | `memory` ≥ | `compute` ≥ | `accelerator` | `storage` ≥ |
|---|---|---|---|---|
| `T0_MICRO` | (floor) | (floor) | any | any |
| `T1_EDGE` | `low` | `low` | any | `standard` |
| `T2_BALANCED` | `medium` | `medium` | any | `standard` |
| `T3_CAPABLE` | `high` | `medium` | `gpu_small`+ | `fast` |
| `T4_WORKSTATION` | `extreme` | `high` | `gpu_large`+ | `fast` |

Because it is a minimum over axes, the awkward machines land sensibly:

- Pi 5 / 16 GB, no GPU → `high` memory, `medium` compute, `none` accelerator ⇒ **T2**.
  Correct: plenty of RAM, but no GPU means 7–8B CPU inference, not 32B.
- Gaming laptop, 16 GB RAM + RTX 4060 8 GB, 4 cores → `high` memory, `low` compute,
  `gpu_small` ⇒ **T2**, and it will *feel* like T3 for GPU-offloaded models. This is the
  known coarseness of a tier scalar, and the reason subsystems are encouraged to read the
  axis vector rather than the tier when they can.

## 5. Overrides

`/etc/otwono/capability.override.toml`:

```toml
# Force a tier. Forcing upward is allowed and is your problem.
tier = "T3_CAPABLE"

# Or override individual axes.
[axes]
accelerator = "gpu_large"

# Or state facts the probes cannot discover.
[facts]
vram_bytes = 24_000_000_000
```

Overrides are recorded in the profile as `overridden: true` with the original values kept
in `detected`, so a bug report shows both what was found and what the user forced.

## 6. Output contract

`otwono-hwctl profile --json` emits a document validated against
`schemas/capability-profile.schema.json`. It carries `schema_version`, the raw detected
hardware, the per-axis classes, the overall tier, and the derived feature gates.

Consumers must read the schema fields, not parse the human-readable output.

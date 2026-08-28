//! Per-axis classification.
//!
//! Thresholds live here and nowhere else. They are deliberately expressed against
//! *observed* values (GiB as the kernel reports them), not marketing numbers, which is why
//! a "16 GB" machine is checked against 14 GiB — firmware and the GPU aperture always take
//! a bite before the kernel sees the RAM.

use otwono_hal::{AcceleratorKind, HardwareReport};
use serde::{Deserialize, Serialize};

const GIB: u64 = 1024 * 1024 * 1024;

macro_rules! ordered_class {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self { $(Self::$variant => $s),+ }
            }
        }
    };
}

ordered_class!(
    /// CPU capability for inference workloads.
    ComputeClass { Minimal => "minimal", Low => "low", Medium => "medium", High => "high", Extreme => "extreme" }
);
ordered_class!(
    /// System RAM.
    MemoryClass { Minimal => "minimal", Low => "low", Medium => "medium", High => "high", Extreme => "extreme" }
);
ordered_class!(
    /// Inference accelerators, ordered by usefulness for LLM work.
    AcceleratorClass {
        None => "none", NpuSmall => "npu_small", Igpu => "igpu",
        GpuSmall => "gpu_small", GpuLarge => "gpu_large", GpuMulti => "gpu_multi"
    }
);
ordered_class!(
    /// Free space and speed on the data path.
    StorageClass { Constrained => "constrained", Standard => "standard", Fast => "fast", Bulk => "bulk" }
);
ordered_class!(
    /// Connectivity, from fully isolated to Internet gateway.
    NetworkClass {
        Offline => "offline", Intermittent => "intermittent", Lan => "lan",
        Broadband => "broadband", Gateway => "gateway"
    }
);
ordered_class!(
    /// Sustained power and thermal headroom.
    PowerClass { Constrained => "constrained", Managed => "managed", Unconstrained => "unconstrained" }
);

/// The capability vector. Subsystems are encouraged to read the axis they care about
/// rather than the composed tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityAxes {
    pub compute: ComputeClass,
    pub memory: MemoryClass,
    pub accelerator: AcceleratorClass,
    pub storage: StorageClass,
    pub network: NetworkClass,
    pub power: PowerClass,
}

impl CapabilityAxes {
    pub fn from_report(r: &HardwareReport) -> Self {
        CapabilityAxes {
            compute: classify_compute(r),
            memory: classify_memory(r),
            accelerator: classify_accelerator(r),
            storage: classify_storage(r),
            network: classify_network(r),
            power: classify_power(r),
        }
    }
}

fn classify_compute(r: &HardwareReport) -> ComputeClass {
    let cores = r.cpu.physical_cores.max(r.cpu.logical_cpus);
    let modern = r.cpu.features.has_modern_vector_isa();
    match cores {
        0..=2 => ComputeClass::Minimal,
        3..=4 => {
            // Four modern cores (Pi 5, recent laptops) genuinely outrun four old ones.
            if modern {
                ComputeClass::Medium
            } else {
                ComputeClass::Low
            }
        }
        5..=8 => {
            if modern {
                ComputeClass::Medium
            } else {
                ComputeClass::Low
            }
        }
        9..=16 => {
            if modern {
                ComputeClass::High
            } else {
                ComputeClass::Medium
            }
        }
        _ => {
            if modern {
                ComputeClass::Extreme
            } else {
                ComputeClass::High
            }
        }
    }
}

fn classify_memory(r: &HardwareReport) -> MemoryClass {
    match r.memory.total_bytes {
        b if b < 2 * GIB => MemoryClass::Minimal,
        b if b < 6 * GIB => MemoryClass::Low,
        b if b < 14 * GIB => MemoryClass::Medium,
        b if b < 30 * GIB => MemoryClass::High,
        _ => MemoryClass::Extreme,
    }
}

fn classify_accelerator(r: &HardwareReport) -> AcceleratorClass {
    let gpus: Vec<_> = r
        .accelerators
        .iter()
        .filter(|a| a.kind == AcceleratorKind::Gpu)
        .collect();
    let discrete: Vec<_> = gpus.iter().filter(|a| a.discrete).collect();
    let has_npu = r.accelerators.iter().any(|a| a.kind == AcceleratorKind::Npu);

    if discrete.len() > 1 {
        return AcceleratorClass::GpuMulti;
    }
    if let Some(g) = discrete.first() {
        // An unknown VRAM size must never round upward: an over-optimistic tier turns into
        // an OOM kill the first time the user asks a question.
        return match g.vram_bytes {
            Some(v) if v >= 12 * GIB => AcceleratorClass::GpuLarge,
            Some(_) => AcceleratorClass::GpuSmall,
            None => AcceleratorClass::GpuSmall,
        };
    }
    // An integrated GPU with a usable compute API beats an NPU for general LLM work today;
    // NPUs are strong on fixed quantized models but weak on arbitrary GGUF.
    if gpus.iter().any(|g| !g.compute_apis.is_empty()) {
        return AcceleratorClass::Igpu;
    }
    if has_npu {
        return AcceleratorClass::NpuSmall;
    }
    AcceleratorClass::None
}

fn classify_storage(r: &HardwareReport) -> StorageClass {
    let free = r.storage.data_free_bytes;
    if free < 16 * GIB {
        return StorageClass::Constrained;
    }
    if free >= 1024 * GIB && !r.storage.rotational_only {
        return StorageClass::Bulk;
    }
    if free >= 128 * GIB && !r.storage.rotational_only {
        return StorageClass::Fast;
    }
    StorageClass::Standard
}

fn classify_network(r: &HardwareReport) -> NetworkClass {
    if !r.network.has_link {
        return NetworkClass::Offline;
    }
    if !r.network.has_default_route {
        return NetworkClass::Lan;
    }
    // Gateway capability is a *hardware* statement here: more than one usable link, so the
    // node could bridge. Whether it actually does is a user decision, never automatic.
    let usable_links = r
        .network
        .interfaces
        .iter()
        .filter(|i| i.operstate == "up" && !matches!(i.kind, otwono_hal::network::InterfaceKind::Loopback))
        .count();
    if usable_links > 1 {
        NetworkClass::Gateway
    } else {
        NetworkClass::Broadband
    }
}

fn classify_power(r: &HardwareReport) -> PowerClass {
    if r.power.on_battery {
        return PowerClass::Constrained;
    }
    if r.power.has_battery {
        return PowerClass::Managed;
    }
    PowerClass::Unconstrained
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;

    #[test]
    fn memory_thresholds_sit_below_marketing_numbers() {
        let mut r = report_pi4_4gb();
        // A "16 GB" board typically reports ~15.6 GiB after firmware carve-out.
        r.memory.total_bytes = 15_600 * 1024 * 1024;
        assert_eq!(classify_memory(&r), MemoryClass::High);
        // A "32 GB" machine reporting 31.2 GiB must not fall to `high`.
        r.memory.total_bytes = 31_200 * 1024 * 1024;
        assert_eq!(classify_memory(&r), MemoryClass::Extreme);
    }

    #[test]
    fn four_modern_cores_beat_four_old_ones() {
        let mut old = report_pi4_4gb(); // A72, no dotprod
        old.cpu.physical_cores = 4;
        assert_eq!(classify_compute(&old), ComputeClass::Low);

        let new = report_pi5_16gb(); // A76, dotprod
        assert_eq!(classify_compute(&new), ComputeClass::Medium);
    }

    #[test]
    fn unknown_vram_is_classified_conservatively() {
        let mut r = report_workstation();
        r.accelerators[0].vram_bytes = None;
        assert_eq!(
            classify_accelerator(&r),
            AcceleratorClass::GpuSmall,
            "unknown VRAM must never be rounded up to gpu_large"
        );
    }

    #[test]
    fn two_discrete_gpus_are_gpu_multi() {
        let mut r = report_workstation();
        r.accelerators.push(gpu("nvidia", 24 * GIB_T));
        assert_eq!(classify_accelerator(&r), AcceleratorClass::GpuMulti);
    }

    #[test]
    fn npu_only_is_ranked_below_igpu() {
        assert!(AcceleratorClass::NpuSmall < AcceleratorClass::Igpu);
        let mut r = report_pi5_16gb();
        r.accelerators = vec![otwono_hal::AcceleratorInfo {
            kind: AcceleratorKind::Npu,
            vendor: "rockchip".into(),
            name: None,
            driver: Some("rknpu".into()),
            discrete: false,
            vram_bytes: None,
            compute_apis: vec!["rknn".into()],
            source: "test".into(),
        }];
        assert_eq!(classify_accelerator(&r), AcceleratorClass::NpuSmall);
    }

    #[test]
    fn a_display_only_framebuffer_yields_no_accelerator() {
        // Regression: a QEMU amd64 guest reports a `simple-framebuffer` DRM card. It has no
        // compute API, so the accelerator axis must be `none`, not `igpu`.
        let mut r = report_pi5_16gb();
        r.accelerators = vec![otwono_hal::AcceleratorInfo {
            kind: AcceleratorKind::Gpu,
            vendor: "unknown".into(),
            name: None,
            driver: Some("simple-framebuffer".into()),
            discrete: false,
            vram_bytes: None,
            compute_apis: vec![],
            source: "test".into(),
        }];
        assert_eq!(classify_accelerator(&r), AcceleratorClass::None);
    }

    #[test]
    fn rotational_disks_never_reach_fast() {
        let mut r = report_workstation();
        r.storage.rotational_only = true;
        assert_eq!(classify_storage(&r), StorageClass::Standard);
    }

    #[test]
    fn tiny_free_space_is_constrained_regardless_of_disk_size() {
        let mut r = report_workstation();
        r.storage.data_free_bytes = 4 * GIB_T;
        assert_eq!(classify_storage(&r), StorageClass::Constrained);
    }

    #[test]
    fn no_link_is_offline() {
        let r = report_pi4_4gb();
        assert_eq!(classify_network(&r), NetworkClass::Offline);
    }

    #[test]
    fn battery_on_mains_is_managed_not_unconstrained() {
        let mut r = report_workstation();
        r.power.has_battery = true;
        r.power.on_battery = false;
        assert_eq!(classify_power(&r), PowerClass::Managed);
        r.power.on_battery = true;
        assert_eq!(classify_power(&r), PowerClass::Constrained);
    }

    const GIB_T: u64 = 1024 * 1024 * 1024;
}

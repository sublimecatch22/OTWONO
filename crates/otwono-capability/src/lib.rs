//! Capability classification.
//!
//! One component decides what this machine can do; everything else reads its answer.
//! See `docs/hardware/CAPABILITY-TIERS.md` and ADR-0004.
//!
//! The design point worth defending: this is a **vector**, not a score. A Pi 5 with 16 GB
//! and no GPU, and a 4-core laptop with an 8 GB RTX 4060, are not comparable on one axis,
//! and a single number hides exactly the bottleneck that will break the system. The
//! overall tier is composed as the highest tier whose every requirement is met, so the
//! weakest binding axis wins — and the profile reports *which* axis that was.

#![forbid(unsafe_code)]

pub mod axes;
pub mod features;
pub mod overrides;

pub use axes::{
    AcceleratorClass, CapabilityAxes, ComputeClass, MemoryClass, NetworkClass, PowerClass, StorageClass,
};
pub use features::{DesktopProfile, FeatureGates};
pub use overrides::CapabilityOverrides;

use otwono_hal::HardwareReport;
use serde::{Deserialize, Serialize};

/// Schema version of [`CapabilityProfile`]. Bump on any breaking change.
pub const CAPABILITY_PROFILE_SCHEMA_VERSION: &str = "2.0.0";

/// Overall hardware capability tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Tier {
    #[serde(rename = "T0_MICRO")]
    T0Micro,
    #[serde(rename = "T1_EDGE")]
    T1Edge,
    #[serde(rename = "T2_BALANCED")]
    T2Balanced,
    #[serde(rename = "T3_CAPABLE")]
    T3Capable,
    #[serde(rename = "T4_WORKSTATION")]
    T4Workstation,
}

impl Tier {
    pub const ALL: [Tier; 5] = [
        Tier::T0Micro,
        Tier::T1Edge,
        Tier::T2Balanced,
        Tier::T3Capable,
        Tier::T4Workstation,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::T0Micro => "T0_MICRO",
            Tier::T1Edge => "T1_EDGE",
            Tier::T2Balanced => "T2_BALANCED",
            Tier::T3Capable => "T3_CAPABLE",
            Tier::T4Workstation => "T4_WORKSTATION",
        }
    }

    pub fn parse(s: &str) -> Option<Tier> {
        Tier::ALL.into_iter().find(|t| t.as_str().eq_ignore_ascii_case(s))
    }
}

/// The requirement set a machine must satisfy to reach a tier.
#[derive(Debug, Clone, Copy)]
struct TierRequirements {
    memory: MemoryClass,
    compute: ComputeClass,
    accelerator: AcceleratorClass,
    storage: StorageClass,
}

impl TierRequirements {
    fn for_tier(tier: Tier) -> Self {
        match tier {
            Tier::T0Micro => TierRequirements {
                memory: MemoryClass::Minimal,
                compute: ComputeClass::Minimal,
                accelerator: AcceleratorClass::None,
                storage: StorageClass::Constrained,
            },
            Tier::T1Edge => TierRequirements {
                memory: MemoryClass::Low,
                compute: ComputeClass::Low,
                accelerator: AcceleratorClass::None,
                storage: StorageClass::Standard,
            },
            Tier::T2Balanced => TierRequirements {
                memory: MemoryClass::Medium,
                compute: ComputeClass::Medium,
                accelerator: AcceleratorClass::None,
                storage: StorageClass::Standard,
            },
            Tier::T3Capable => TierRequirements {
                memory: MemoryClass::High,
                compute: ComputeClass::Medium,
                accelerator: AcceleratorClass::GpuSmall,
                storage: StorageClass::Fast,
            },
            Tier::T4Workstation => TierRequirements {
                memory: MemoryClass::Extreme,
                compute: ComputeClass::High,
                accelerator: AcceleratorClass::GpuLarge,
                storage: StorageClass::Fast,
            },
        }
    }

    /// Which axis, if any, prevents this tier? Returns the first unmet requirement.
    fn unmet_by(&self, axes: &CapabilityAxes) -> Option<String> {
        if axes.memory < self.memory {
            return Some(format!("memory ({:?} < {:?})", axes.memory, self.memory));
        }
        if axes.compute < self.compute {
            return Some(format!("compute ({:?} < {:?})", axes.compute, self.compute));
        }
        if axes.accelerator < self.accelerator {
            return Some(format!(
                "accelerator ({:?} < {:?})",
                axes.accelerator, self.accelerator
            ));
        }
        if axes.storage < self.storage {
            return Some(format!("storage ({:?} < {:?})", axes.storage, self.storage));
        }
        None
    }
}

/// The machine-readable contract every other OTWONO subsystem consumes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CapabilityProfile {
    pub schema_version: String,
    pub tier: Tier,
    /// Why this tier and not the next one up. `None` at the top tier.
    pub limiting_factor: Option<String>,
    pub axes: CapabilityAxes,
    pub features: FeatureGates,
    /// True when an operator override changed the tier or an axis.
    pub overridden: bool,
    /// What was detected before any override, present only when `overridden`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected: Option<Box<DetectedSummary>>,
    pub hardware: HardwareReport,
    pub warnings: Vec<String>,
}

/// The pre-override classification, kept so a bug report shows both what was found and
/// what the operator forced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DetectedSummary {
    pub tier: Tier,
    pub axes: CapabilityAxes,
}

/// Classify a hardware report into a capability profile.
pub fn classify(report: &HardwareReport) -> CapabilityProfile {
    classify_with_overrides(report, &CapabilityOverrides::default())
}

/// Classify with operator overrides applied.
pub fn classify_with_overrides(report: &HardwareReport, ov: &CapabilityOverrides) -> CapabilityProfile {
    let detected_axes = CapabilityAxes::from_report(report);
    let (detected_tier, _) = compose_tier(&detected_axes);

    let axes = ov.apply_to_axes(detected_axes.clone());
    let (mut tier, mut limiting) = compose_tier(&axes);

    let mut warnings = report.warnings.clone();

    if let Some(forced) = ov.tier {
        if forced > tier {
            warnings.push(format!(
                "capability: tier forced up to {} from detected {}; the hardware may not sustain it",
                forced.as_str(),
                tier.as_str()
            ));
        }
        tier = forced;
        limiting = Some("operator override".to_string());
    }

    let overridden = ov.changes_anything();
    let features = FeatureGates::for_tier(tier, &axes);

    CapabilityProfile {
        schema_version: CAPABILITY_PROFILE_SCHEMA_VERSION.to_string(),
        tier,
        limiting_factor: limiting,
        axes,
        features,
        overridden,
        detected: overridden.then(|| {
            Box::new(DetectedSummary {
                tier: detected_tier,
                axes: detected_axes,
            })
        }),
        hardware: report.clone(),
        warnings,
    }
}

/// The highest tier whose every requirement is met, plus the axis blocking the next one.
fn compose_tier(axes: &CapabilityAxes) -> (Tier, Option<String>) {
    let mut best = Tier::T0Micro;
    let mut blocker = None;

    for tier in Tier::ALL.into_iter().skip(1) {
        match TierRequirements::for_tier(tier).unmet_by(axes) {
            None => best = tier,
            Some(reason) => {
                blocker = Some(reason);
                break;
            }
        }
    }
    (best, blocker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;

    #[test]
    fn tier_names_round_trip() {
        for t in Tier::ALL {
            assert_eq!(Tier::parse(t.as_str()), Some(t));
        }
        assert_eq!(Tier::parse("t2_balanced"), Some(Tier::T2Balanced));
        assert_eq!(Tier::parse("nonsense"), None);
    }

    #[test]
    fn tiers_are_ordered() {
        assert!(Tier::T0Micro < Tier::T1Edge);
        assert!(Tier::T3Capable < Tier::T4Workstation);
    }

    #[test]
    fn weakest_axis_binds() {
        // Everything is workstation-class except memory, which is merely `high`.
        let axes = CapabilityAxes {
            compute: ComputeClass::Extreme,
            memory: MemoryClass::High,
            accelerator: AcceleratorClass::GpuLarge,
            storage: StorageClass::Bulk,
            network: NetworkClass::Broadband,
            power: PowerClass::Unconstrained,
        };
        let (tier, why) = compose_tier(&axes);
        assert_eq!(tier, Tier::T3Capable);
        assert!(
            why.unwrap().starts_with("memory"),
            "the limiting axis must be named"
        );
    }

    #[test]
    fn top_tier_has_no_limiting_factor() {
        let axes = CapabilityAxes {
            compute: ComputeClass::Extreme,
            memory: MemoryClass::Extreme,
            accelerator: AcceleratorClass::GpuMulti,
            storage: StorageClass::Bulk,
            network: NetworkClass::Gateway,
            power: PowerClass::Unconstrained,
        };
        let (tier, why) = compose_tier(&axes);
        assert_eq!(tier, Tier::T4Workstation);
        assert_eq!(why, None);
    }

    #[test]
    fn pi_zero_class_hardware_is_t0() {
        let profile = classify(&report_pi_zero());
        assert_eq!(profile.tier, Tier::T0Micro);
        assert!(!profile.features.local_llm, "T0 must not promise a local LLM");
    }

    #[test]
    fn pi4_4gb_is_t1() {
        assert_eq!(classify(&report_pi4_4gb()).tier, Tier::T1Edge);
    }

    #[test]
    fn pi5_16gb_without_gpu_is_t2_limited_by_accelerator() {
        let profile = classify(&report_pi5_16gb());
        assert_eq!(profile.tier, Tier::T2Balanced);
        assert_eq!(profile.axes.accelerator, AcceleratorClass::None);
        assert!(
            profile
                .limiting_factor
                .as_deref()
                .unwrap()
                .starts_with("accelerator"),
            "plenty of RAM but no GPU should be reported as an accelerator limit, got {:?}",
            profile.limiting_factor
        );
    }

    #[test]
    fn gpu_workstation_is_t4() {
        assert_eq!(classify(&report_workstation()).tier, Tier::T4Workstation);
    }

    #[test]
    fn a_big_gpu_does_not_rescue_a_tiny_machine() {
        // 2 GiB of RAM with a 24 GiB GPU is still not a workstation: you cannot load the
        // model to hand it to the GPU.
        let mut r = report_pi4_4gb();
        r.memory.total_bytes = 2 * GIB;
        r.memory.available_bytes = GIB;
        r.accelerators = vec![gpu("nvidia", 24 * GIB)];
        let profile = classify(&r);
        assert!(profile.tier <= Tier::T1Edge, "got {:?}", profile.tier);
    }

    #[test]
    fn empty_hardware_degrades_to_t0_without_panicking() {
        let report = otwono_hal::SystemProbe::from_root("/nonexistent").probe();
        let profile = classify(&report);
        assert_eq!(profile.tier, Tier::T0Micro);
        assert!(!profile.warnings.is_empty());
    }

    #[test]
    fn forcing_a_tier_up_warns_and_records_what_was_detected() {
        let ov = CapabilityOverrides {
            tier: Some(Tier::T4Workstation),
            ..Default::default()
        };
        let profile = classify_with_overrides(&report_pi4_4gb(), &ov);
        assert_eq!(profile.tier, Tier::T4Workstation);
        assert!(profile.overridden);
        assert_eq!(profile.detected.as_ref().unwrap().tier, Tier::T1Edge);
        assert!(
            profile.warnings.iter().any(|w| w.contains("forced up")),
            "forcing a tier up must warn: {:?}",
            profile.warnings
        );
    }

    #[test]
    fn forcing_a_tier_down_does_not_warn() {
        let ov = CapabilityOverrides {
            tier: Some(Tier::T0Micro),
            ..Default::default()
        };
        let profile = classify_with_overrides(&report_workstation(), &ov);
        assert_eq!(profile.tier, Tier::T0Micro);
        assert!(!profile.warnings.iter().any(|w| w.contains("forced up")));
    }

    #[test]
    fn profile_round_trips_through_json() {
        let profile = classify(&report_pi5_16gb());
        let json = serde_json::to_string(&profile).unwrap();
        let back: CapabilityProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(profile, back);
    }
}

/// Hardware fixtures other crates test against.
///
/// Public behind a feature rather than `#[cfg(test)]` because the machines these describe
/// — a Pi Zero, a Pi 4, a Pi 5, a GPU workstation — are the cases every tier-aware
/// subsystem has to get right, and each of them re-deriving its own idea of "a small
/// board" is how the tiering stops being one contract (CLAUDE.md §2.6).
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use otwono_hal::*;

    pub const GIB: u64 = 1024 * 1024 * 1024;

    pub fn gpu(vendor: &str, vram: u64) -> AcceleratorInfo {
        AcceleratorInfo {
            kind: AcceleratorKind::Gpu,
            vendor: vendor.to_string(),
            name: None,
            driver: Some(vendor.to_string()),
            discrete: true,
            vram_bytes: Some(vram),
            compute_apis: vec!["vulkan".into()],
            source: "test".into(),
        }
    }

    fn base(arch: &str) -> HardwareReport {
        HardwareReport {
            schema_version: HARDWARE_REPORT_SCHEMA_VERSION.to_string(),
            machine: MachineInfo {
                model: None,
                chassis: None,
                architecture: arch.to_string(),
            },
            cpu: CpuInfo {
                architecture: arch.to_string(),
                ..Default::default()
            },
            memory: MemoryInfo::default(),
            accelerators: Vec::new(),
            storage: StorageInfo::default(),
            network: NetworkInfo::default(),
            power: PowerInfo::default(),
            warnings: Vec::new(),
        }
    }

    fn with_storage(mut r: HardwareReport, free: u64, rotational: bool) -> HardwareReport {
        r.storage = StorageInfo {
            devices: vec![BlockDevice {
                name: "test0".into(),
                size_bytes: free * 2,
                rotational,
                removable: false,
            }],
            data_path: "/var/lib/otwono".into(),
            data_total_bytes: free * 2,
            data_free_bytes: free,
            rotational_only: rotational,
        };
        r
    }

    /// Raspberry Pi Zero 2 W class: 512 MiB, 4 slow cores, no modern vector ISA.
    pub fn report_pi_zero() -> HardwareReport {
        let mut r = base("aarch64");
        r.cpu.logical_cpus = 4;
        r.cpu.physical_cores = 4;
        r.cpu.features.neon = true; // asimd only, no dotprod
        r.memory = MemoryInfo {
            total_bytes: 512 * 1024 * 1024,
            available_bytes: 400 * 1024 * 1024,
            swap_total_bytes: 0,
        };
        with_storage(r, 8 * GIB, false)
    }

    /// Raspberry Pi 4, 4 GiB, Cortex-A72 (no dotprod).
    pub fn report_pi4_4gb() -> HardwareReport {
        let mut r = base("aarch64");
        r.cpu.logical_cpus = 4;
        r.cpu.physical_cores = 4;
        r.cpu.features.neon = true;
        r.memory = MemoryInfo {
            total_bytes: 4 * GIB,
            available_bytes: 3 * GIB,
            swap_total_bytes: 0,
        };
        with_storage(r, 50 * GIB, false)
    }

    /// Raspberry Pi 5, 16 GiB, Cortex-A76 (dotprod), no discrete GPU.
    pub fn report_pi5_16gb() -> HardwareReport {
        let mut r = base("aarch64");
        r.cpu.logical_cpus = 4;
        r.cpu.physical_cores = 4;
        r.cpu.features.neon = true;
        r.cpu.features.dotprod = true;
        r.memory = MemoryInfo {
            total_bytes: 16 * GIB,
            available_bytes: 14 * GIB,
            swap_total_bytes: 0,
        };
        with_storage(r, 400 * GIB, false)
    }

    /// 24-core x86 workstation with a 24 GiB GPU.
    pub fn report_workstation() -> HardwareReport {
        let mut r = base("x86_64");
        r.cpu.logical_cpus = 32;
        r.cpu.physical_cores = 24;
        r.cpu.features.avx2 = true;
        r.cpu.features.avx512f = true;
        r.memory = MemoryInfo {
            total_bytes: 64 * GIB,
            available_bytes: 58 * GIB,
            swap_total_bytes: 8 * GIB,
        };
        r.accelerators = vec![gpu("nvidia", 24 * GIB)];
        with_storage(r, 2000 * GIB, false)
    }
}

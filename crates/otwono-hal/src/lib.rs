//! OTWONO hardware abstraction layer.
//!
//! Every probe reads from an **injectable root path** rather than a hardcoded `/proc` or
//! `/sys`. In production the root is `/`; in tests it is a fixture directory captured from
//! real hardware. This is what makes hardware detection for a Raspberry Pi 5 or an RK3588
//! board testable from a CI runner that is neither.
//!
//! Probes never panic and never fail the whole report. A file that is missing or
//! unparseable produces a default plus a warning, because a partially-detected machine is
//! far more useful than an error.

#![forbid(unsafe_code)]

pub mod accelerator;
pub mod cpu;
pub mod memory;
pub mod network;
pub mod power;
pub mod storage;
mod sysfs;

pub use accelerator::{AcceleratorInfo, AcceleratorKind};
pub use cpu::{CpuFeatures, CpuInfo};
pub use memory::MemoryInfo;
pub use network::{NetworkInfo, NetworkInterface};
pub use power::PowerInfo;
pub use storage::{BlockDevice, StorageInfo};

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Schema version of [`HardwareReport`]. Bump on any breaking change.
pub const HARDWARE_REPORT_SCHEMA_VERSION: &str = "1.0.0";

/// Identifying information about the machine as a whole.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MachineInfo {
    /// Device-tree model string (SBCs) or DMI product name (x86). `None` if undetectable.
    pub model: Option<String>,
    /// DMI chassis type where available, e.g. `laptop`, `desktop`, `server`.
    pub chassis: Option<String>,
    /// Kernel-reported architecture, e.g. `x86_64`, `aarch64`.
    pub architecture: String,
}

/// A complete hardware probe result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HardwareReport {
    pub schema_version: String,
    pub machine: MachineInfo,
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    pub accelerators: Vec<AcceleratorInfo>,
    pub storage: StorageInfo,
    pub network: NetworkInfo,
    pub power: PowerInfo,
    /// Non-fatal problems encountered while probing. Empty is the happy path.
    pub warnings: Vec<String>,
}

/// Probes a system rooted at `root`.
///
/// ```no_run
/// use otwono_hal::SystemProbe;
/// let report = SystemProbe::system().probe();          // reads the real machine
/// let report = SystemProbe::from_root("tests/fixtures/rpi5-8gb").probe(); // reads a fixture
/// ```
#[derive(Debug, Clone)]
pub struct SystemProbe {
    root: PathBuf,
    /// True when probing the live system. Fixture probes must not touch real syscalls.
    live: bool,
}

impl SystemProbe {
    /// Probe the running system.
    pub fn system() -> Self {
        Self {
            root: PathBuf::from("/"),
            live: true,
        }
    }

    /// Probe a captured fixture tree.
    pub fn from_root(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let live = root == Path::new("/");
        Self { root, live }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    /// Resolve an absolute-looking system path against the probe root.
    pub(crate) fn path(&self, absolute: &str) -> PathBuf {
        self.root.join(absolute.trim_start_matches('/'))
    }

    /// Run every probe. Never panics; never returns `Err`.
    pub fn probe(&self) -> HardwareReport {
        let mut warnings = Vec::new();

        let cpu = cpu::probe(self, &mut warnings);
        let memory = memory::probe(self, &mut warnings);
        let accelerators = accelerator::probe(self, &mut warnings);
        let storage = storage::probe(self, &mut warnings);
        let network = network::probe(self, &mut warnings);
        let power = power::probe(self, &mut warnings);
        let machine = self.probe_machine(&cpu);

        HardwareReport {
            schema_version: HARDWARE_REPORT_SCHEMA_VERSION.to_string(),
            machine,
            cpu,
            memory,
            accelerators,
            storage,
            network,
            power,
            warnings,
        }
    }

    fn probe_machine(&self, cpu: &CpuInfo) -> MachineInfo {
        // Device tree first: it is the authoritative model string on every SBC we care
        // about, and it is absent on x86 where DMI takes over.
        let model = sysfs::read_trimmed(&self.path("/proc/device-tree/model"))
            .or_else(|| sysfs::read_trimmed(&self.path("/sys/firmware/devicetree/base/model")))
            .or_else(|| sysfs::read_trimmed(&self.path("/sys/class/dmi/id/product_name")))
            .filter(|s| !s.is_empty());

        let chassis = sysfs::read_trimmed(&self.path("/sys/class/dmi/id/chassis_type"))
            .and_then(|v| v.parse::<u32>().ok())
            .map(dmi_chassis_name)
            .map(str::to_string);

        MachineInfo {
            model,
            chassis,
            architecture: cpu.architecture.clone(),
        }
    }
}

/// DMI chassis type codes (SMBIOS 3.x, table 7.4.1). Only the ones we act on are named.
fn dmi_chassis_name(code: u32) -> &'static str {
    match code {
        3 | 4 | 6 | 7 | 15 | 16 | 24 => "desktop",
        8 | 9 | 10 | 11 | 14 | 31 | 32 => "laptop",
        17 | 23 | 25 | 28 => "server",
        30 => "tablet",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_joins_against_root() {
        let p = SystemProbe::from_root("/tmp/fixture");
        assert_eq!(p.path("/proc/cpuinfo"), Path::new("/tmp/fixture/proc/cpuinfo"));
    }

    #[test]
    fn system_root_is_live() {
        assert!(SystemProbe::system().is_live());
        assert!(!SystemProbe::from_root("/tmp/fixture").is_live());
    }

    #[test]
    fn missing_tree_yields_a_report_not_a_panic() {
        let report = SystemProbe::from_root("/nonexistent-probe-root").probe();
        assert_eq!(report.cpu.logical_cpus, 0);
        assert!(!report.warnings.is_empty(), "a missing tree must warn");
    }

    #[test]
    fn chassis_codes_map() {
        assert_eq!(dmi_chassis_name(3), "desktop");
        assert_eq!(dmi_chassis_name(10), "laptop");
        assert_eq!(dmi_chassis_name(23), "server");
        assert_eq!(dmi_chassis_name(999), "unknown");
    }
}

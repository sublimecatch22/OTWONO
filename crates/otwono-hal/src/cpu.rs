//! CPU probing.
//!
//! Sources: `/proc/cpuinfo`, `/sys/devices/system/cpu/`, `/proc/device-tree/model`.
//!
//! ISA feature flags matter more than core count for local inference. On arm64,
//! `asimddp` (dot product) and `i8mm` roughly double quantized matmul throughput; on x86,
//! `avx2` is the floor and `avx512f`/`avx_vnni` are significant. The classifier reads these
//! rather than guessing from the model name.

use super::{sysfs, SystemProbe};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CpuFeatures {
    // x86_64
    pub avx: bool,
    pub avx2: bool,
    pub avx512f: bool,
    pub avx_vnni: bool,
    pub f16c: bool,
    pub amx: bool,
    // aarch64
    pub neon: bool,
    pub sve: bool,
    pub dotprod: bool,
    pub i8mm: bool,
    pub bf16: bool,
}

impl CpuFeatures {
    /// Does this CPU have vector support modern enough to make quantized inference
    /// worthwhile? Used by the compute classifier.
    pub fn has_modern_vector_isa(&self) -> bool {
        self.avx2 || self.avx512f || self.dotprod || self.i8mm || self.sve
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CpuInfo {
    pub architecture: String,
    pub logical_cpus: u32,
    pub physical_cores: u32,
    pub vendor: Option<String>,
    pub model_name: Option<String>,
    pub max_frequency_mhz: Option<u32>,
    /// True when core `max_freq` values differ, i.e. big.LITTLE or P/E cores.
    pub heterogeneous: bool,
    pub features: CpuFeatures,
    /// The raw flag set, sorted. Kept so a future classifier can use a flag we do not
    /// parse today without needing a new fixture capture.
    pub flags: Vec<String>,
}

pub(crate) fn probe(p: &SystemProbe, warnings: &mut Vec<String>) -> CpuInfo {
    let mut info = CpuInfo {
        architecture: detect_arch(p),
        ..Default::default()
    };

    let cpuinfo_path = p.path("/proc/cpuinfo");
    match sysfs::read_trimmed(&cpuinfo_path) {
        Some(text) if !text.is_empty() => parse_cpuinfo(&text, &mut info),
        _ => warnings.push(format!("cpu: cannot read {}", cpuinfo_path.display())),
    }

    // Logical CPU count from sysfs is more reliable than counting `processor:` lines,
    // because /proc/cpuinfo hides offline CPUs on some kernels.
    let online = count_online_cpus(p);
    if online > 0 {
        info.logical_cpus = online;
    }

    if info.physical_cores == 0 {
        info.physical_cores = count_physical_cores_from_topology(p).unwrap_or(info.logical_cpus);
    }

    let (max_mhz, hetero) = probe_frequencies(p);
    info.max_frequency_mhz = max_mhz;
    info.heterogeneous = hetero;

    if info.logical_cpus == 0 {
        warnings.push("cpu: detected zero logical CPUs".to_string());
    }

    info
}

fn detect_arch(p: &SystemProbe) -> String {
    // For a live probe the compile-time target is authoritative. For a fixture we infer
    // from the flag vocabulary, which is what distinguishes the trees in practice.
    if p.is_live() {
        return std::env::consts::ARCH.to_string();
    }
    let text = sysfs::read_trimmed(&p.path("/proc/cpuinfo")).unwrap_or_default();
    if text.contains("CPU implementer") || text.contains("\nFeatures\t") || text.starts_with("Features") {
        "aarch64".to_string()
    } else if text.contains("vendor_id") || text.contains("\nflags\t") {
        "x86_64".to_string()
    } else {
        "unknown".to_string()
    }
}

fn parse_cpuinfo(text: &str, info: &mut CpuInfo) {
    let mut processors = 0u32;
    let mut core_ids: BTreeSet<(String, String)> = BTreeSet::new();
    let mut current_physical_id: Option<String> = None;
    let mut current_core_id: Option<String> = None;

    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            // A blank line ends a processor block on both architectures.
            if line.trim().is_empty() {
                if let (Some(p), Some(c)) = (current_physical_id.take(), current_core_id.take()) {
                    core_ids.insert((p, c));
                }
            }
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        match key {
            "processor" => processors += 1,
            "vendor_id" | "CPU implementer" => {
                if info.vendor.is_none() {
                    info.vendor = Some(normalize_vendor(value));
                }
            }
            "model name" | "Model" | "Hardware" => {
                if info.model_name.is_none() && !value.is_empty() {
                    info.model_name = Some(value.to_string());
                }
            }
            // x86 gives the per-socket core count directly.
            "cpu cores" => {
                if let Ok(n) = value.parse::<u32>() {
                    if n > info.physical_cores {
                        info.physical_cores = n;
                    }
                }
            }
            "physical id" => current_physical_id = Some(value.to_string()),
            "core id" => current_core_id = Some(value.to_string()),
            // Guarded rather than nested: only the first processor block's flag list is
            // read. Every core reports the same set, and on big.LITTLE the first block is
            // the one the scheduler starts threads on.
            "flags" | "Features" if info.flags.is_empty() => {
                let mut flags: Vec<String> = value.split_whitespace().map(str::to_string).collect();
                flags.sort();
                info.features = parse_features(&flags);
                info.flags = flags;
            }
            _ => {}
        }
    }

    if let (Some(p), Some(c)) = (current_physical_id, current_core_id) {
        core_ids.insert((p, c));
    }

    info.logical_cpus = processors;

    // On multi-socket x86, `cpu cores` is per socket; the (physical id, core id) set is
    // the whole machine. Prefer whichever is larger.
    let topology_cores = core_ids.len() as u32;
    if topology_cores > info.physical_cores {
        info.physical_cores = topology_cores;
    }
}

fn normalize_vendor(raw: &str) -> String {
    match raw {
        "GenuineIntel" => "intel".to_string(),
        "AuthenticAMD" => "amd".to_string(),
        // ARM `CPU implementer` codes, JEP106.
        "0x41" => "arm".to_string(),
        "0x42" => "broadcom".to_string(),
        "0x51" => "qualcomm".to_string(),
        "0x61" => "apple".to_string(),
        "0xc0" => "ampere".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

fn parse_features(flags: &[String]) -> CpuFeatures {
    let has = |name: &str| flags.iter().any(|f| f == name);
    CpuFeatures {
        avx: has("avx"),
        avx2: has("avx2"),
        avx512f: has("avx512f"),
        avx_vnni: has("avx_vnni") || has("avx512_vnni"),
        f16c: has("f16c"),
        amx: has("amx_tile") || has("amx_int8") || has("amx_bf16"),
        neon: has("neon") || has("asimd"),
        sve: has("sve") || has("sve2"),
        dotprod: has("asimddp"),
        i8mm: has("i8mm"),
        bf16: has("bf16") || has("asimdbf16"),
    }
}

fn count_online_cpus(p: &SystemProbe) -> u32 {
    // `/sys/devices/system/cpu/online` is a range list like "0-3" or "0-1,4-7".
    if let Some(spec) = sysfs::read_trimmed(&p.path("/sys/devices/system/cpu/online")) {
        let n = parse_cpu_range(&spec);
        if n > 0 {
            return n;
        }
    }
    sysfs::list_dir(&p.path("/sys/devices/system/cpu"))
        .iter()
        .filter(|n| n.starts_with("cpu") && n[3..].chars().all(|c| c.is_ascii_digit()) && n.len() > 3)
        .count() as u32
}

/// Parse a kernel CPU range list, e.g. `"0-3"`, `"0,2,4"`, `"0-1,4-7"`.
fn parse_cpu_range(spec: &str) -> u32 {
    let mut count = 0u32;
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                    if b >= a {
                        count += b - a + 1;
                    }
                }
            }
            None => {
                if part.parse::<u32>().is_ok() {
                    count += 1;
                }
            }
        }
    }
    count
}

fn count_physical_cores_from_topology(p: &SystemProbe) -> Option<u32> {
    let base = p.path("/sys/devices/system/cpu");
    let mut pairs: BTreeSet<(u64, u64)> = BTreeSet::new();
    for name in sysfs::list_dir(&base) {
        if !(name.starts_with("cpu") && name.len() > 3 && name[3..].chars().all(|c| c.is_ascii_digit())) {
            continue;
        }
        let topo = base.join(&name).join("topology");
        let pkg = sysfs::read_u64(&topo.join("physical_package_id")).unwrap_or(0);
        if let Some(core) = sysfs::read_u64(&topo.join("core_id")) {
            pairs.insert((pkg, core));
        }
    }
    (!pairs.is_empty()).then_some(pairs.len() as u32)
}

/// Returns (max frequency in MHz, whether cores have differing max frequencies).
fn probe_frequencies(p: &SystemProbe) -> (Option<u32>, bool) {
    let base = p.path("/sys/devices/system/cpu");
    let mut freqs: Vec<u64> = Vec::new();
    for name in sysfs::list_dir(&base) {
        if !name.starts_with("cpu") {
            continue;
        }
        if let Some(khz) = sysfs::read_u64(&base.join(&name).join("cpufreq/cpuinfo_max_freq")) {
            freqs.push(khz);
        }
    }
    if freqs.is_empty() {
        return (None, false);
    }
    let max = *freqs.iter().max().unwrap_or(&0);
    let min = *freqs.iter().min().unwrap_or(&0);
    (Some((max / 1000) as u32), max != min)
}

#[cfg(test)]
mod tests {
    use super::*;

    const X86: &str = "\
processor\t: 0
vendor_id\t: GenuineIntel
model name\t: Intel(R) Xeon(R) Processor @ 2.80GHz
cpu cores\t: 4
physical id\t: 0
core id\t: 0
flags\t\t: fpu avx avx2 f16c avx512f avx512_vnni sse4_2

processor\t: 1
vendor_id\t: GenuineIntel
model name\t: Intel(R) Xeon(R) Processor @ 2.80GHz
cpu cores\t: 4
physical id\t: 0
core id\t: 1
flags\t\t: fpu avx avx2 f16c avx512f avx512_vnni sse4_2
";

    const ARM64: &str = "\
processor\t: 0
BogoMIPS\t: 108.00
Features\t: fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm lrcpc dcpop asimddp
CPU implementer\t: 0x41
CPU architecture: 8
CPU variant\t: 0x4
CPU part\t: 0xd0b

processor\t: 1
Features\t: fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm lrcpc dcpop asimddp
CPU implementer\t: 0x41
CPU part\t: 0xd0b
";

    #[test]
    fn parses_x86_cpuinfo() {
        let mut info = CpuInfo::default();
        parse_cpuinfo(X86, &mut info);
        assert_eq!(info.logical_cpus, 2);
        assert_eq!(
            info.physical_cores, 4,
            "`cpu cores` reports the socket's core count"
        );
        assert_eq!(info.vendor.as_deref(), Some("intel"));
        assert!(info.features.avx2 && info.features.avx512f && info.features.avx_vnni);
        assert!(!info.features.neon);
        assert!(info.features.has_modern_vector_isa());
    }

    #[test]
    fn parses_arm64_cpuinfo() {
        let mut info = CpuInfo::default();
        parse_cpuinfo(ARM64, &mut info);
        assert_eq!(info.logical_cpus, 2);
        assert_eq!(info.vendor.as_deref(), Some("arm"));
        assert!(info.features.neon, "asimd implies neon");
        assert!(info.features.dotprod, "asimddp must be detected");
        assert!(!info.features.i8mm);
        assert!(!info.features.avx2);
        assert!(info.features.has_modern_vector_isa());
    }

    #[test]
    fn old_arm_without_dotprod_is_not_modern() {
        let mut info = CpuInfo::default();
        parse_cpuinfo(
            "processor\t: 0\nFeatures\t: fp asimd evtstrm crc32\nCPU implementer\t: 0x41\n",
            &mut info,
        );
        assert!(info.features.neon);
        assert!(
            !info.features.has_modern_vector_isa(),
            "asimd alone is not a modern vector ISA for inference"
        );
    }

    #[test]
    fn multi_socket_uses_topology_pairs() {
        let text = "\
processor\t: 0
cpu cores\t: 2
physical id\t: 0
core id\t: 0

processor\t: 1
cpu cores\t: 2
physical id\t: 0
core id\t: 1

processor\t: 2
cpu cores\t: 2
physical id\t: 1
core id\t: 0

processor\t: 3
cpu cores\t: 2
physical id\t: 1
core id\t: 1
";
        let mut info = CpuInfo::default();
        parse_cpuinfo(text, &mut info);
        assert_eq!(info.logical_cpus, 4);
        assert_eq!(
            info.physical_cores, 4,
            "two sockets of two cores is four cores, not two"
        );
    }

    #[test]
    fn parses_cpu_ranges() {
        assert_eq!(parse_cpu_range("0-3"), 4);
        assert_eq!(parse_cpu_range("0"), 1);
        assert_eq!(parse_cpu_range("0-1,4-7"), 6);
        assert_eq!(parse_cpu_range("0,2,4"), 3);
        assert_eq!(parse_cpu_range(""), 0);
        assert_eq!(parse_cpu_range("garbage"), 0);
    }

    #[test]
    fn vendor_codes_normalize() {
        assert_eq!(normalize_vendor("GenuineIntel"), "intel");
        assert_eq!(normalize_vendor("AuthenticAMD"), "amd");
        assert_eq!(normalize_vendor("0x41"), "arm");
        assert_eq!(normalize_vendor("0x42"), "broadcom");
    }
}

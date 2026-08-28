//! Memory probing from `/proc/meminfo`.

use super::{sysfs, SystemProbe};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    /// Kernel's own estimate of what is actually usable without swapping.
    pub available_bytes: u64,
    pub swap_total_bytes: u64,
}

impl MemoryInfo {
    pub fn total_gib(&self) -> f64 {
        self.total_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    }
}

pub(crate) fn probe(p: &SystemProbe, warnings: &mut Vec<String>) -> MemoryInfo {
    let path = p.path("/proc/meminfo");
    let Some(text) = sysfs::read_trimmed(&path) else {
        warnings.push(format!("memory: cannot read {}", path.display()));
        return MemoryInfo::default();
    };
    let info = parse_meminfo(&text);
    if info.total_bytes == 0 {
        warnings.push("memory: MemTotal missing or zero".to_string());
    }
    info
}

fn parse_meminfo(text: &str) -> MemoryInfo {
    let mut info = MemoryInfo::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(value) = parse_kb_value(rest) else {
            continue;
        };
        match key.trim() {
            "MemTotal" => info.total_bytes = value,
            "MemAvailable" => info.available_bytes = value,
            "SwapTotal" => info.swap_total_bytes = value,
            _ => {}
        }
    }
    // Very old kernels have no MemAvailable. Falling back to MemTotal would be a lie that
    // makes admission control approve loads that will OOM, so fall back to MemFree.
    if info.available_bytes == 0 {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("MemFree:") {
                if let Some(v) = parse_kb_value(rest) {
                    info.available_bytes = v;
                }
            }
        }
    }
    info
}

/// `/proc/meminfo` values are in kibibytes with a `kB` suffix (which actually means KiB).
fn parse_kb_value(rest: &str) -> Option<u64> {
    let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
    kb.checked_mul(1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str = "\
MemTotal:       16324608 kB
MemFree:        11500000 kB
MemAvailable:   14800000 kB
Buffers:          100000 kB
SwapTotal:             0 kB
SwapFree:              0 kB
";

    #[test]
    fn parses_meminfo() {
        let m = parse_meminfo(MEMINFO);
        assert_eq!(m.total_bytes, 16_324_608 * 1024);
        assert_eq!(m.available_bytes, 14_800_000 * 1024);
        assert_eq!(m.swap_total_bytes, 0);
        assert!((m.total_gib() - 15.567).abs() < 0.01);
    }

    #[test]
    fn falls_back_to_memfree_when_memavailable_absent() {
        let m = parse_meminfo("MemTotal:  1000000 kB\nMemFree:   400000 kB\n");
        assert_eq!(
            m.available_bytes,
            400_000 * 1024,
            "must not fall back to MemTotal"
        );
    }

    #[test]
    fn tolerates_garbage() {
        let m = parse_meminfo("this is not meminfo\nMemTotal: banana kB\n");
        assert_eq!(m.total_bytes, 0);
    }
}

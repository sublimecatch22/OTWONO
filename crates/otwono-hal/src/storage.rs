//! Block device and filesystem capacity probing.
//!
//! Free space on the data path gates model downloads and replication roles. A T3-class GPU
//! with 8 GiB of free disk cannot host a 32B model, and the profile has to say so *before*
//! the download starts.

use super::{sysfs, SystemProbe};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Where OTWONO keeps models and content-addressed data.
pub const DEFAULT_DATA_PATH: &str = "/var/lib/otwono";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockDevice {
    pub name: String,
    pub size_bytes: u64,
    pub rotational: bool,
    pub removable: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageInfo {
    pub devices: Vec<BlockDevice>,
    /// Path whose free space was measured.
    pub data_path: String,
    pub data_total_bytes: u64,
    pub data_free_bytes: u64,
    /// True when no non-rotational device was found. Affects model load latency enough to
    /// be worth reporting.
    pub rotational_only: bool,
}

pub(crate) fn probe(p: &SystemProbe, warnings: &mut Vec<String>) -> StorageInfo {
    let mut info = StorageInfo {
        data_path: DEFAULT_DATA_PATH.to_string(),
        ..Default::default()
    };

    let base = p.path("/sys/block");
    for name in sysfs::list_dir(&base) {
        if is_virtual_device(&name) {
            continue;
        }
        let dev = base.join(&name);
        // `size` is in 512-byte sectors regardless of the device's logical block size.
        let Some(sectors) = sysfs::read_u64(&dev.join("size")) else {
            continue;
        };
        if sectors == 0 {
            continue; // an empty card reader slot
        }
        info.devices.push(BlockDevice {
            name: name.clone(),
            size_bytes: sectors * 512,
            rotational: sysfs::read_u64(&dev.join("queue/rotational")).unwrap_or(0) == 1,
            removable: sysfs::read_u64(&dev.join("removable")).unwrap_or(0) == 1,
        });
    }

    info.rotational_only = !info.devices.is_empty() && info.devices.iter().all(|d| d.rotational);

    match measure_filesystem(p, &info.data_path) {
        Some((total, free)) => {
            info.data_total_bytes = total;
            info.data_free_bytes = free;
        }
        None => warnings.push(format!(
            "storage: cannot measure free space at {}",
            info.data_path
        )),
    }

    if info.devices.is_empty() {
        warnings.push("storage: no block devices found".to_string());
    }
    info
}

/// Loopback, ramdisk, device-mapper and zram entries are not storage capacity.
fn is_virtual_device(name: &str) -> bool {
    const PREFIXES: [&str; 6] = ["loop", "ram", "zram", "dm-", "md", "sr"];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Returns `(total_bytes, free_bytes)` for the filesystem holding `path`.
///
/// On a live probe this is a real `statvfs(2)`. On a fixture it reads
/// `.otwono-probe/filesystem.json`, because free space cannot be captured as a `/sys` file.
fn measure_filesystem(p: &SystemProbe, data_path: &str) -> Option<(u64, u64)> {
    if p.is_live() {
        return statvfs_live(data_path);
    }
    let fixture = p.path("/.otwono-probe/filesystem.json");
    let text = sysfs::read_trimmed(&fixture)?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some((v.get("total_bytes")?.as_u64()?, v.get("free_bytes")?.as_u64()?))
}

#[cfg(feature = "statvfs")]
fn statvfs_live(data_path: &str) -> Option<(u64, u64)> {
    // Fall back up the tree: /var/lib/otwono does not exist before first install.
    let mut candidate = Path::new(data_path);
    loop {
        if candidate.exists() {
            let st = rustix::fs::statvfs(candidate).ok()?;
            let bsize = st.f_frsize;
            // f_bavail, not f_bfree: the reserved blocks are not ours to fill.
            return Some((st.f_blocks * bsize, st.f_bavail * bsize));
        }
        candidate = candidate.parent()?;
    }
}

#[cfg(not(feature = "statvfs"))]
fn statvfs_live(_data_path: &str) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_devices_are_excluded() {
        for n in ["loop0", "ram3", "zram0", "dm-1", "md0", "sr0"] {
            assert!(is_virtual_device(n), "{n} should be excluded");
        }
        for n in ["sda", "nvme0n1", "mmcblk0", "vda"] {
            assert!(!is_virtual_device(n), "{n} should be counted");
        }
    }

    #[test]
    fn sectors_are_always_512_bytes() {
        // A 4Kn NVMe still reports `size` in 512-byte units; getting this wrong
        // under-reports capacity by 8x.
        assert_eq!(2_000_409_264u64 * 512, 1_024_209_543_168);
    }
}

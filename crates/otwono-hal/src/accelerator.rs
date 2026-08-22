//! GPU and NPU probing.
//!
//! Sources: `/sys/class/drm/`, `/sys/bus/pci/devices/`, `/sys/class/accel/`, device nodes
//! under `/dev`, and device-tree node names on SBCs.
//!
//! VRAM is the hard part. There is no portable interface, so we read what each driver
//! exposes and report `None` when we genuinely do not know. The classifier must never
//! round an unknown accelerator *upward* — an over-optimistic tier turns into an OOM kill
//! the first time the user asks a question.

use super::{sysfs, SystemProbe};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceleratorKind {
    Gpu,
    Npu,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceleratorInfo {
    pub kind: AcceleratorKind,
    /// Lowercase vendor slug: `nvidia`, `amd`, `intel`, `broadcom`, `arm`, `rockchip`, ...
    pub vendor: String,
    pub name: Option<String>,
    pub driver: Option<String>,
    /// Discrete card (own VRAM) versus integrated (shares system RAM).
    pub discrete: bool,
    /// Dedicated video memory in bytes. `None` means undetectable, not zero.
    pub vram_bytes: Option<u64>,
    /// Compute APIs plausibly usable, derived from driver and device-node presence.
    pub compute_apis: Vec<String>,
    /// Where this entry came from, so a surprising result can be traced.
    pub source: String,
}

pub(crate) fn probe(p: &SystemProbe, warnings: &mut Vec<String>) -> Vec<AcceleratorInfo> {
    let mut out = Vec::new();
    probe_drm(p, &mut out);
    probe_accel_class(p, &mut out);
    probe_device_tree_npu(p, &mut out);

    if out.is_empty() {
        warnings.push("accelerator: none detected; inference will be CPU-only".to_string());
    }
    out
}

/// `/sys/class/drm/cardN` — every GPU with a kernel DRM driver, discrete or integrated,
/// PCI or SoC.
fn probe_drm(p: &SystemProbe, out: &mut Vec<AcceleratorInfo>) {
    let drm = p.path("/sys/class/drm");
    for name in sysfs::list_dir(&drm) {
        if !is_card_node(&name) {
            continue;
        }
        let dev = drm.join(&name).join("device");
        let driver = sysfs::keyval(&dev.join("uevent"), "DRIVER");
        let vendor_id = sysfs::read_u64(&dev.join("vendor"));
        let device_id = sysfs::read_u64(&dev.join("device"));
        let vendor = pci_vendor_slug(vendor_id).unwrap_or_else(|| {
            driver
                .as_deref()
                .map(driver_vendor_slug)
                .unwrap_or("unknown")
                .to_string()
        });

        let vram_bytes = read_vram(p, &dev, driver.as_deref());
        let discrete = is_discrete(&vendor, driver.as_deref(), vram_bytes);

        out.push(AcceleratorInfo {
            kind: AcceleratorKind::Gpu,
            vendor,
            name: device_id.map(|d| format!("{:04x}:{:04x}", vendor_id.unwrap_or(0), d)),
            driver: driver.clone(),
            discrete,
            vram_bytes,
            compute_apis: compute_apis(p, driver.as_deref()),
            source: format!("/sys/class/drm/{name}"),
        });
    }
}

/// `cardN`, not `card0-HDMI-A-1` (those are connectors, not devices).
fn is_card_node(name: &str) -> bool {
    name.strip_prefix("card")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

fn read_vram(p: &SystemProbe, dev: &std::path::Path, driver: Option<&str>) -> Option<u64> {
    // amdgpu exposes this directly and accurately.
    if let Some(v) = sysfs::read_u64(&dev.join("mem_info_vram_total")) {
        return Some(v);
    }
    // Nouveau/other DRM drivers sometimes expose a memory info node.
    if let Some(v) = sysfs::read_u64(&dev.join("mem_info_vis_vram_total")) {
        return Some(v);
    }
    // The proprietary NVIDIA driver exposes it only through NVML/nvidia-smi, which we do
    // not shell out to from a probe. `otwono-hwd` will fill this in later via a helper.
    let _ = (p, driver);
    None
}

fn is_discrete(vendor: &str, driver: Option<&str>, vram: Option<u64>) -> bool {
    // A real VRAM reading is the strongest signal available.
    if let Some(v) = vram {
        return v >= 1024 * 1024 * 1024;
    }
    match driver {
        // Proprietary NVIDIA and nouveau are effectively always discrete on our targets.
        Some("nvidia") | Some("nouveau") => true,
        // SoC GPUs.
        Some("i915") | Some("xe") | Some("vc4") | Some("v3d") | Some("panfrost") | Some("panthor")
        | Some("lima") | Some("etnaviv") | Some("msm") | Some("komeda") | Some("meson") => false,
        _ => vendor == "nvidia",
    }
}

fn compute_apis(p: &SystemProbe, driver: Option<&str>) -> Vec<String> {
    let mut apis = Vec::new();
    match driver {
        Some("nvidia") => {
            if sysfs::exists(&p.path("/dev/nvidiactl")) || sysfs::exists(&p.path("/dev/nvidia0")) {
                apis.push("cuda".to_string());
            }
            apis.push("vulkan".to_string());
        }
        Some("amdgpu") => {
            if sysfs::exists(&p.path("/dev/kfd")) {
                apis.push("rocm".to_string());
            }
            apis.push("vulkan".to_string());
        }
        Some(_) => apis.push("vulkan".to_string()),
        None => {}
    }
    apis
}

/// `/sys/class/accel/` is the kernel's accelerator subsystem — Intel NPU (`intel_vpu`),
/// AMD XDNA (`amdxdna`), Habana, and others land here.
fn probe_accel_class(p: &SystemProbe, out: &mut Vec<AcceleratorInfo>) {
    let base = p.path("/sys/class/accel");
    for name in sysfs::list_dir(&base) {
        let dev = base.join(&name).join("device");
        let driver = sysfs::keyval(&dev.join("uevent"), "DRIVER");
        let vendor = pci_vendor_slug(sysfs::read_u64(&dev.join("vendor"))).unwrap_or_else(|| {
            driver
                .as_deref()
                .map(driver_vendor_slug)
                .unwrap_or("unknown")
                .to_string()
        });
        out.push(AcceleratorInfo {
            kind: AcceleratorKind::Npu,
            vendor,
            name: Some(name.clone()),
            driver,
            discrete: false,
            vram_bytes: None,
            compute_apis: vec!["onnxruntime".to_string()],
            source: format!("/sys/class/accel/{name}"),
        });
    }
}

/// SoC NPUs that predate `/sys/class/accel`, most importantly Rockchip's RKNPU on
/// RK3588 — one of the most common capable arm64 SBC platforms.
fn probe_device_tree_npu(p: &SystemProbe, out: &mut Vec<AcceleratorInfo>) {
    if sysfs::exists(&p.path("/dev/rknpu")) || sysfs::exists(&p.path("/sys/kernel/debug/rknpu")) {
        push_unique_npu(out, "rockchip", "rknpu", "rknn", "/dev/rknpu");
    }
    // Device-tree node names are the fallback: `npu@fdab0000`, `hailo`, etc.
    for node in sysfs::list_dir(&p.path("/proc/device-tree")) {
        let lower = node.to_ascii_lowercase();
        if lower.starts_with("npu@") || lower == "npu" {
            push_unique_npu(out, "soc", "device-tree", "onnxruntime", "/proc/device-tree");
        }
    }
    if sysfs::exists(&p.path("/dev/hailo0")) {
        push_unique_npu(out, "hailo", "hailo", "hailort", "/dev/hailo0");
    }
    if sysfs::exists(&p.path("/dev/apex_0")) {
        push_unique_npu(out, "google", "apex", "edgetpu", "/dev/apex_0");
    }
}

fn push_unique_npu(out: &mut Vec<AcceleratorInfo>, vendor: &str, driver: &str, api: &str, source: &str) {
    if out
        .iter()
        .any(|a| a.kind == AcceleratorKind::Npu && a.vendor == vendor)
    {
        return;
    }
    out.push(AcceleratorInfo {
        kind: AcceleratorKind::Npu,
        vendor: vendor.to_string(),
        name: Some(driver.to_string()),
        driver: Some(driver.to_string()),
        discrete: false,
        vram_bytes: None,
        compute_apis: vec![api.to_string()],
        source: source.to_string(),
    });
}

fn pci_vendor_slug(id: Option<u64>) -> Option<String> {
    Some(
        match id? {
            0x10de => "nvidia",
            0x1002 | 0x1022 => "amd",
            0x8086 => "intel",
            0x14e4 => "broadcom",
            0x1de1 => "tekram",
            _ => return None,
        }
        .to_string(),
    )
}

fn driver_vendor_slug(driver: &str) -> &'static str {
    match driver {
        "nvidia" | "nouveau" => "nvidia",
        "amdgpu" | "radeon" | "amdxdna" => "amd",
        "i915" | "xe" | "intel_vpu" => "intel",
        "vc4" | "v3d" => "broadcom",
        "panfrost" | "panthor" | "mali" => "arm",
        "lima" => "arm",
        "rknpu" => "rockchip",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_nodes_exclude_connectors() {
        assert!(is_card_node("card0"));
        assert!(is_card_node("card12"));
        assert!(!is_card_node("card0-HDMI-A-1"));
        assert!(!is_card_node("card"));
        assert!(!is_card_node("renderD128"));
        assert!(!is_card_node("version"));
    }

    #[test]
    fn discreteness_prefers_real_vram_over_driver_guesses() {
        assert!(is_discrete("amd", Some("amdgpu"), Some(16 * 1024 * 1024 * 1024)));
        assert!(
            !is_discrete("amd", Some("amdgpu"), Some(512 * 1024 * 1024)),
            "an APU carve-out is not a discrete GPU"
        );
    }

    #[test]
    fn soc_gpus_are_integrated() {
        assert!(!is_discrete("broadcom", Some("v3d"), None));
        assert!(!is_discrete("arm", Some("panfrost"), None));
        assert!(!is_discrete("intel", Some("i915"), None));
    }

    #[test]
    fn nvidia_without_vram_reading_is_still_discrete() {
        assert!(is_discrete("nvidia", Some("nvidia"), None));
    }

    #[test]
    fn unknown_driver_without_vram_is_not_assumed_discrete() {
        assert!(!is_discrete("unknown", Some("weird_gpu"), None));
    }

    #[test]
    fn pci_vendor_ids_map() {
        assert_eq!(pci_vendor_slug(Some(0x10de)).as_deref(), Some("nvidia"));
        assert_eq!(pci_vendor_slug(Some(0x1002)).as_deref(), Some("amd"));
        assert_eq!(pci_vendor_slug(Some(0xdead)), None);
        assert_eq!(pci_vendor_slug(None), None);
    }
}

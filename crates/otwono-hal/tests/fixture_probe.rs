//! Fixture-driven probe tests.
//!
//! These exercise the whole probe stack against captured `/proc` and `/sys` trees. See
//! `tools/capture-hw-fixture.sh` for how a fixture is made.
//!
//! Fixtures whose `.otwono-probe/capture.json` says `"synthetic": true` are hand-written
//! from published specifications and are placeholders until real hardware is available
//! (CLAUDE.md §6). They still guard the parsers against regressions.

use otwono_hal::{AcceleratorKind, SystemProbe};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn is_synthetic(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(".otwono-probe/capture.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("synthetic").and_then(|s| s.as_bool()))
        .unwrap_or(false)
}

#[test]
fn every_fixture_declares_its_provenance() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("fixtures directory must exist") {
        let path = entry.unwrap().path();
        if !path.is_dir() {
            continue;
        }
        let meta = path.join(".otwono-probe/capture.json");
        assert!(
            meta.exists(),
            "{} has no .otwono-probe/capture.json; every fixture must say where it came from",
            path.display()
        );
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&meta).unwrap())
            .expect("capture.json must be valid JSON");
        assert!(
            v.get("synthetic").is_some(),
            "{} must declare `synthetic`",
            path.display()
        );
        assert!(
            v.get("label").is_some(),
            "{} must declare `label`",
            path.display()
        );
        checked += 1;
    }
    assert!(checked >= 3, "expected at least three fixtures, found {checked}");
}

#[test]
fn real_capture_of_the_dev_vm_round_trips() {
    let dir = fixture("x86_64-cloud-vm");
    assert!(!is_synthetic(&dir), "this fixture is a real capture");

    let r = SystemProbe::from_root(&dir).probe();
    assert_eq!(r.machine.architecture, "x86_64");
    assert_eq!(r.cpu.logical_cpus, 4);
    assert_eq!(r.cpu.physical_cores, 4);
    assert_eq!(r.cpu.vendor.as_deref(), Some("intel"));
    assert!(r.cpu.features.avx2 && r.cpu.features.avx512f);
    assert!(r.memory.total_bytes > 15 * 1024 * 1024 * 1024);
    assert!(r.accelerators.is_empty(), "the dev VM has no GPU");
    assert!(r.network.has_default_route);
    assert!(r
        .network
        .interfaces
        .iter()
        .any(|i| i.name == "eth0" && i.operstate == "up"));
    assert!(!r.power.has_battery);
    // The only warning should be the absent accelerator.
    assert!(
        r.warnings.iter().all(|w| w.starts_with("accelerator")),
        "unexpected warnings: {:?}",
        r.warnings
    );
}

#[test]
fn rpi5_fixture_detects_arm_features_and_an_integrated_gpu() {
    let dir = fixture("aarch64-rpi5-8gb-synthetic");
    let r = SystemProbe::from_root(&dir).probe();

    assert_eq!(r.machine.architecture, "aarch64");
    assert_eq!(r.machine.model.as_deref(), Some("Raspberry Pi 5 Model B Rev 1.0"));
    assert_eq!(r.cpu.logical_cpus, 4);
    assert_eq!(r.cpu.vendor.as_deref(), Some("arm"));
    assert!(r.cpu.features.neon);
    assert!(r.cpu.features.dotprod, "Cortex-A76 has asimddp");
    assert!(!r.cpu.features.avx2);
    assert_eq!(r.cpu.max_frequency_mhz, Some(2400));

    assert_eq!(r.accelerators.len(), 1);
    let gpu = &r.accelerators[0];
    assert_eq!(gpu.kind, AcceleratorKind::Gpu);
    assert_eq!(gpu.driver.as_deref(), Some("v3d"));
    assert!(!gpu.discrete, "VideoCore is integrated");

    // The SD card and the NVMe both count; loop devices must not appear.
    assert_eq!(r.storage.devices.len(), 2);
    assert!(!r.storage.rotational_only);

    assert!(r
        .network
        .interfaces
        .iter()
        .any(|i| i.name == "wlan0" && i.kind == otwono_hal::network::InterfaceKind::Wireless));
}

#[test]
fn workstation_fixture_separates_the_discrete_card_from_the_igpu() {
    let dir = fixture("x86_64-gpu-workstation-synthetic");
    let r = SystemProbe::from_root(&dir).probe();

    assert_eq!(r.cpu.logical_cpus, 32);
    assert_eq!(r.cpu.physical_cores, 16, "16 cores, 32 threads");

    let gpus: Vec<_> = r
        .accelerators
        .iter()
        .filter(|a| a.kind == AcceleratorKind::Gpu)
        .collect();
    assert_eq!(gpus.len(), 2, "a discrete card plus the CPU's iGPU");
    let discrete: Vec<_> = gpus.iter().filter(|g| g.discrete).collect();
    assert_eq!(
        discrete.len(),
        1,
        "the 512 MiB Raphael iGPU must not count as discrete"
    );
    assert_eq!(discrete[0].vram_bytes, Some(25_753_026_560));
    assert!(
        discrete[0].compute_apis.contains(&"rocm".to_string()),
        "/dev/kfd present implies ROCm"
    );

    // A rotational bulk drive alongside NVMe must not make the machine rotational-only.
    assert!(!r.storage.rotational_only);
    assert_eq!(r.storage.devices.len(), 2);
}

//! End-to-end classification against captured hardware fixtures.
//!
//! These are the tests that would catch a threshold change silently re-tiering real
//! machines. Each asserts the tier *and* the limiting axis, because "T2" without "limited
//! by accelerator" does not tell an operator anything actionable.

use otwono_capability::{classify, AcceleratorClass, MemoryClass, Tier};
use otwono_hal::SystemProbe;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../otwono-hal/tests/fixtures")
        .join(name)
}

fn profile_of(name: &str) -> otwono_capability::CapabilityProfile {
    classify(&SystemProbe::from_root(fixture(name)).probe())
}

#[test]
fn cloud_vm_is_t2_limited_by_the_absent_gpu() {
    let p = profile_of("x86_64-cloud-vm");
    assert_eq!(p.tier, Tier::T2Balanced);
    assert_eq!(p.axes.accelerator, AcceleratorClass::None);
    assert_eq!(p.axes.memory, MemoryClass::High);
    assert!(
        p.limiting_factor.as_deref().unwrap().starts_with("accelerator"),
        "got {:?}",
        p.limiting_factor
    );
    assert!(p.features.local_llm);
    assert!(!p.features.image_generation, "no GPU means no image generation");
    assert!(!p.features.serve_ai_to_peers);
}

#[test]
fn rpi5_8gb_is_t2_and_offers_a_tier_appropriate_model() {
    let p = profile_of("aarch64-rpi5-8gb-synthetic");
    assert_eq!(p.tier, Tier::T2Balanced);
    assert_eq!(p.axes.accelerator, AcceleratorClass::Igpu);
    assert_eq!(p.features.max_model_parameters, Some(8_000_000_000));
    assert!(p.features.local_rag);
    assert!(!p.features.image_generation);
    assert_eq!(p.features.desktop, otwono_capability::DesktopProfile::Light);
}

#[test]
fn gpu_workstation_is_t4_with_no_limiting_factor() {
    let p = profile_of("x86_64-gpu-workstation-synthetic");
    assert_eq!(p.tier, Tier::T4Workstation);
    assert_eq!(p.limiting_factor, None);
    assert_eq!(p.axes.accelerator, AcceleratorClass::GpuLarge);
    assert!(p.features.serve_ai_to_peers);
    assert!(p.features.image_generation);
    assert!(p.features.eligible_node_roles.contains(&"archive".to_string()));
    assert!(p
        .features
        .eligible_node_roles
        .contains(&"ai-provider".to_string()));
}

#[test]
fn every_fixture_produces_a_serialisable_profile_with_a_known_tier() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../otwono-hal/tests/fixtures");
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if !path.is_dir() {
            continue;
        }
        let p = classify(&SystemProbe::from_root(&path).probe());
        let json = serde_json::to_string(&p).expect("profile must serialise");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            Tier::parse(v["tier"].as_str().unwrap()).is_some(),
            "{} produced an unparseable tier",
            path.display()
        );
    }
}

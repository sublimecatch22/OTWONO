//! Contract test: every capability profile OTWONO emits must validate against the
//! published JSON Schema.
//!
//! This is what makes `schemas/` a contract rather than documentation. A field added to
//! the Rust struct without a matching schema change fails here (the schema sets
//! `additionalProperties: false` throughout), and so does a schema tightened beyond what
//! the code produces.

use otwono_capability::{classify, classify_with_overrides, CapabilityOverrides, Tier};
use otwono_hal::SystemProbe;
use std::path::{Path, PathBuf};

fn schema_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/capability-profile.schema.json")
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/otwono-hal/tests/fixtures")
}

fn load_schema() -> serde_json::Value {
    let text = std::fs::read_to_string(schema_path()).expect("schema file must exist");
    serde_json::from_str(&text).expect("schema must be valid JSON")
}

fn assert_valid(validator: &jsonschema::Validator, instance: &serde_json::Value, what: &str) {
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("{}: {e}", e.instance_path))
        .collect();
    assert!(
        errors.is_empty(),
        "{what} failed schema validation:\n  {}",
        errors.join("\n  ")
    );
}

#[test]
fn schema_itself_compiles() {
    jsonschema::validator_for(&load_schema()).expect("the schema must be a valid JSON Schema");
}

#[test]
fn every_fixture_profile_validates() {
    let validator = jsonschema::validator_for(&load_schema()).unwrap();
    let mut checked = 0;

    for entry in std::fs::read_dir(fixtures_dir()).expect("fixtures must exist") {
        let path = entry.unwrap().path();
        if !path.is_dir() {
            continue;
        }
        let profile = classify(&SystemProbe::from_root(&path).probe());
        let json = serde_json::to_value(&profile).unwrap();
        assert_valid(&validator, &json, &format!("profile for {}", path.display()));
        checked += 1;
    }
    assert!(
        checked >= 3,
        "expected at least three fixtures, validated {checked}"
    );
}

#[test]
fn a_profile_from_a_bare_machine_validates() {
    // The degenerate case: nothing detectable at all. It must still be a valid document,
    // because that is exactly the state a first boot on unknown hardware can produce.
    let validator = jsonschema::validator_for(&load_schema()).unwrap();
    let profile = classify(&SystemProbe::from_root("/nonexistent-probe-root").probe());
    assert_eq!(profile.tier, Tier::T0Micro);
    assert_valid(
        &validator,
        &serde_json::to_value(&profile).unwrap(),
        "bare-machine profile",
    );
}

#[test]
fn an_overridden_profile_validates_and_carries_the_detected_block() {
    let validator = jsonschema::validator_for(&load_schema()).unwrap();
    let overrides = CapabilityOverrides {
        tier: Some(Tier::T4Workstation),
        ..Default::default()
    };
    let report = SystemProbe::from_root(fixtures_dir().join("x86_64-cloud-vm")).probe();
    let profile = classify_with_overrides(&report, &overrides);

    let json = serde_json::to_value(&profile).unwrap();
    assert_valid(&validator, &json, "overridden profile");
    assert_eq!(json["overridden"], true);
    assert!(
        json.get("detected").is_some(),
        "an overridden profile must record what was detected"
    );
    assert_eq!(json["detected"]["tier"], "T2_BALANCED");
}

#[test]
fn the_live_machine_validates() {
    // Whatever CI happens to be running on, its own profile must satisfy the contract.
    let validator = jsonschema::validator_for(&load_schema()).unwrap();
    let profile = classify(&SystemProbe::system().probe());
    assert_valid(
        &validator,
        &serde_json::to_value(&profile).unwrap(),
        "live machine profile",
    );
}

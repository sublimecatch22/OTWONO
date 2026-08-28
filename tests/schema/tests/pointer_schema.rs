//! The pointer record against its published schema (ADR-0027).
//!
//! A schema is only worth having if something checks the code against it. The value here is
//! that the schema is what a *second implementation* would read — so the interesting
//! assertions are the ones where a conformant-looking record must be refused, since those
//! are the rules a reimplementation would otherwise get wrong quietly.

use otwono_identity::NodeIdentity;
use otwono_pointer::{Pointer, SCHEMA_VERSION};
use std::path::{Path, PathBuf};

fn schema_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/pointer.schema.json")
}

fn validator() -> jsonschema::Validator {
    let text = std::fs::read_to_string(schema_path()).expect("the schema is readable");
    let schema: serde_json::Value = serde_json::from_str(&text).expect("the schema is JSON");
    jsonschema::validator_for(&schema).expect("the schema itself is valid")
}

fn assert_valid(value: &serde_json::Value) {
    let v = validator();
    if let Err(e) = v.validate(value) {
        panic!("{value} failed its own schema: {e}");
    }
}

fn assert_invalid(value: &serde_json::Value, why: &str) {
    assert!(
        validator().validate(value).is_err(),
        "the schema accepted {why}: {value}"
    );
}

fn identity() -> NodeIdentity {
    NodeIdentity::from_seeds(&[7; 32], &[7; 32], 1)
}

fn signed(sequence: u64, content: Option<String>) -> Pointer {
    let id = identity();
    let mut p = Pointer::new(id.node_id(), "wiki", "Home", sequence, content, 1_700_000_000_000);
    let payload = p.payload_for_id_sign().unwrap();
    let signature = id.sign(&otwono_identity::domain_separated(&payload));
    p.signature = data_encoding::BASE64.encode(&signature.to_bytes());
    p
}

fn encoded(p: &Pointer) -> serde_json::Value {
    serde_json::to_value(p).unwrap()
}

#[test]
fn a_real_pointer_validates() {
    assert_valid(&encoded(&signed(1, Some("ab".repeat(32)))));
    // And a tombstone, which is the same record with no content id.
    let tombstone = encoded(&signed(2, None));
    assert!(tombstone.get("content_id").is_none(), "absent, not null");
    assert_valid(&tombstone);
}

/// The schema's version is the one the code writes.
#[test]
fn the_schema_id_and_the_crate_agree_on_the_version() {
    let text = std::fs::read_to_string(schema_path()).unwrap();
    assert!(
        text.contains(&format!("pointer-{SCHEMA_VERSION}.json")),
        "the schema $id does not name {SCHEMA_VERSION}"
    );
}

/// Sequence zero is refused by the schema as well as by the code.
///
/// Zero means "nothing seen yet" in a reader's log, so a record using it would make a first
/// record and an absent one compare equal. A second implementation reading only the schema
/// has to learn that from the schema.
#[test]
fn the_schema_refuses_sequence_zero_like_the_code_does() {
    let mut v = encoded(&signed(1, Some("cd".repeat(32))));
    v["sequence"] = serde_json::json!(0);
    assert_invalid(&v, "sequence zero");

    let mut zero = signed(1, Some("cd".repeat(32)));
    zero.sequence = 0;
    assert!(
        zero.verify(&identity().public_key_bytes()).is_err(),
        "the code accepted what the schema refuses"
    );
}

/// An unknown field is refused, because the parser refuses it too.
#[test]
fn the_schema_refuses_a_field_the_parser_would_reject() {
    let mut v = encoded(&signed(1, Some("ef".repeat(32))));
    v["expires_at"] = serde_json::json!(123);
    assert_invalid(&v, "an unknown field");
    assert!(
        serde_json::from_value::<Pointer>(v).is_err(),
        "the parser accepted a field the schema refuses"
    );
}

/// A content id must be lowercase hex of the right length, in both places.
#[test]
fn the_schema_and_the_code_agree_on_what_a_content_id_looks_like() {
    for (why, bad) in [
        ("uppercase", "AB".repeat(32)),
        ("too short", "ab".repeat(31)),
        ("not hex", "zz".repeat(32)),
    ] {
        let mut v = encoded(&signed(1, Some("ab".repeat(32))));
        v["content_id"] = serde_json::json!(bad);
        assert_invalid(&v, why);
    }
}

/// The service name is constrained, so a second implementation cannot invent namespaces
/// that this one would refuse.
#[test]
fn the_schema_bounds_the_service_name() {
    for bad in ["Wiki", "wiki/sub", "", "-leading"] {
        let mut v = encoded(&signed(1, Some("ab".repeat(32))));
        v["service"] = serde_json::json!(bad);
        assert_invalid(&v, &format!("service {bad:?}"));
    }
    for good in ["wiki", "profile", "forum", "a", "my-service9"] {
        let mut v = encoded(&signed(1, Some("ab".repeat(32))));
        v["service"] = serde_json::json!(good);
        assert_valid(&v);
    }
}

/// Every required field is required.
#[test]
fn dropping_any_required_field_is_refused() {
    for field in [
        "schema_version",
        "node_id",
        "service",
        "name",
        "sequence",
        "published_at_ms",
        "signature",
    ] {
        let mut v = encoded(&signed(1, Some("ab".repeat(32))));
        v.as_object_mut().unwrap().remove(field);
        assert_invalid(&v, &format!("a record with no {field}"));
    }
}

/// The schema says what the record is for, in the record.
///
/// Not decoration. A schema is read by people implementing against it, and "sequence is the
/// rollback defence, never a timestamp" is the one thing a reimplementation must not get
/// wrong — ordering by `published_at_ms` would look correct and be broken.
#[test]
fn the_schema_explains_that_sequence_orders_and_the_timestamp_does_not() {
    let text = std::fs::read_to_string(schema_path()).unwrap();
    let schema: serde_json::Value = serde_json::from_str(&text).unwrap();
    let sequence = schema["properties"]["sequence"]["description"]
        .as_str()
        .expect("sequence is described");
    assert!(sequence.contains("rollback"), "{sequence}");

    let published = schema["properties"]["published_at_ms"]["description"]
        .as_str()
        .expect("published_at_ms is described");
    assert!(
        published.contains("NEVER used for ordering"),
        "the schema does not warn a reimplementer off ordering by timestamp: {published}"
    );
}

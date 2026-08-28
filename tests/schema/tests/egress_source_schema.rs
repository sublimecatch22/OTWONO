//! Contract test: the allow-list a node loads must be the allow-list the schema describes.
//!
//! The schema is the thing an operator reads before writing a file that decides where this
//! node may send bytes, so a loader that quietly accepts more than the schema documents is
//! worse than no schema at all. These tests run both directions: a file the loader accepts
//! must validate, and a file the schema rejects must not load.

use otwono_fetch::SourceSet;
use std::path::{Path, PathBuf};

fn schema_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/egress-source.schema.json")
}

fn validator() -> jsonschema::Validator {
    let text = std::fs::read_to_string(schema_path()).expect("schema file must exist");
    let schema: serde_json::Value = serde_json::from_str(&text).expect("schema must be valid JSON");
    jsonschema::validator_for(&schema).expect("the schema must be a valid JSON Schema")
}

/// The TOML a node actually loads, re-expressed as the JSON the schema describes.
fn as_json(toml_text: &str) -> serde_json::Value {
    let parsed: toml::Value = toml::from_str(toml_text).expect("test input must be valid TOML");
    serde_json::to_value(parsed).expect("TOML converts to JSON")
}

fn assert_valid(toml_text: &str) {
    let v = validator();
    let instance = as_json(toml_text);
    let errors: Vec<String> = v.iter_errors(&instance).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "should validate but did not: {errors:?}");
}

fn assert_invalid(toml_text: &str) {
    let v = validator();
    let instance = as_json(toml_text);
    assert!(
        !v.is_valid(&instance),
        "schema accepted something it should reject: {instance}"
    );
}

const REAL: &str = r#"
[[source]]
id = "models"
host = "models.example.org"
path_prefix = "/otwono/models/"
max_bytes = 21474836480

[[source]]
id = "updates"
host = "updates.example.org"
port = 8443
path_prefix = "/"
max_bytes = 4294967296
"#;

#[test]
fn a_file_the_loader_accepts_validates() {
    SourceSet::parse(REAL).expect("the loader must accept this");
    assert_valid(REAL);
}

#[test]
fn an_empty_allow_list_is_a_valid_document() {
    let empty = "";
    SourceSet::parse(empty).expect("the loader must accept an empty file");
    assert_valid(empty);
}

#[test]
fn the_schema_and_the_loader_agree_about_what_is_wrong() {
    // Each of these is refused by the running code. If the schema accepted one, an operator
    // could write a file that reads as correct and then stops the daemon starting.
    let cases = [
        // No trailing slash: a suffix could extend the last segment.
        r#"
            [[source]]
            id = "models"
            host = "models.example.org"
            path_prefix = "/otwono/model"
            max_bytes = 1024
        "#,
        // An IP literal is not a name.
        r#"
            [[source]]
            id = "models"
            host = "10.0.0.5"
            path_prefix = "/"
            max_bytes = 1024
        "#,
        // Uppercase host: matching is case-insensitive, but the file should read the way
        // the code compares it.
        r#"
            [[source]]
            id = "models"
            host = "Models.Example.org"
            path_prefix = "/"
            max_bytes = 1024
        "#,
        // A cap of zero permits nothing; delete the source instead.
        r#"
            [[source]]
            id = "models"
            host = "models.example.org"
            path_prefix = "/"
            max_bytes = 0
        "#,
        // An id that is not a safe, stable name.
        r#"
            [[source]]
            id = "Models Host"
            host = "models.example.org"
            path_prefix = "/"
            max_bytes = 1024
        "#,
        // A relative prefix.
        r#"
            [[source]]
            id = "models"
            host = "models.example.org"
            path_prefix = "otwono/"
            max_bytes = 1024
        "#,
    ];
    for case in cases {
        assert!(
            SourceSet::parse(case).is_err(),
            "the loader should reject: {case}"
        );
        assert_invalid(case);
    }
}

#[test]
fn an_unknown_field_is_refused_by_both() {
    // A misspelled key must not be silently ignored: "max_byte = 10" would otherwise read
    // as a cap and impose none.
    let case = r#"
        [[source]]
        id = "models"
        host = "models.example.org"
        path_prefix = "/"
        max_bytes = 1024
        max_byte = 10
    "#;
    assert_invalid(case);
    assert!(
        SourceSet::parse(case).is_err(),
        "the loader should reject an unknown field too"
    );
}

#[test]
fn the_schema_example_is_one_the_loader_would_load() {
    // A schema whose own example does not work is a document nobody can trust.
    let text = std::fs::read_to_string(schema_path()).expect("schema file");
    let schema: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    let example = &schema["examples"][0];
    assert!(validator().is_valid(example), "the example must validate");
    let as_toml = toml::to_string(example).expect("the example converts to TOML");
    SourceSet::parse(&as_toml).expect("the loader must accept the schema's own example");
}

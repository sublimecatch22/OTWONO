//! Contract test: what the store writes must be what the schema describes.
//!
//! Both directions. A record the store produces has to validate, and the constraints the
//! schema states — the chunking constant, the digest shape, the maximum chunk length — have
//! to be ones the code actually holds to. A schema that documents a stricter store than
//! exists is worse than none, because someone will rely on it.

use otwono_store::{cas::Store, chunk, label::Visibility, object::Object};
use std::path::{Path, PathBuf};

fn schema_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/content-object.schema.json")
}

fn validator() -> jsonschema::Validator {
    let text = std::fs::read_to_string(schema_path()).expect("schema file must exist");
    let schema: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    jsonschema::validator_for(&schema).expect("a valid JSON Schema")
}

fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    // Mixed, not `seed | 1`: that maps 2 and 3 (and every other adjacent pair) to the same
    // stream, so two "different" fixtures come out byte-identical. Cost one debugging round
    // in the cache's LRU test.
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn assert_valid(o: &Object) {
    let v = validator();
    let instance = serde_json::to_value(o).expect("serialize");
    let errors: Vec<String> = v.iter_errors(&instance).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "record failed its own schema: {errors:?}");
}

#[test]
fn a_record_the_store_writes_validates() {
    let dir = std::env::temp_dir().join(format!("otwono-schema-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let s = Store::new(&dir);
    // Several sizes, so single-chunk and many-chunk records are both covered.
    for (len, seed) in [(0usize, 1u64), (100, 2), (100_000, 3), (4 << 20, 4)] {
        let o = s.put_bytes(&data(len, seed), Visibility::Public).expect("put");
        assert_valid(&o);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_label_the_code_has_is_a_label_the_schema_allows() {
    for v in [
        Visibility::Private,
        Visibility::Shared,
        Visibility::Public,
        Visibility::Replicated,
    ] {
        assert_valid(&Object::new(&chunk::slice(&data(70_000, 5)), v));
    }
}

#[test]
fn the_schemas_chunking_constant_is_the_one_the_code_uses() {
    // If these drift, every record this node writes is invalid against its own contract.
    let text = std::fs::read_to_string(schema_path()).expect("schema");
    let schema: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    assert_eq!(
        schema["properties"]["chunking"]["const"],
        serde_json::json!(chunk::CHUNKING_VERSION)
    );
}

#[test]
fn the_schemas_maximum_chunk_is_the_one_the_chunker_enforces() {
    let text = std::fs::read_to_string(schema_path()).expect("schema");
    let schema: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    let max = &schema["properties"]["chunks"]["items"]["properties"]["length"]["maximum"];
    assert_eq!(max, &serde_json::json!(chunk::MAX_CHUNK));
}

#[test]
fn the_schema_rejects_what_the_code_rejects() {
    let v = validator();
    let good = Object::new(&chunk::slice(&data(70_000, 6)), Visibility::Public);

    // Chunked under other rules.
    let mut foreign = serde_json::to_value(&good).expect("serialize");
    foreign["chunking"] = serde_json::json!("fastcdc-v2020-4k-16k-64k");
    assert!(!v.is_valid(&foreign), "a foreign chunking version must fail");

    // A digest that is not one.
    let mut bad_digest = serde_json::to_value(&good).expect("serialize");
    bad_digest["chunks"][0]["blake3"] = serde_json::json!("nonsense");
    assert!(!v.is_valid(&bad_digest), "a malformed digest must fail");

    // A chunk over the ceiling.
    let mut huge = serde_json::to_value(&good).expect("serialize");
    huge["chunks"][0]["length"] = serde_json::json!(chunk::MAX_CHUNK + 1);
    assert!(!v.is_valid(&huge), "an oversized chunk must fail");

    // A field from a version this node does not understand.
    let mut extra = serde_json::to_value(&good).expect("serialize");
    extra["expires_at"] = serde_json::json!(1);
    assert!(!v.is_valid(&extra), "an unknown field must fail");
}

#[test]
fn an_unrecognised_label_is_refused_by_the_schema_and_read_as_private_by_the_code() {
    // The two halves of the fail-closed rule. The schema tells an author they wrote
    // something wrong; the reader, faced with it anyway, chooses the safe meaning rather
    // than erroring — because a record that cannot be read must not become more available.
    let good = Object::new(&chunk::slice(&data(70_000, 7)), Visibility::Public);
    let mut odd = serde_json::to_value(&good).expect("serialize");
    odd["visibility"] = serde_json::json!("world-readable");

    assert!(!validator().is_valid(&odd), "the schema names the four labels");
    let parsed: Object = serde_json::from_value(odd).expect("the reader must not fail");
    assert_eq!(parsed.visibility, Visibility::Private);
}

#[test]
fn a_shared_record_the_store_writes_validates() {
    // The path that produces a sharing envelope, checked against the schema that describes
    // one. Several sizes so a single-frame and a multi-frame object are both covered.
    let dir = std::env::temp_dir().join(format!("otwono-schema-shared-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let s = Store::new(&dir);
    let key = otwono_identity::SharingKey::from_seed(&[3u8; 32], 1_700_000_000_000);
    let recipients = [otwono_store::cas::Recipient {
        node_id: "otw1exampleneighbour".to_string(),
        sharing_public_key: key.public(),
    }];

    for (len, seed) in [(0usize, 11u64), (100_000, 12), (2 << 20, 13)] {
        let (o, _) = s
            .put_shared_reader(data(len, seed).as_slice(), &recipients)
            .expect("put_shared_reader");
        assert_valid(&o);
        let sharing = o.sharing.as_ref().expect("shared objects carry an envelope");
        assert_eq!(sharing.plaintext_size_bytes, len as u64);
        assert!(
            o.size_bytes > sharing.plaintext_size_bytes,
            "the ciphertext is longer by a tag per frame"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_schema_refuses_a_shared_envelope_with_no_recipients() {
    // The invariant the code enforces, stated in the contract too: an object nobody can
    // open is not a shared object.
    let dir = std::env::temp_dir().join(format!("otwono-schema-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let s = Store::new(&dir);
    let key = otwono_identity::SharingKey::from_seed(&[4u8; 32], 1_700_000_000_000);
    let (o, _) = s
        .put_shared_reader(
            data(50_000, 14).as_slice(),
            &[otwono_store::cas::Recipient {
                node_id: "otw1someone".to_string(),
                sharing_public_key: key.public(),
            }],
        )
        .expect("put");

    let mut instance = serde_json::to_value(&o).expect("serialize");
    instance["sharing"]["sealed_keys"] = serde_json::json!([]);
    let v = validator();
    assert!(
        v.iter_errors(&instance).next().is_some(),
        "an empty recipient list must not validate"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_schema_refuses_an_unrecognised_encryption_scheme() {
    // The code refuses it rather than guessing; the schema must say the same, or a record
    // written by a future build would look contractually fine to this one.
    let dir = std::env::temp_dir().join(format!("otwono-schema-scheme-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let s = Store::new(&dir);
    let key = otwono_identity::SharingKey::from_seed(&[5u8; 32], 1_700_000_000_000);
    let (o, _) = s
        .put_shared_reader(
            data(20_000, 15).as_slice(),
            &[otwono_store::cas::Recipient {
                node_id: "otw1someone".to_string(),
                sharing_public_key: key.public(),
            }],
        )
        .expect("put");

    let mut instance = serde_json::to_value(&o).expect("serialize");
    instance["sharing"]["encryption"] = serde_json::json!("aes-gcm-siv-whatever");
    let v = validator();
    assert!(v.iter_errors(&instance).next().is_some());

    // And the code agrees, rather than only the schema saying so.
    let mut record = o.clone();
    record.sharing.as_mut().unwrap().encryption = "aes-gcm-siv-whatever".to_string();
    assert!(record.validate().is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

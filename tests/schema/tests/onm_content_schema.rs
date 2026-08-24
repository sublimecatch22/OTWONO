//! Contract test: the ONM content messages on the wire must be what the schema describes.
//!
//! Both directions, as always. Every message the transport can encode has to validate, and
//! every constraint the schema states has to be one the code enforces — a schema that
//! promises a stricter peer than exists is worse than no schema, because a second
//! implementation will believe it.

use otwono_net::content::{
    self, ChunkEntry, ChunkPart, ManifestPage, Request, Response, MAX_CHUNKS_PER_REQUEST, MAX_CHUNK_BYTES,
};
use std::path::{Path, PathBuf};

fn schema_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/onm-content.schema.json")
}

fn validator() -> jsonschema::Validator {
    let text = std::fs::read_to_string(schema_path()).expect("schema file must exist");
    let schema: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    jsonschema::validator_for(&schema).expect("a valid JSON Schema")
}

fn assert_valid(value: &serde_json::Value) {
    let v = validator();
    let errors: Vec<String> = v.iter_errors(value).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{value} failed its own schema: {errors:?}");
}

fn assert_invalid(value: &serde_json::Value, why: &str) {
    assert!(!validator().is_valid(value), "schema accepted {why}: {value}");
}

fn id(byte: u8) -> String {
    std::iter::repeat_n(format!("{byte:02x}"), 32).collect()
}

fn encoded<T: serde::Serialize>(v: &T) -> serde_json::Value {
    serde_json::from_slice(&content::encode(v).expect("encode")).expect("valid JSON")
}

#[test]
fn both_requests_validate() {
    assert_valid(&encoded(&Request::Manifest {
        content_id: id(0xab),
        from_chunk: 0,
        max_chunks: 16,
    }));
    assert_valid(&encoded(&Request::Chunk {
        content_id: id(0xab),
        digest: id(0xcd),
        offset: 4096,
        max_bytes: 8192,
    }));
}

#[test]
fn all_three_responses_validate() {
    assert_valid(&encoded(&Response::Manifest(ManifestPage {
        content_id: id(1),
        size_bytes: 70_000,
        chunking: otwono_store::CHUNKING_VERSION.to_string(),
        visibility: "public".into(),
        total_chunks: 2,
        from_chunk: 0,
        chunks: vec![
            ChunkEntry {
                blake3: id(2),
                length: 65_536,
            },
            ChunkEntry {
                blake3: id(3),
                length: 4_464,
            },
        ],
    })));
    assert_valid(&encoded(&Response::Chunk(ChunkPart {
        content_id: id(1),
        digest: id(2),
        offset: 0,
        total_length: 5,
        data: data_encoding::BASE64.encode(b"hello"),
    })));
    assert_valid(&encoded(&Response::not_available(id(9))));
}

#[test]
fn the_schema_refuses_a_label_that_may_not_leave_the_node() {
    // The one that matters. If `private` ever validates here, the schema has stopped
    // describing the boundary.
    for label in ["private", "shared", "", "PUBLIC"] {
        let mut page = encoded(&Response::Manifest(ManifestPage {
            content_id: id(1),
            size_bytes: 1,
            chunking: otwono_store::CHUNKING_VERSION.to_string(),
            visibility: "public".into(),
            total_chunks: 0,
            from_chunk: 0,
            chunks: vec![],
        }));
        page["visibility"] = serde_json::json!(label);
        assert_invalid(&page, &format!("a manifest labelled {label:?}"));
    }
}

#[test]
fn the_schema_refuses_a_refusal_that_explains_itself() {
    // A `reason` field would be a disclosure channel. The schema must reject one so a
    // second implementation cannot add it and stay conformant.
    let mut refusal = encoded(&Response::not_available(id(9)));
    refusal["reason"] = serde_json::json!("private");
    assert_invalid(&refusal, "a refusal carrying a reason");
}

#[test]
fn the_schema_refuses_an_uppercase_digest() {
    let mut req = encoded(&Request::Manifest {
        content_id: id(0xab),
        from_chunk: 0,
        max_chunks: 1,
    });
    req["content_id"] = serde_json::json!(id(0xab).to_uppercase());
    assert_invalid(&req, "an uppercase content id");
}

#[test]
fn the_ceilings_in_the_schema_are_the_ceilings_in_the_code() {
    // The schema's maxima and Request::validate must agree, or one of them is a lie.
    let over_bytes = Request::Chunk {
        content_id: id(1),
        digest: id(2),
        offset: 0,
        max_bytes: MAX_CHUNK_BYTES + 1,
    };
    let over_chunks = Request::Manifest {
        content_id: id(1),
        from_chunk: 0,
        max_chunks: MAX_CHUNKS_PER_REQUEST + 1,
    };
    for r in [&over_bytes, &over_chunks] {
        assert!(r.validate().is_err(), "the code accepted {r:?}");
        assert_invalid(&encoded(r), "a request over the ceiling");
    }

    let at_bytes = Request::Chunk {
        content_id: id(1),
        digest: id(2),
        offset: 0,
        max_bytes: MAX_CHUNK_BYTES,
    };
    let at_chunks = Request::Manifest {
        content_id: id(1),
        from_chunk: 0,
        max_chunks: MAX_CHUNKS_PER_REQUEST,
    };
    for r in [&at_bytes, &at_chunks] {
        r.validate().expect("the code must accept the ceiling itself");
        assert_valid(&encoded(r));
    }
}

#[test]
fn the_schema_refuses_an_unknown_field_just_as_the_parser_does() {
    // Defect 26 again: a field one side drops is a limit that does not exist.
    let mut req = encoded(&Request::Chunk {
        content_id: id(1),
        digest: id(2),
        offset: 0,
        max_bytes: 16,
    });
    req["max_byte"] = serde_json::json!(9);
    assert_invalid(&req, "a request with a misspelled field");
    assert!(content::decode::<Request>(req.to_string().as_bytes()).is_err());
}

#[test]
fn the_chunking_constant_the_schema_sees_is_the_one_the_store_writes() {
    // If ADR-0016's parameters are ever changed, this is where the wire notices.
    assert_eq!(otwono_store::CHUNKING_VERSION, "fastcdc-v2020-16k-64k-256k");
    let text = std::fs::read_to_string(schema_path()).unwrap();
    assert!(
        text.contains("64 KiB average"),
        "the schema should still explain the pagination in ADR-0016's terms"
    );
}

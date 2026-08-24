//! The ONM content-fetch protocol (ADR-0017).
//!
//! Two requests, both ranged, both scoped to an object, carried inside [`SecureChannel`]
//! frames once `Hello` has been exchanged.
//!
//! [`SecureChannel`]: crate::SecureChannel
//!
//! # Why every message is bounded by the *link*
//!
//! A Noise transport message holds at most 65519 bytes of plaintext, and ADR-0016 permits
//! chunks of 256 KiB. So even the smallest unit of content does not fit in one frame. Below
//! that, `BandwidthClass::max_reasonable_payload` is 256 bytes on LoRa. Fixing a message
//! size from the content would therefore produce a protocol that runs on exactly one
//! medium.
//!
//! Instead the requester asks for a range, the responder is free to return less, and the
//! requester loops. [`max_body_bytes`] and [`max_chunks_per_page`] turn a
//! [`LinkProperties`] into the two numbers a requester needs.
//!
//! # Why a chunk request names its object
//!
//! Asking for a chunk by digest alone is a probe: the store is content-addressed, so a peer
//! that guesses a digest learns whether this node holds those exact bytes — and chunks are
//! shared between objects, so a private object and a public one can contain the same one.
//! Naming the object means a responder can require the digest to be reachable from
//! something the peer is already allowed to have, and "do you hold chunk X" stops being a
//! question the wire can ask.
//!
//! # One refusal
//!
//! [`Response::NotAvailable`] is the only error. Absent, private, shared, damaged, and
//! not-part-of-that-object are all the same answer, because any finer distinction tells a
//! stranger what this node holds.

use crate::link::LinkProperties;
use serde::{Deserialize, Serialize};

/// Wire version. A breaking change to any message here bumps it.
pub const PROTOCOL_VERSION: &str = "1.0.0";

/// Largest plaintext one Noise transport message can carry: the 65535-byte message limit
/// less the 16-byte AEAD tag.
pub const MAX_NOISE_PLAINTEXT: usize = 65535 - 16;

/// Room reserved for the JSON envelope around a body — field names, the hex content id and
/// digest, and the numbers. Measured against the largest envelope this module can emit and
/// rounded up; [`envelope_fits_reserve`] keeps that honest.
pub const ENVELOPE_RESERVE: usize = 512;

/// Ceiling on one chunk, from ADR-0016's `MAX_CHUNK`. Duplicated rather than depended on:
/// this crate is the transport and must not pull in the store.
pub const MAX_CHUNK_BYTES: u32 = 256 * 1024;

/// Ceiling on a manifest window, whatever the link would allow. A responder builds the
/// page in memory, so an unbounded request is an allocation a peer chooses.
pub const MAX_CHUNKS_PER_REQUEST: u32 = 4096;

/// Bytes of JSON one [`ChunkEntry`] costs: 64 hex characters, the field names, the length,
/// and the punctuation. Deliberately generous — being wrong here means an over-long frame,
/// and [`chunk_entry_fits_estimate`] pins it.
const CHUNK_ENTRY_JSON_BYTES: usize = 112;

/// A request from a peer. Untrusted until [`Request::validate`] has passed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", deny_unknown_fields)]
pub enum Request {
    /// One window of an object's chunk list.
    #[serde(rename = "content.manifest")]
    Manifest {
        content_id: String,
        /// Index of the first chunk wanted.
        from_chunk: u32,
        /// How many chunks the requester can receive in one reply.
        max_chunks: u32,
    },
    /// One range of one chunk of one object.
    #[serde(rename = "content.chunk")]
    Chunk {
        content_id: String,
        /// Hex BLAKE3 digest, which must be in that object's chunk list.
        digest: String,
        /// Offset within the chunk.
        offset: u32,
        /// How many bytes the requester can receive in one reply.
        max_bytes: u32,
    },
}

/// One entry in a manifest window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkEntry {
    pub blake3: String,
    pub length: u32,
}

/// A window of an object's chunk list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestPage {
    pub content_id: String,
    pub size_bytes: u64,
    /// The chunking parameter set that produced this object (ADR-0016). A peer running
    /// different parameters can *detect* the mismatch, which is all it can do about it.
    pub chunking: String,
    /// Always `public` or `replicated` — nothing else may be served. Sent so a receiver
    /// knows whether it may re-serve what it caches.
    pub visibility: String,
    pub total_chunks: u32,
    pub from_chunk: u32,
    pub chunks: Vec<ChunkEntry>,
}

/// A range of one chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkPart {
    pub content_id: String,
    pub digest: String,
    pub offset: u32,
    /// Length of the whole chunk, so the requester knows when it is done.
    pub total_length: u32,
    /// Base64 of the bytes at `offset`. May be shorter than asked for; never longer.
    pub data: String,
}

/// A reply. `NotAvailable` is the only error, on purpose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", deny_unknown_fields)]
pub enum Response {
    #[serde(rename = "manifest")]
    Manifest(ManifestPage),
    #[serde(rename = "chunk")]
    Chunk(ChunkPart),
    /// Absent, refused, damaged, or not part of that object. One answer for all of them.
    #[serde(rename = "not_available")]
    NotAvailable { content_id: String },
}

impl Response {
    pub fn not_available(content_id: impl Into<String>) -> Response {
        Response::NotAvailable {
            content_id: content_id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    NotHex {
        field: &'static str,
    },
    ZeroLength {
        field: &'static str,
    },
    TooLarge {
        field: &'static str,
        asked: u64,
        ceiling: u64,
    },
    Malformed(String),
    /// The peer answered a different question than the one asked.
    Mismatched(String),
    /// A chunk arrived whose bytes do not hash to the digest the manifest gave.
    ChunkDigestMismatch {
        expected: String,
        actual: String,
    },
    /// The assembled chunk list does not produce the content id that was requested.
    ObjectIdMismatch {
        expected: String,
        actual: String,
    },
    /// The peer refused, or does not have it. Indistinguishable by design.
    NotAvailable(String),
    /// Two replies in a row moved nothing. A peer that will not make progress is a peer to
    /// give up on, not one to keep asking (see defect 29, `docs/build/VERIFICATION-LOG.md`).
    NoProgress,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::NotHex { field } => {
                write!(f, "{field} must be 64 lowercase hex characters")
            }
            ProtocolError::ZeroLength { field } => write!(f, "{field} must be greater than zero"),
            ProtocolError::TooLarge {
                field,
                asked,
                ceiling,
            } => {
                write!(f, "{field} of {asked} exceeds the ceiling of {ceiling}")
            }
            ProtocolError::Malformed(e) => write!(f, "malformed content message: {e}"),
            ProtocolError::Mismatched(e) => write!(f, "the peer answered a different request: {e}"),
            ProtocolError::ChunkDigestMismatch { expected, actual } => write!(
                f,
                "chunk does not hash to {expected}; the peer sent {actual} instead"
            ),
            ProtocolError::ObjectIdMismatch { expected, actual } => write!(
                f,
                "the chunk list the peer served describes {actual}, not the {expected} that was asked for"
            ),
            ProtocolError::NotAvailable(id) => {
                write!(f, "the peer will not serve {id}, or does not have it")
            }
            ProtocolError::NoProgress => write!(f, "the peer stopped making progress"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// 64 lowercase hex characters, and nothing else.
///
/// Strict rather than forgiving: a content id is compared byte for byte everywhere else,
/// so accepting uppercase here would produce two spellings of one object.
pub fn is_hex_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Request {
    /// The content id this request is about, whatever its shape.
    pub fn content_id(&self) -> &str {
        match self {
            Request::Manifest { content_id, .. } | Request::Chunk { content_id, .. } => content_id,
        }
    }

    /// Check a request from the wire before acting on any part of it.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if !is_hex_digest(self.content_id()) {
            return Err(ProtocolError::NotHex { field: "content_id" });
        }
        match self {
            Request::Manifest { max_chunks, .. } => {
                if *max_chunks == 0 {
                    return Err(ProtocolError::ZeroLength { field: "max_chunks" });
                }
                if *max_chunks > MAX_CHUNKS_PER_REQUEST {
                    return Err(ProtocolError::TooLarge {
                        field: "max_chunks",
                        asked: *max_chunks as u64,
                        ceiling: MAX_CHUNKS_PER_REQUEST as u64,
                    });
                }
            }
            Request::Chunk {
                digest, max_bytes, ..
            } => {
                if !is_hex_digest(digest) {
                    return Err(ProtocolError::NotHex { field: "digest" });
                }
                if *max_bytes == 0 {
                    return Err(ProtocolError::ZeroLength { field: "max_bytes" });
                }
                if *max_bytes > MAX_CHUNK_BYTES {
                    return Err(ProtocolError::TooLarge {
                        field: "max_bytes",
                        asked: *max_bytes as u64,
                        ceiling: MAX_CHUNK_BYTES as u64,
                    });
                }
            }
        }
        Ok(())
    }
}

/// How many raw content bytes fit in one reply on this link.
///
/// The smaller of what the link will carry and what a Noise frame holds, less the JSON
/// envelope, less base64's third. Always at least one byte: a `Trickle` link is slow, not
/// unusable, and a protocol that returned zero here would deadlock rather than crawl.
pub fn max_body_bytes(link: &LinkProperties) -> u32 {
    let frame = link
        .bandwidth_class
        .max_reasonable_payload()
        .min(MAX_NOISE_PLAINTEXT);
    let body = frame.saturating_sub(ENVELOPE_RESERVE);
    // Base64 costs four characters per three bytes.
    let raw = (body / 4) * 3;
    raw.clamp(1, MAX_CHUNK_BYTES as usize) as u32
}

/// How many chunk entries fit in one manifest window on this link. At least one.
pub fn max_chunks_per_page(link: &LinkProperties) -> u32 {
    let frame = link
        .bandwidth_class
        .max_reasonable_payload()
        .min(MAX_NOISE_PLAINTEXT);
    let body = frame.saturating_sub(ENVELOPE_RESERVE);
    let n = body / CHUNK_ENTRY_JSON_BYTES;
    n.clamp(1, MAX_CHUNKS_PER_REQUEST as usize) as u32
}

/// Encode a message for a channel frame.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    serde_json::to_vec(value).map_err(|e| ProtocolError::Malformed(e.to_string()))
}

/// Decode a message from a channel frame.
pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, ProtocolError> {
    serde_json::from_slice(bytes).map_err(|e| ProtocolError::Malformed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> String {
        std::iter::repeat_n(format!("{byte:02x}"), 32).collect()
    }

    fn manifest_request() -> Request {
        Request::Manifest {
            content_id: id(0xab),
            from_chunk: 0,
            max_chunks: 16,
        }
    }

    fn chunk_request() -> Request {
        Request::Chunk {
            content_id: id(0xab),
            digest: id(0xcd),
            offset: 0,
            max_bytes: 4096,
        }
    }

    #[test]
    fn requests_round_trip_through_their_wire_names() {
        for r in [manifest_request(), chunk_request()] {
            let json = String::from_utf8(encode(&r).unwrap()).unwrap();
            assert!(json.contains("content."), "{json}");
            assert_eq!(decode::<Request>(json.as_bytes()).unwrap(), r);
        }
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        // The lesson of defect 26: a field the parser drops is a limit that does not exist.
        let json = format!(
            r#"{{"method":"content.chunk","content_id":"{}","digest":"{}","offset":0,"max_bytes":16,"max_byte":9}}"#,
            id(0xab),
            id(0xcd)
        );
        assert!(decode::<Request>(json.as_bytes()).is_err());
    }

    #[test]
    fn a_content_id_that_is_not_hex_is_refused() {
        let bad = Request::Manifest {
            content_id: "not-a-content-id".into(),
            from_chunk: 0,
            max_chunks: 1,
        };
        assert_eq!(bad.validate(), Err(ProtocolError::NotHex { field: "content_id" }));
    }

    #[test]
    fn an_uppercase_digest_is_refused_because_ids_are_compared_bytewise() {
        assert!(!is_hex_digest(&id(0xAB).to_uppercase()));
        assert!(is_hex_digest(&id(0xab)));
    }

    #[test]
    fn a_chunk_request_larger_than_the_largest_legal_chunk_is_refused() {
        let bad = Request::Chunk {
            content_id: id(1),
            digest: id(2),
            offset: 0,
            max_bytes: MAX_CHUNK_BYTES + 1,
        };
        assert!(matches!(
            bad.validate(),
            Err(ProtocolError::TooLarge {
                field: "max_bytes",
                ..
            })
        ));
    }

    #[test]
    fn a_zero_length_request_is_refused_because_it_can_never_make_progress() {
        let zero_bytes = Request::Chunk {
            content_id: id(1),
            digest: id(2),
            offset: 0,
            max_bytes: 0,
        };
        let zero_chunks = Request::Manifest {
            content_id: id(1),
            from_chunk: 0,
            max_chunks: 0,
        };
        assert!(matches!(
            zero_bytes.validate(),
            Err(ProtocolError::ZeroLength { .. })
        ));
        assert!(matches!(
            zero_chunks.validate(),
            Err(ProtocolError::ZeroLength { .. })
        ));
    }

    #[test]
    fn a_manifest_window_larger_than_the_ceiling_is_refused() {
        let bad = Request::Manifest {
            content_id: id(1),
            from_chunk: 0,
            max_chunks: MAX_CHUNKS_PER_REQUEST + 1,
        };
        assert!(matches!(
            bad.validate(),
            Err(ProtocolError::TooLarge {
                field: "max_chunks",
                ..
            })
        ));
    }

    #[test]
    fn valid_requests_pass() {
        manifest_request().validate().unwrap();
        chunk_request().validate().unwrap();
    }

    #[test]
    fn a_lora_link_gets_a_smaller_body_than_ethernet_but_never_zero() {
        let lora = max_body_bytes(&LinkProperties::lora_eu868());
        let internet = max_body_bytes(&LinkProperties::internet());
        assert!(lora >= 1, "a Trickle link must still make progress");
        assert!(lora < internet, "lora {lora} internet {internet}");
    }

    #[test]
    fn a_body_never_exceeds_what_one_noise_frame_can_hold() {
        // The Wide class permits 64 MiB; the frame does not care what the class permits.
        let mut wide = LinkProperties::internet();
        wide.bandwidth_class = crate::link::BandwidthClass::Wide;
        let body = max_body_bytes(&wide) as usize;
        assert!(body <= MAX_CHUNK_BYTES as usize);
        // base64 of the body, plus the envelope, must still fit one frame.
        assert!(body.div_ceil(3) * 4 + ENVELOPE_RESERVE <= MAX_NOISE_PLAINTEXT);
    }

    #[test]
    fn a_manifest_window_is_at_least_one_entry_on_every_link() {
        for p in [LinkProperties::lora_eu868(), LinkProperties::internet()] {
            assert!(max_chunks_per_page(&p) >= 1, "{:?}", p.kind);
        }
    }

    #[test]
    fn chunk_entry_fits_estimate() {
        // Pins CHUNK_ENTRY_JSON_BYTES to reality: if the struct grows a field, this fails
        // rather than silently producing over-long frames.
        let entry = ChunkEntry {
            blake3: id(0xff),
            length: u32::MAX,
        };
        let encoded = serde_json::to_vec(&entry).unwrap().len() + 1; // + the array comma
        assert!(
            encoded <= CHUNK_ENTRY_JSON_BYTES,
            "one entry encodes to {encoded} bytes, over the {CHUNK_ENTRY_JSON_BYTES} reserved"
        );
    }

    #[test]
    fn envelope_fits_reserve() {
        // The largest envelope this module emits, with an empty body.
        let page = ManifestPage {
            content_id: id(0xff),
            size_bytes: u64::MAX,
            chunking: "fastcdc-v2020-16k-64k-256k".into(),
            visibility: "replicated".into(),
            total_chunks: u32::MAX,
            from_chunk: u32::MAX,
            chunks: vec![],
        };
        let part = ChunkPart {
            content_id: id(0xff),
            digest: id(0xff),
            offset: u32::MAX,
            total_length: u32::MAX,
            data: String::new(),
        };
        for (name, len) in [
            ("manifest", encode(&Response::Manifest(page)).unwrap().len()),
            ("chunk", encode(&Response::Chunk(part)).unwrap().len()),
        ] {
            assert!(
                len <= ENVELOPE_RESERVE,
                "the {name} envelope is {len} bytes, over the {ENVELOPE_RESERVE} reserved"
            );
        }
    }

    #[test]
    fn every_refusal_is_the_same_refusal() {
        // Two different reasons, one wire form. If these ever differ, a peer can tell
        // "private" from "absent".
        let absent = Response::not_available(id(1));
        let private = Response::not_available(id(1));
        assert_eq!(encode(&absent).unwrap(), encode(&private).unwrap());
    }

    #[test]
    fn a_refusal_round_trips() {
        let r = Response::not_available(id(7));
        let bytes = encode(&r).unwrap();
        assert_eq!(decode::<Response>(&bytes).unwrap(), r);
    }

    #[test]
    fn responses_round_trip() {
        let page = Response::Manifest(ManifestPage {
            content_id: id(1),
            size_bytes: 4096,
            chunking: "fastcdc-v2020-16k-64k-256k".into(),
            visibility: "public".into(),
            total_chunks: 1,
            from_chunk: 0,
            chunks: vec![ChunkEntry {
                blake3: id(2),
                length: 4096,
            }],
        });
        let part = Response::Chunk(ChunkPart {
            content_id: id(1),
            digest: id(2),
            offset: 0,
            total_length: 4096,
            data: data_encoding::BASE64.encode(b"hello"),
        });
        for r in [page, part] {
            assert_eq!(decode::<Response>(&encode(&r).unwrap()).unwrap(), r);
        }
    }
}

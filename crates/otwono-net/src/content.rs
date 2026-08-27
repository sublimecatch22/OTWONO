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

/// A Noise transport message costs 16 bytes of AEAD tag on top of its plaintext, and the
/// *link* budget is spent on the frame, not the plaintext.
pub const NOISE_TAG_BYTES: usize = 16;

/// Room for a chunk reply's JSON envelope with an empty body. Measured at 229 bytes for the
/// widest possible numbers and two 64-character hex ids; `envelope_fits_reserve` pins it.
pub const CHUNK_ENVELOPE_RESERVE: usize = 232;

/// Room for a manifest reply's envelope with no entries, worst case.
///
/// The worst case is a `shared` object, whose envelope also carries the recipient's sealed
/// content key: a NodeID, two base64 blobs and a nonce prefix. Measured at 646 against 262
/// for a public one. One constant rather than two because the *requester* does not know an
/// object is shared until the first manifest arrives, so sizing the window by the smaller
/// number would mean discovering the reply does not fit after asking for it.
/// `envelope_fits_reserve` measures both and pins this.
pub const MANIFEST_ENVELOPE_RESERVE: usize = 664;

/// Ceiling on one chunk, from ADR-0016's `MAX_CHUNK`. Duplicated rather than depended on:
/// this crate is the transport and must not pull in the store.
pub const MAX_CHUNK_BYTES: u32 = 256 * 1024;

/// Ceiling on a manifest window, whatever the link would allow. A responder builds the
/// page in memory, so an unbounded request is an allocation a peer chooses.
pub const MAX_CHUNKS_PER_REQUEST: u32 = 4096;

/// Bytes of JSON one [`ChunkEntry`] costs: 64 hex characters, the field names, the length,
/// and the punctuation. Measured at 98; `chunk_entry_fits_estimate` pins it. Being wrong
/// here means an over-long frame, so it rounds up.
const CHUNK_ENTRY_JSON_BYTES: usize = 104;

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
    /// What has this node sealed to me? (ADR-0020)
    ///
    /// There is no field naming the asker. It is the NodeID the handshake authenticated, and
    /// a peer that could ask on somebody else's behalf would be asking a different and much
    /// worse question — an enumeration oracle for the whole recipient graph.
    #[serde(rename = "content.shared_with_me")]
    SharedWithMe {
        /// Continue after this content id. Absent starts at the beginning.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
        /// How many entries the requester can receive in one reply.
        max_entries: u32,
    },
    /// What REPLICATED content do you hold that I could take a copy of? (ADR-0026 §7)
    ///
    /// Unlike `SharedWithMe` the answer is the same for every asker, because `REPLICATED`
    /// means "explicitly permitted to be copied" and is not scoped to a recipient. There is
    /// nothing to filter and so nothing for a filter to get wrong.
    #[serde(rename = "content.replicable")]
    Replicable {
        /// Continue after this content id. Absent starts at the beginning.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
        /// How many entries the requester can receive in one reply.
        max_entries: u32,
    },
    /// What does this name point at right now? (ADR-0027)
    ///
    /// The second primitive on the wire. There is no `node_id` field, and that is the
    /// decision: a peer answers only for **itself**, so the owner is the NodeID the Noise
    /// handshake authenticated and the key that verifies the answer is the one the handshake
    /// proved. Asking a peer for somebody else's pointer would need that somebody's public
    /// key from a third place, and would reintroduce the caching question ADR-0027 left
    /// open — a cached pointer is a rollback risk with a friendly face.
    #[serde(rename = "content.pointer")]
    Pointer {
        /// Which namespace: `wiki`, `profile`, `forum`.
        service: String,
        /// The path within it.
        name: String,
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
    /// `public`, `replicated`, or `shared` — nothing else may be served. Sent so a receiver
    /// knows whether it may re-serve what it caches, and `shared` is exactly the case where
    /// it may not.
    pub visibility: String,
    /// Present when and only when `visibility` is `shared` (ADR-0019 §4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharedEnvelope>,
    pub total_chunks: u32,
    pub from_chunk: u32,
    pub chunks: Vec<ChunkEntry>,
}

impl ManifestPage {
    /// Check the label and the envelope agree, from the receiving side.
    ///
    /// A `shared` manifest with no envelope is bytes the receiver could never open, and a
    /// non-`shared` one carrying an envelope is a peer describing an object in two
    /// contradictory ways. Both are refused rather than reconciled.
    ///
    /// `me` is the receiver's own NodeID: a sealed key addressed to somebody else cannot be
    /// opened here, so accepting it would mean downloading an object to fail at the end.
    pub fn sharing_is_consistent(&self, me: &str) -> Result<(), ProtocolError> {
        match (self.visibility.as_str(), &self.sharing) {
            ("shared", None) => Err(ProtocolError::Mismatched(format!(
                "{} is offered as shared with no sealed key, so it could never be opened",
                self.content_id
            ))),
            ("shared", Some(envelope)) if envelope.sealed_key.recipient != me => {
                Err(ProtocolError::Mismatched(format!(
                    "{} came with a key sealed to {}, not to this node",
                    self.content_id, envelope.sealed_key.recipient
                )))
            }
            (label, Some(_)) if label != "shared" => Err(ProtocolError::Mismatched(format!(
                "{} is offered as {label:?} and also carries a sealed content key",
                self.content_id
            ))),
            _ => Ok(()),
        }
    }
}

/// One object a peer may ask for, in the answer to "what have you sealed to me?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedIndexEntry {
    pub content_id: String,
    /// What the object becomes once opened, so a recipient can decide whether to fetch now.
    /// The manifest's `size_bytes` measures ciphertext and is a different number.
    pub plaintext_size_bytes: u64,
}

/// One object a node is willing to have copied (ADR-0026 §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicableEntry {
    pub content_id: String,
    pub size_bytes: u64,
    /// How long a holder should keep it before dropping it unless re-offered.
    pub ttl_days: u32,
    /// The owner's size cap. A holder checks this *and* its own budget.
    pub max_size_bytes: u64,
    /// Whether a holder may offer it onward. A request rather than a control (ADR-0026 §5).
    pub allow_rereplication: bool,
}

/// One pointer record, as the owner signed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointerReply {
    /// The record, verbatim. Carried as a JSON value rather than a typed struct so this
    /// crate does not have to depend on `otwono-pointer` — the transport's job is to deliver
    /// it unaltered, and a type here would mean a record this build did not understand could
    /// not even be relayed.
    pub record: serde_json::Value,
}

/// A page of what one node is willing to have copied (ADR-0026 §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicablePage {
    pub entries: Vec<ReplicableEntry>,
}

/// A page of what one node has sealed to the peer asking (ADR-0020).
///
/// Ordered by content id, so paging is stable and needs no timestamp — sharing time is
/// metadata this system does not record and a recipient has not asked for.
///
/// An empty page from a node that has sealed nothing to this peer is identical to an empty
/// page from a node that shares with nobody. That is deliberate: asking must not be a way to
/// find out whether a node shares at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedIndexPage {
    pub entries: Vec<SharedIndexEntry>,
}

/// Ceiling on one index page, whatever the link would allow. A responder builds the page in
/// memory, so an unbounded request is an allocation a peer chooses.
pub const MAX_SHARED_ENTRIES_PER_REQUEST: u32 = 256;

/// Longest pointer name this protocol will carry, matching `pointer.schema.json`.
pub const MAX_POINTER_NAME_BYTES: usize = 512;

/// Bytes of JSON one [`SharedIndexEntry`] costs: 64 hex characters, the field names, a size
/// and the punctuation. Measured at 125 for the widest possible size;
/// `shared_entry_fits_estimate` pins it. Being wrong here means an over-long frame, so it
/// rounds up — and the first guess here was 124, which the measurement rejected by one byte.
const SHARED_ENTRY_JSON_BYTES: usize = 132;

/// Room for an index reply's envelope with no entries. Measured at 44.
pub const SHARED_INDEX_ENVELOPE_RESERVE: usize = 64;

/// How many index entries fit in one reply on this link.
pub fn max_shared_entries_per_page(link: &LinkProperties) -> u32 {
    let body = plaintext_budget(link).saturating_sub(SHARED_INDEX_ENVELOPE_RESERVE);
    let n = body / SHARED_ENTRY_JSON_BYTES;
    n.clamp(1, MAX_SHARED_ENTRIES_PER_REQUEST as usize) as u32
}

/// What a recipient needs to open a `SHARED` object it has fetched (ADR-0019).
///
/// **Only the asking peer's own copy of the content key travels.** A recipient learns that
/// it may open the object and nothing about who else may — which is a real, if partial,
/// answer to OQ-28 on the wire, even though the object record on the serving node still
/// holds the whole list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedEnvelope {
    /// The scheme the object was sealed with. A value the receiver does not recognise must
    /// be refused, never guessed at.
    pub encryption: String,
    /// Base64 nonce prefix.
    pub nonce_prefix: String,
    /// What the object will be once opened. The manifest's `size_bytes` measures ciphertext.
    pub plaintext_size_bytes: u64,
    /// This peer's copy of the content key, and no other.
    pub sealed_key: otwono_identity::SealedKey,
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
    #[serde(rename = "shared_with_you")]
    SharedWithYou(SharedIndexPage),
    /// `replicable`, not `content.replicable`: that is the *request*'s method name, and a
    /// reply tagged with a method name reads as one. Every other variant here is a bare
    /// noun, and the schema is where the inconsistency showed up.
    #[serde(rename = "replicable")]
    Replicable(ReplicablePage),
    /// The signed record itself, passed through unexamined by the transport.
    ///
    /// Deliberately opaque here: `otwono-net` moves bytes and must not become a second place
    /// that knows how a pointer verifies. The asker checks it against the key the handshake
    /// proved, in `otwono-netd`.
    #[serde(rename = "pointer")]
    Pointer(PointerReply),
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
    /// The content id this request is about, when it is about one.
    ///
    /// `None` for [`Request::SharedWithMe`], which asks *which* objects rather than about a
    /// named one. An `Option` rather than an empty string because the difference decides how
    /// a refusal is shaped: a request about an object is refused with `not_available` naming
    /// that object, and one that asks which objects is refused with an **empty page** — the
    /// same answer a peer with nothing gets, so asking cannot distinguish the two
    /// (ADR-0020).
    pub fn content_id(&self) -> Option<&str> {
        match self {
            Request::Manifest { content_id, .. } | Request::Chunk { content_id, .. } => Some(content_id),
            // A pointer request names no content id either — it asks what a *name* points
            // at, and the id is the answer rather than the question.
            Request::SharedWithMe { .. } | Request::Replicable { .. } | Request::Pointer { .. } => None,
        }
    }

    /// Check a request from the wire before acting on any part of it.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if let Some(id) = self.content_id() {
            if !is_hex_digest(id) {
                return Err(ProtocolError::NotHex { field: "content_id" });
            }
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
            // Same bounds for both index requests: they are the same shape of question and
            // a divergence would be an accident rather than a decision.
            Request::Pointer { service, name } => {
                // Bounded here rather than at the store: these strings arrive from a
                // stranger and become a lookup key. The limits match the pointer schema's.
                if service.is_empty() || service.len() > 32 {
                    return Err(ProtocolError::TooLarge {
                        field: "service",
                        asked: service.len() as u64,
                        ceiling: 32,
                    });
                }
                if name.is_empty() {
                    return Err(ProtocolError::ZeroLength { field: "name" });
                }
                if name.len() > MAX_POINTER_NAME_BYTES {
                    return Err(ProtocolError::TooLarge {
                        field: "name",
                        asked: name.len() as u64,
                        ceiling: MAX_POINTER_NAME_BYTES as u64,
                    });
                }
            }
            Request::SharedWithMe { after, max_entries } | Request::Replicable { after, max_entries } => {
                if let Some(after) = after {
                    if !is_hex_digest(after) {
                        return Err(ProtocolError::NotHex { field: "after" });
                    }
                }
                if *max_entries == 0 {
                    return Err(ProtocolError::ZeroLength { field: "max_entries" });
                }
                if *max_entries > MAX_SHARED_ENTRIES_PER_REQUEST {
                    return Err(ProtocolError::TooLarge {
                        field: "max_entries",
                        asked: *max_entries as u64,
                        ceiling: MAX_SHARED_ENTRIES_PER_REQUEST as u64,
                    });
                }
            }
        }
        Ok(())
    }
}

/// Plaintext this link can carry in one message: what the link will bear, less the AEAD
/// tag, and never more than a Noise transport message holds.
fn plaintext_budget(link: &LinkProperties) -> usize {
    link.bandwidth_class
        .max_reasonable_payload()
        .saturating_sub(NOISE_TAG_BYTES)
        .min(MAX_NOISE_PLAINTEXT)
}

/// How many raw content bytes fit in one chunk reply on this link.
///
/// Always at least one byte. On a `Trickle` link this is single digits, which is not useful
/// but is correct — and correct at six bytes a transmission is what a duty-cycle-limited
/// radio actually offers.
pub fn max_body_bytes(link: &LinkProperties) -> u32 {
    let body = plaintext_budget(link).saturating_sub(CHUNK_ENVELOPE_RESERVE);
    // Base64 costs four characters per three bytes.
    let raw = (body / 4) * 3;
    raw.clamp(1, MAX_CHUNK_BYTES as usize) as u32
}

/// How many chunk entries fit in one manifest window on this link.
///
/// Reports at least one so a caller never builds an empty request, which means the answer
/// can be a window this link cannot actually carry. Ask [`carries_a_manifest`] first.
pub fn max_chunks_per_page(link: &LinkProperties) -> u32 {
    let body = plaintext_budget(link).saturating_sub(MANIFEST_ENVELOPE_RESERVE);
    let n = body / CHUNK_ENTRY_JSON_BYTES;
    n.clamp(1, MAX_CHUNKS_PER_REQUEST as usize) as u32
}

/// Can this link carry a manifest window with even one entry in it?
///
/// **Measured, and the answer for a `Trickle` link is no.** A manifest reply's envelope is
/// 262 bytes before any entry — 646 if the object is shared, which is the number the reserve
/// is sized for — and one entry is 98 more; EU868 LoRa will bear 256 in total.
/// Chunk replies do fit, so an object could be *transferred* over such a link — but not
/// described over one, so a fetch cannot begin. Saying so before sending anything is better
/// than a `PayloadTooLarge` from three layers down.
///
/// The secure channel has the same problem first: a session proof frame is 447 bytes
/// (measured), so a Noise handshake does not complete over a `Trickle` link either. See
/// OQ-23 and OQ-24.
pub fn carries_a_manifest(link: &LinkProperties) -> bool {
    plaintext_budget(link) >= MANIFEST_ENVELOPE_RESERVE + CHUNK_ENTRY_JSON_BYTES
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
    fn a_chunk_reply_sized_for_a_link_fits_that_link() {
        // The property the sizing exists for, checked by building the reply rather than by
        // trusting the arithmetic. Includes LoRa, where the margin is a handful of bytes.
        for link in [LinkProperties::lora_eu868(), LinkProperties::internet()] {
            let body = max_body_bytes(&link) as usize;
            let reply = Response::Chunk(ChunkPart {
                content_id: id(0xff),
                digest: id(0xff),
                offset: u32::MAX,
                total_length: u32::MAX,
                data: data_encoding::BASE64.encode(&vec![0u8; body]),
            });
            let frame = encode(&reply).unwrap().len() + NOISE_TAG_BYTES;
            link.permits_payload(frame)
                .unwrap_or_else(|e| panic!("a reply sized for a {:?} link does not fit it: {e}", link.kind));
            assert!(frame - NOISE_TAG_BYTES <= MAX_NOISE_PLAINTEXT);
        }
    }

    #[test]
    fn a_trickle_link_cannot_carry_a_manifest_window() {
        // Measured, not assumed: a manifest reply is 262 bytes before any entry and EU868
        // LoRa bears 256. The protocol must say so rather than fail somewhere below.
        assert!(!carries_a_manifest(&LinkProperties::lora_eu868()));
        assert!(carries_a_manifest(&LinkProperties::internet()));
    }

    #[test]
    fn a_manifest_window_sized_for_a_link_that_can_carry_one_fits_it() {
        let link = LinkProperties::internet();
        let n = max_chunks_per_page(&link) as usize;
        let reply = Response::Manifest(ManifestPage {
            content_id: id(0xff),
            size_bytes: u64::MAX,
            chunking: "fastcdc-v2020-16k-64k-256k".into(),
            visibility: "replicated".into(),
            sharing: None,
            total_chunks: u32::MAX,
            from_chunk: u32::MAX,
            chunks: vec![
                ChunkEntry {
                    blake3: id(0xff),
                    length: u32::MAX,
                };
                n
            ],
        });
        let frame = encode(&reply).unwrap().len() + NOISE_TAG_BYTES;
        link.permits_payload(frame).expect("a full window must fit");
        assert!(frame - NOISE_TAG_BYTES <= MAX_NOISE_PLAINTEXT, "{frame}");
    }

    #[test]
    fn a_body_never_exceeds_what_one_noise_frame_can_hold() {
        // The Wide class permits 64 MiB; the frame does not care what the class permits.
        let mut wide = LinkProperties::internet();
        wide.bandwidth_class = crate::link::BandwidthClass::Wide;
        let body = max_body_bytes(&wide) as usize;
        assert!(body <= MAX_CHUNK_BYTES as usize);
        assert!(body.div_ceil(3) * 4 + CHUNK_ENVELOPE_RESERVE <= MAX_NOISE_PLAINTEXT);
    }

    #[test]
    fn a_manifest_window_is_never_reported_as_zero_entries() {
        // Even where the window will not fit: a zero would produce a request that
        // `validate` refuses. `carries_a_manifest` is the check for that case.
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
        // The two reserves are measured numbers, and this is the measurement. If a field is
        // added to either reply, this fails rather than a link silently refusing a frame.
        let page = ManifestPage {
            content_id: id(0xff),
            size_bytes: u64::MAX,
            chunking: "fastcdc-v2020-16k-64k-256k".into(),
            visibility: "replicated".into(),
            sharing: None,
            total_chunks: u32::MAX,
            from_chunk: u32::MAX,
            chunks: vec![],
        };
        // The worst case: a shared object, whose envelope also carries a sealed key. The
        // strings here are as long as the real ones can be — a full NodeID, a 32-byte
        // public key and a 48-byte ciphertext in base64.
        let shared = ManifestPage {
            content_id: id(0xff),
            size_bytes: u64::MAX,
            chunking: "fastcdc-v2020-16k-64k-256k".into(),
            visibility: "shared".into(),
            sharing: Some(SharedEnvelope {
                encryption: "xchacha20poly1305-stream-be32-1MiB".into(),
                nonce_prefix: "A".repeat(28),
                plaintext_size_bytes: u64::MAX,
                sealed_key: otwono_identity::SealedKey {
                    recipient: format!("otw1{}", "z".repeat(56)),
                    ephemeral_public_key: "A".repeat(44),
                    sealed: "A".repeat(64),
                },
            }),
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
        for (name, len, reserve) in [
            (
                "manifest",
                encode(&Response::Manifest(page)).unwrap().len(),
                MANIFEST_ENVELOPE_RESERVE,
            ),
            (
                "shared manifest",
                encode(&Response::Manifest(shared)).unwrap().len(),
                MANIFEST_ENVELOPE_RESERVE,
            ),
            (
                "chunk",
                encode(&Response::Chunk(part)).unwrap().len(),
                CHUNK_ENVELOPE_RESERVE,
            ),
        ] {
            assert!(
                len <= reserve,
                "the {name} envelope is {len} bytes, over the {reserve} reserved"
            );
        }
    }

    #[test]
    fn shared_entry_fits_estimate() {
        // The same discipline as chunk_entry_fits_estimate: if an entry ever grows past what
        // the page arithmetic assumes, a reply sized by that arithmetic is over-long and the
        // link refuses it.
        let entry = SharedIndexEntry {
            content_id: id(0xff),
            plaintext_size_bytes: u64::MAX,
        };
        let len = serde_json::to_vec(&entry).unwrap().len();
        assert!(
            len <= SHARED_ENTRY_JSON_BYTES,
            "a shared index entry is {len} bytes, over the {SHARED_ENTRY_JSON_BYTES} assumed"
        );

        let empty = encode(&Response::SharedWithYou(SharedIndexPage { entries: vec![] }))
            .unwrap()
            .len();
        assert!(
            empty <= SHARED_INDEX_ENVELOPE_RESERVE,
            "the index envelope is {empty} bytes, over the {SHARED_INDEX_ENVELOPE_RESERVE} reserved"
        );
    }

    #[test]
    fn an_index_page_sized_for_a_link_fits_it() {
        let link = LinkProperties::internet();
        let n = max_shared_entries_per_page(&link) as usize;
        let reply = Response::SharedWithYou(SharedIndexPage {
            entries: vec![
                SharedIndexEntry {
                    content_id: id(0xff),
                    plaintext_size_bytes: u64::MAX,
                };
                n
            ],
        });
        let len = encode(&reply).unwrap().len() + NOISE_TAG_BYTES;
        let bears = link.bandwidth_class.max_reasonable_payload();
        assert!(
            len <= bears,
            "a page sized for this link is {len} bytes and the link bears {bears}"
        );
    }

    #[test]
    fn an_index_request_names_nobody() {
        // The property the whole design rests on: a peer cannot ask what somebody *else* has
        // been sent. If a field for it ever appears, this fails.
        let request = Request::SharedWithMe {
            after: None,
            max_entries: 10,
        };
        let json = String::from_utf8(encode(&request).unwrap()).unwrap();
        assert!(!json.contains("peer"), "{json}");
        assert!(!json.contains("node_id"), "{json}");
        assert!(!json.contains("recipient"), "{json}");
        assert_eq!(request.content_id(), None, "it is not about one object");
    }

    #[test]
    fn an_index_request_is_bounded_like_every_other() {
        assert!(matches!(
            Request::SharedWithMe {
                after: None,
                max_entries: 0
            }
            .validate(),
            Err(ProtocolError::ZeroLength { .. })
        ));
        assert!(matches!(
            Request::SharedWithMe {
                after: None,
                max_entries: MAX_SHARED_ENTRIES_PER_REQUEST + 1
            }
            .validate(),
            Err(ProtocolError::TooLarge { .. })
        ));
        assert!(matches!(
            Request::SharedWithMe {
                after: Some("not a digest".into()),
                max_entries: 10
            }
            .validate(),
            Err(ProtocolError::NotHex { field: "after" })
        ));
        assert!(Request::SharedWithMe {
            after: Some(id(3)),
            max_entries: 10
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn a_request_fits_every_link_a_reply_does() {
        // Requests are smaller than replies, but not by so much that it can be assumed.
        let biggest = [
            encode(&Request::Chunk {
                content_id: id(0xff),
                digest: id(0xff),
                offset: u32::MAX,
                max_bytes: MAX_CHUNK_BYTES,
            })
            .unwrap()
            .len(),
            encode(&Request::Manifest {
                content_id: id(0xff),
                from_chunk: u32::MAX,
                max_chunks: MAX_CHUNKS_PER_REQUEST,
            })
            .unwrap()
            .len(),
        ];
        for len in biggest {
            LinkProperties::lora_eu868()
                .permits_payload(len + NOISE_TAG_BYTES)
                .unwrap_or_else(|e| panic!("a request of {len} bytes will not fit LoRa: {e}"));
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
            sharing: None,
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

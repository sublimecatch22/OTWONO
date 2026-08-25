//! What an object is, and what its name means.
//!
//! # Identity is the content, and nothing else
//!
//! An object's [`ContentId`] is derived from its chunk list alone — not from its label, its
//! filename, its timestamps, or who stored it. Two people who store the same bytes get the
//! same id even if one marks it `Public` and the other `Private`.
//!
//! That is deliberate and it is what makes the neighbourhood cache work: a peer holding a
//! chunk is interchangeable with any other peer holding it, and a fetch can be verified
//! against a name computed independently. Folding metadata into the identity would mean two
//! nodes with the same file could not recognise it as the same file.
//!
//! It also means a `ContentId` **reveals that you hold particular bytes** to anyone who can
//! guess them. That is the privacy cost ADR-0015 names as "holding is publishing", and it is
//! why `Private` objects never enter any shared index.

use crate::chunk::{ChunkRef, CHUNKING_VERSION};
use crate::label::Visibility;
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: &str = "1.0.0";

/// Domain separator, so a content id can never collide with a chunk digest or with anything
/// else this system hashes. Every hashed structure in OTWONO has its own prefix.
const OBJECT_DOMAIN: &[u8] = b"otwono-object-v1:";

/// The name of an object: a hash over what it contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentId([u8; 32]);

impl ContentId {
    /// Compute the id from a chunk list.
    ///
    /// The length of each chunk is folded in as well as its digest. Without that, a
    /// different split of the same bytes could produce the same id — which cannot happen at
    /// fixed chunking parameters, but relying on that would make the id's meaning depend on
    /// a constant rather than on the arithmetic.
    pub fn of(chunks: &[ChunkRef]) -> ContentId {
        let mut h = blake3::Hasher::new();
        h.update(OBJECT_DOMAIN);
        h.update(CHUNKING_VERSION.as_bytes());
        h.update(b"\0");
        h.update(&(chunks.len() as u64).to_le_bytes());
        for c in chunks {
            h.update(&c.digest);
            h.update(&c.length.to_le_bytes());
        }
        ContentId(*h.finalize().as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(s: &str) -> Option<ContentId> {
        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(ContentId(out))
    }
}

impl std::fmt::Display for ContentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for ContentId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ContentId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<ContentId, D::Error> {
        let s = String::deserialize(d)?;
        ContentId::from_hex(&s).ok_or_else(|| serde::de::Error::custom(format!("not a content id: {s:?}")))
    }
}

/// A chunk as it appears in a stored record: hex digest and length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredChunk {
    pub blake3: String,
    pub length: u32,
}

impl From<&ChunkRef> for StoredChunk {
    fn from(c: &ChunkRef) -> StoredChunk {
        StoredChunk {
            blake3: c.hex(),
            length: c.length,
        }
    }
}

/// How a `SHARED` object's bytes were sealed, and who can open them (ADR-0019).
///
/// Present on any object whose chunks are ciphertext, whatever its current label. It is
/// **not** removed when an object is demoted: the bytes on disk are still sealed, and
/// dropping the envelope would leave the owner unable to read their own object.
///
/// None of this is part of the [`ContentId`], because the id is over the chunk list and
/// nothing else. A peer that substitutes a nonce prefix or a recipient's copy therefore
/// produces an object that still verifies as the right bytes and then fails to open. That
/// is a denial, not a disclosure: the ciphertext is covered by the id and each frame is
/// authenticated, so no substitution yields *different* plaintext.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sharing {
    /// The scheme that produced the ciphertext. A value this build does not recognise must
    /// be refused, never guessed at.
    pub encryption: String,
    /// Base64 nonce prefix, 19 bytes. Fresh per object.
    pub nonce_prefix: String,
    /// What the object was before it was sealed. Recorded because the chunk lengths measure
    /// ciphertext, and a caller wants to know how much file to expect.
    pub plaintext_size_bytes: u64,
    /// The content key, sealed once per recipient. This list *is* the authorized set:
    /// keeping a separate `authorized.nodes` alongside it would create two facts that can
    /// disagree, and the disagreement would be a security bug in whichever direction it
    /// went. That the list names who a node shares with is **OQ-28**, unsolved.
    pub sealed_keys: Vec<otwono_identity::SealedKey>,
}

impl Sharing {
    /// The NodeIDs that can open this object, in text form.
    pub fn authorized_nodes(&self) -> Vec<&str> {
        self.sealed_keys.iter().map(|k| k.recipient.as_str()).collect()
    }

    /// Whether this node may open the object — by name only. Holding the copy is what
    /// decides in the end; this is the check a serving daemon can make before doing work.
    pub fn names(&self, node_id: &str) -> bool {
        self.sealed_keys.iter().any(|k| k.recipient == node_id)
    }

    /// This recipient's copy of the content key, if there is one.
    pub fn copy_for(&self, node_id: &str) -> Option<&otwono_identity::SealedKey> {
        self.sealed_keys.iter().find(|k| k.recipient == node_id)
    }

    /// Check the envelope is usable before anything relies on it.
    ///
    /// An envelope with no recipients is not a shared object, it is an object nobody can
    /// open — including whoever sealed it. Refusing at the record is better than
    /// discovering it when someone tries to read.
    pub fn validate(&self) -> Result<(), ObjectError> {
        if self.encryption != crate::shared::SHARED_ENCRYPTION {
            return Err(ObjectError::BadEnvelope(format!(
                "encrypted as {:?}, which this build does not implement",
                self.encryption
            )));
        }
        if self.sealed_keys.is_empty() {
            return Err(ObjectError::BadEnvelope(
                "no sealed keys, so nobody can open it".to_string(),
            ));
        }
        let prefix = data_encoding::BASE64
            .decode(self.nonce_prefix.as_bytes())
            .map_err(|e| ObjectError::BadEnvelope(format!("the nonce prefix is not base64: {e}")))?;
        crate::shared::decode_prefix(&prefix).map_err(|e| ObjectError::BadEnvelope(e.to_string()))?;

        // Two copies for one recipient is either a duplicate or two different keys under
        // one name, and there is no way to tell which. Refuse rather than pick.
        let mut seen: Vec<&str> = self.sealed_keys.iter().map(|k| k.recipient.as_str()).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        if seen.len() != before {
            return Err(ObjectError::BadEnvelope(
                "the same recipient appears twice, and there is no way to tell which copy \
                 is meant"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

/// Everything the store knows about one object.
///
/// The label lives here rather than in the identity, so relabelling an object does not
/// rename it — and so two nodes holding the same bytes under different labels still
/// recognise them as the same bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Object {
    pub schema_version: String,
    pub content_id: ContentId,
    /// Which chunking rules produced this. Recorded so a node can *detect* an object split
    /// under different parameters, which is all it can do about it (ADR-0016).
    pub chunking: String,
    pub chunks: Vec<StoredChunk>,
    pub size_bytes: u64,
    /// Absent in a stored record means `Private`, like everything else about labels.
    #[serde(default)]
    pub visibility: Visibility,
    /// Present when the chunks are ciphertext (ADR-0019). Absent on everything else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<Sharing>,
    /// Free-form, and never part of the identity. A media type, an original filename, a
    /// service's own key — all things that may differ between two nodes holding the same
    /// bytes.
    #[serde(default)]
    pub metadata: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectError {
    /// The chunk list does not produce the id the record claims.
    IdMismatch {
        claimed: String,
        actual: String,
    },
    /// The chunk lengths do not add up to the stated size.
    SizeMismatch {
        claimed: u64,
        actual: u64,
    },
    /// A chunk digest is not a digest.
    BadDigest(String),
    /// Labelled `Shared` with no way for anybody to open it.
    SharedWithoutAnEnvelope,
    /// The envelope is malformed: no recipients, or a nonce prefix that is not one.
    BadEnvelope(String),
    /// Chunked under rules this node does not use, so its chunks cannot be shared.
    ForeignChunking {
        theirs: String,
        ours: String,
    },
    Schema(String),
}

impl std::fmt::Display for ObjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ObjectError::IdMismatch { claimed, actual } => write!(
                f,
                "the object claims to be {claimed} but its chunks hash to {actual}"
            ),
            ObjectError::SizeMismatch { claimed, actual } => {
                write!(
                    f,
                    "the object claims {claimed} bytes but its chunks total {actual}"
                )
            }
            ObjectError::BadDigest(d) => write!(f, "{d:?} is not a BLAKE3 digest"),
            ObjectError::SharedWithoutAnEnvelope => write!(
                f,
                "this object is labelled shared but its bytes are not sealed, so there is \
                 no content key for anybody — including its owner — to be given. A shared \
                 object is encrypted before it is chunked (ADR-0019); an existing object \
                 becomes shared by being stored again sealed, not by being relabelled"
            ),
            ObjectError::BadEnvelope(m) => write!(f, "the sharing envelope is unusable: {m}"),
            ObjectError::ForeignChunking { theirs, ours } => write!(
                f,
                "chunked as {theirs}, but this node chunks as {ours}; its chunks cannot be shared"
            ),
            ObjectError::Schema(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ObjectError {}

impl Object {
    /// Build a record from a chunk list.
    pub fn new(chunks: &[ChunkRef], visibility: Visibility) -> Object {
        Object {
            schema_version: SCHEMA_VERSION.to_string(),
            content_id: ContentId::of(chunks),
            chunking: CHUNKING_VERSION.to_string(),
            chunks: chunks.iter().map(StoredChunk::from).collect(),
            size_bytes: chunks.iter().map(|c| c.length as u64).sum(),
            visibility,
            sharing: None,
            metadata: Default::default(),
        }
    }

    /// Attach a sharing envelope. Used by the `SHARED` put path, which is the only place
    /// that has both the ciphertext's chunk list and the sealed keys.
    #[must_use]
    pub fn with_sharing(mut self, sharing: Sharing) -> Object {
        self.sharing = Some(sharing);
        self
    }

    /// Check a record against itself.
    ///
    /// Worth doing on anything read from disk or received from a peer: a record is a claim,
    /// and this is the arithmetic that decides whether the claim is self-consistent. It does
    /// not prove the *chunks* are present or correct — that needs the bytes, and the store
    /// checks each one against its digest as it reads it.
    pub fn validate(&self) -> Result<(), ObjectError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ObjectError::Schema(format!(
                "schema {} is not {SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        if self.chunking != CHUNKING_VERSION {
            return Err(ObjectError::ForeignChunking {
                theirs: self.chunking.clone(),
                ours: CHUNKING_VERSION.to_string(),
            });
        }

        let mut refs = Vec::with_capacity(self.chunks.len());
        for c in &self.chunks {
            let digest = crate::object::digest_from_hex(&c.blake3)
                .ok_or_else(|| ObjectError::BadDigest(c.blake3.clone()))?;
            refs.push(ChunkRef {
                digest,
                length: c.length,
            });
        }

        let total: u64 = refs.iter().map(|c| c.length as u64).sum();
        if total != self.size_bytes {
            return Err(ObjectError::SizeMismatch {
                claimed: self.size_bytes,
                actual: total,
            });
        }

        let actual = ContentId::of(&refs);
        if actual != self.content_id {
            return Err(ObjectError::IdMismatch {
                claimed: self.content_id.to_hex(),
                actual: actual.to_hex(),
            });
        }

        // Shared implies an envelope, but not the converse: a demoted object keeps its
        // envelope because its bytes are still sealed, and requiring the biconditional
        // would make demotion destroy the owner's own access.
        match (&self.sharing, self.visibility) {
            (None, Visibility::Shared) => return Err(ObjectError::SharedWithoutAnEnvelope),
            (Some(s), _) => s.validate()?,
            (None, _) => {}
        }
        Ok(())
    }

    pub fn chunk_refs(&self) -> Vec<ChunkRef> {
        self.chunks
            .iter()
            .filter_map(|c| {
                digest_from_hex(&c.blake3).map(|digest| ChunkRef {
                    digest,
                    length: c.length,
                })
            })
            .collect()
    }
}

pub(crate) fn digest_from_hex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk;

    fn refs(n: u8) -> Vec<ChunkRef> {
        (0..n).map(|i| ChunkRef::of(&[i; 100])).collect()
    }

    #[test]
    fn the_same_bytes_get_the_same_name_on_any_node() {
        // The property the neighbourhood cache depends on.
        let data = b"the same bytes, stored by two different people".repeat(1000);
        assert_eq!(
            ContentId::of(&chunk::slice(&data)),
            ContentId::of(&chunk::slice(&data))
        );
    }

    #[test]
    fn the_label_is_not_part_of_the_name() {
        // Two people storing the same file under different labels must still recognise it
        // as the same file, and relabelling must not rename anything.
        let c = refs(3);
        let private = Object::new(&c, Visibility::Private);
        let public = Object::new(&c, Visibility::Public);
        assert_eq!(private.content_id, public.content_id);
    }

    #[test]
    fn different_content_gets_a_different_name() {
        assert_ne!(ContentId::of(&refs(3)), ContentId::of(&refs(4)));
    }

    #[test]
    fn order_is_part_of_the_name() {
        // The same chunks in a different order are a different file.
        let mut a = refs(4);
        let forward = ContentId::of(&a);
        a.reverse();
        assert_ne!(forward, ContentId::of(&a));
    }

    #[test]
    fn an_empty_object_still_has_a_name() {
        let o = Object::new(&[], Visibility::Private);
        assert_eq!(o.size_bytes, 0);
        assert!(o.chunks.is_empty());
        o.validate().expect("an empty object is valid");
    }

    #[test]
    fn a_record_that_lies_about_its_own_name_is_refused() {
        let mut o = Object::new(&refs(3), Visibility::Public);
        o.chunks.pop();
        assert!(matches!(o.validate(), Err(ObjectError::SizeMismatch { .. })));
    }

    #[test]
    fn a_record_that_lies_about_its_id_is_refused() {
        let mut o = Object::new(&refs(3), Visibility::Public);
        o.content_id = ContentId::of(&refs(9));
        assert!(matches!(o.validate(), Err(ObjectError::IdMismatch { .. })));
    }

    #[test]
    fn a_record_chunked_under_other_rules_is_refused_with_a_reason() {
        // Not a corruption — a node from a different network. The error says so, because
        // "its chunks cannot be shared" is a different problem from "this file is broken".
        let mut o = Object::new(&refs(2), Visibility::Public);
        o.chunking = "fastcdc-v2020-4k-16k-64k".into();
        assert!(matches!(o.validate(), Err(ObjectError::ForeignChunking { .. })));
    }

    #[test]
    fn a_record_with_a_bad_digest_is_refused() {
        let mut o = Object::new(&refs(2), Visibility::Public);
        o.chunks[0].blake3 = "nonsense".into();
        assert!(matches!(o.validate(), Err(ObjectError::BadDigest(_))));
    }

    #[test]
    fn a_record_round_trips_through_json() {
        let mut o = Object::new(&chunk::slice(&b"hello".repeat(10_000)), Visibility::Public);
        o.metadata.insert("media_type".into(), "text/plain".into());
        let json = serde_json::to_string_pretty(&o).expect("serialize");
        let back: Object = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, o);
        back.validate().expect("valid after a round trip");
    }

    #[test]
    fn a_record_with_no_label_reads_as_private() {
        // The fail-closed rule reaching all the way through the record, not just the enum.
        let o = Object::new(&refs(2), Visibility::Public);
        let mut value = serde_json::to_value(&o).expect("to value");
        value.as_object_mut().unwrap().remove("visibility");
        let back: Object = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back.visibility, Visibility::Private);
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        // A record from a newer version may mean something this node would get wrong.
        let o = Object::new(&refs(1), Visibility::Public);
        let mut value = serde_json::to_value(&o).expect("to value");
        value
            .as_object_mut()
            .unwrap()
            .insert("expires_at".into(), serde_json::json!(1));
        assert!(serde_json::from_value::<Object>(value).is_err());
    }

    #[test]
    fn a_content_id_round_trips_through_hex() {
        let id = ContentId::of(&refs(5));
        assert_eq!(ContentId::from_hex(&id.to_hex()), Some(id));
        for bad in ["", "zz", &"f".repeat(63), &"g".repeat(64)] {
            assert_eq!(ContentId::from_hex(bad), None, "{bad:?}");
        }
    }
}

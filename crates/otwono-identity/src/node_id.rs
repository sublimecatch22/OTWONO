//! NodeID: the network-independent name of a node.
//!
//! A NodeID is a multihash of the node's Ed25519 public key, so it can be verified by
//! anyone who receives that key and depends on nothing but the key itself — no registry,
//! no authority, no connectivity (ADR-0006).
//!
//! ```text
//! NodeID       = 0x12 0x20 || sha2-256(ed25519_public_key)      34 bytes
//! Text form    = "otw1" || base32-crockford(NodeID)             ~55 chars
//! Fingerprint  = first 80 bits of the digest, in four groups    otw1:qm7f-2k9x-8v3t-rj5p
//! ```
//!
//! The 0x12 0x20 prefix is the multihash code for sha2-256 and its length. Carrying it
//! means a future algorithm change is self-describing rather than a silent reinterpretation
//! of the same bytes.

use data_encoding::Encoding;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

/// Multihash code for sha2-256, followed by its 32-byte length.
const MULTIHASH_SHA2_256: [u8; 2] = [0x12, 0x20];
pub const NODE_ID_BYTES: usize = 34;
/// Human-facing prefix, so a NodeID is recognisable out of context.
pub const NODE_ID_PREFIX: &str = "otw1";
/// Bits of digest shown in a fingerprint. Eighty is the trade-off between collision
/// resistance and something a person will actually read aloud over a phone.
const FINGERPRINT_BITS: usize = 80;

/// Crockford base32: no I, L, O or U, so a fingerprint read aloud cannot be transcribed
/// into a different one.
fn crockford() -> Encoding {
    let mut spec = data_encoding::Specification::new();
    spec.symbols.push_str("0123456789ABCDEFGHJKMNPQRSTVWXYZ");
    spec.translate.from.push_str("ILOUilou");
    spec.translate
        .to
        .push_str("11 0011 0 0".replace(' ', "").as_str());
    spec.encoding().expect("the Crockford specification is valid")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId([u8; NODE_ID_BYTES]);

impl NodeId {
    /// Compute the NodeID for an Ed25519 public key.
    pub fn from_public_key(public_key: &[u8; 32]) -> Self {
        let digest = Sha256::digest(public_key);
        let mut bytes = [0u8; NODE_ID_BYTES];
        bytes[..2].copy_from_slice(&MULTIHASH_SHA2_256);
        bytes[2..].copy_from_slice(&digest);
        NodeId(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; NODE_ID_BYTES] {
        &self.0
    }

    /// The digest without the multihash prefix.
    pub fn digest(&self) -> &[u8] {
        &self.0[2..]
    }

    /// Full text form. This is what goes in records and on the wire.
    pub fn to_text(&self) -> String {
        format!("{NODE_ID_PREFIX}{}", crockford().encode(&self.0).to_lowercase())
    }

    pub fn parse(text: &str) -> Result<Self, NodeIdError> {
        let body = text
            .strip_prefix(NODE_ID_PREFIX)
            .ok_or_else(|| NodeIdError::BadPrefix(text.chars().take(8).collect()))?;
        let decoded = crockford()
            .decode(body.to_uppercase().as_bytes())
            .map_err(|e| NodeIdError::BadEncoding(e.to_string()))?;
        let bytes: [u8; NODE_ID_BYTES] = decoded
            .as_slice()
            .try_into()
            .map_err(|_| NodeIdError::BadLength(decoded.len()))?;
        if bytes[..2] != MULTIHASH_SHA2_256 {
            return Err(NodeIdError::UnsupportedHash([bytes[0], bytes[1]]));
        }
        Ok(NodeId(bytes))
    }

    /// Short, human-checkable form: `otw1:qm7f-2k9x-8v3t-rj5p`.
    ///
    /// For display and voice comparison only. Every automated comparison uses the full
    /// NodeID — a truncated identifier is a weaker claim and must never be the thing a
    /// trust decision is made against.
    pub fn fingerprint(&self) -> String {
        let bytes = &self.digest()[..FINGERPRINT_BITS / 8];
        let encoded = crockford().encode(bytes).to_lowercase();
        let groups: Vec<String> = encoded
            .as_bytes()
            .chunks(4)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect();
        format!("{NODE_ID_PREFIX}:{}", groups.join("-"))
    }

    /// Does this NodeID actually name that public key?
    ///
    /// The check a peer must run on every handshake: a claimed NodeID means nothing until
    /// it is shown to be the hash of the key that just authenticated.
    pub fn matches_public_key(&self, public_key: &[u8; 32]) -> bool {
        *self == NodeId::from_public_key(public_key)
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_text())
    }
}

impl Serialize for NodeId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_text())
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        NodeId::parse(&text).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeIdError {
    BadPrefix(String),
    BadEncoding(String),
    BadLength(usize),
    UnsupportedHash([u8; 2]),
}

impl std::fmt::Display for NodeIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeIdError::BadPrefix(got) => {
                write!(f, "a NodeID must start with `{NODE_ID_PREFIX}`, got {got:?}")
            }
            NodeIdError::BadEncoding(e) => write!(f, "not valid Crockford base32: {e}"),
            NodeIdError::BadLength(n) => {
                write!(f, "a NodeID decodes to {NODE_ID_BYTES} bytes, got {n}")
            }
            NodeIdError::UnsupportedHash(c) => {
                write!(
                    f,
                    "unsupported multihash code {c:02x?}; this build understands sha2-256 only"
                )
            }
        }
    }
}

impl std::error::Error for NodeIdError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    #[test]
    fn a_node_id_is_a_sha256_multihash_of_the_key() {
        let id = NodeId::from_public_key(&key(1));
        assert_eq!(id.as_bytes()[..2], MULTIHASH_SHA2_256);
        assert_eq!(id.digest(), Sha256::digest(key(1)).as_slice());
        assert_eq!(id.as_bytes().len(), NODE_ID_BYTES);
    }

    #[test]
    fn text_form_round_trips() {
        let id = NodeId::from_public_key(&key(7));
        let text = id.to_text();
        assert!(text.starts_with(NODE_ID_PREFIX), "{text}");
        assert_eq!(NodeId::parse(&text).unwrap(), id);
    }

    #[test]
    fn text_form_is_the_documented_length() {
        // 34 bytes of base32 is 55 symbols, plus the 4-character prefix.
        let text = NodeId::from_public_key(&key(3)).to_text();
        assert_eq!(text.len(), NODE_ID_PREFIX.len() + 55, "{text}");
    }

    #[test]
    fn different_keys_give_different_ids() {
        assert_ne!(NodeId::from_public_key(&key(1)), NodeId::from_public_key(&key(2)));
    }

    #[test]
    fn the_same_key_always_gives_the_same_id() {
        // Identity must survive restarts and reinstalls, so this cannot depend on anything
        // but the key.
        assert_eq!(NodeId::from_public_key(&key(9)), NodeId::from_public_key(&key(9)));
    }

    #[test]
    fn fingerprint_is_four_groups_of_four() {
        let fp = NodeId::from_public_key(&key(5)).fingerprint();
        let body = fp.strip_prefix("otw1:").expect("prefix");
        let groups: Vec<&str> = body.split('-').collect();
        assert_eq!(groups.len(), 4, "{fp}");
        assert!(groups.iter().all(|g| g.len() == 4), "{fp}");
    }

    #[test]
    fn fingerprints_avoid_the_ambiguous_letters() {
        // Crockford omits I, L, O and U so a spoken fingerprint cannot be mis-transcribed.
        for seed in 0..40u8 {
            let fp = NodeId::from_public_key(&key(seed)).fingerprint();
            let body = fp.strip_prefix("otw1:").unwrap();
            for c in body.chars().filter(|c| *c != '-') {
                assert!(!"ilou".contains(c), "{fp} contains an ambiguous character");
            }
        }
    }

    #[test]
    fn parsing_is_case_insensitive() {
        let id = NodeId::from_public_key(&key(11));
        let upper = format!("{NODE_ID_PREFIX}{}", crockford().encode(id.as_bytes()));
        assert_eq!(NodeId::parse(&upper).unwrap(), id);
    }

    #[test]
    fn a_missing_prefix_is_rejected() {
        let id = NodeId::from_public_key(&key(1));
        let without = id.to_text().replace(NODE_ID_PREFIX, "");
        assert!(matches!(NodeId::parse(&without), Err(NodeIdError::BadPrefix(_))));
    }

    #[test]
    fn a_truncated_node_id_is_rejected_not_padded() {
        let id = NodeId::from_public_key(&key(1));
        let text = id.to_text();
        let short = &text[..text.len() - 8];
        assert!(NodeId::parse(short).is_err(), "a short NodeID must not parse");
    }

    #[test]
    fn a_fingerprint_is_not_accepted_as_a_node_id() {
        // The dangerous confusion: 80 bits is for humans, never for a trust decision.
        let fp = NodeId::from_public_key(&key(1)).fingerprint();
        assert!(NodeId::parse(&fp).is_err());
    }

    #[test]
    fn matching_a_public_key_is_what_binds_a_claim() {
        let id = NodeId::from_public_key(&key(4));
        assert!(id.matches_public_key(&key(4)));
        assert!(
            !id.matches_public_key(&key(5)),
            "a NodeID must not accept another key"
        );
    }

    #[test]
    fn serde_uses_the_text_form() {
        let id = NodeId::from_public_key(&key(2));
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{}\"", id.to_text()));
        assert_eq!(serde_json::from_str::<NodeId>(&json).unwrap(), id);
    }

    #[test]
    fn serde_rejects_a_malformed_node_id() {
        assert!(serde_json::from_str::<NodeId>("\"otw1zzzz\"").is_err());
        assert!(serde_json::from_str::<NodeId>("\"not-a-node-id\"").is_err());
    }
}

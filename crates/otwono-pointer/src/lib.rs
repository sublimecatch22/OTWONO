//! Signed mutable pointers — the second OTWONO primitive (ADR-0027).
//!
//! `DISTRIBUTED-SERVICES.md` §1: every service is composed from content-addressed blocks,
//! signed mutable pointers, and addressed messages. Blocks give immutability. This gives
//! change over time, and it is what the profile site, the wiki and the forum are each built
//! out of — a collection of blocks plus a pointer at the current one.
//!
//! # The threat is rollback, and signatures do not solve it
//!
//! Read this before changing anything here.
//!
//! A signature proves the owner wrote a record. It does **not** prove the record is current.
//! An old pointer is a genuine, correctly signed statement by the rightful owner that
//! happens to be out of date — so anyone who can serve responses can roll a reader back to
//! any historical version, and every signature check passes on the way.
//!
//! What defends against that is not cryptography but **memory**: [`SequenceLog`] records the
//! highest sequence seen for each pointer, and anything lower is refused. A reader with no
//! memory has no protection, which is why a first read is trust-on-first-use and says so.
//!
//! # What this crate does not do
//!
//! It does not fetch pointers from peers, publish them, or resolve `onm://` addresses. It is
//! the record, its encoding, and the rules — testable with no network, no daemon and no
//! sockets, which is the only way the rollback rules can be exercised exhaustively.

#![forbid(unsafe_code)]

use otwono_identity::{canonical_json, NodeId, APPLICATION_DOMAIN};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: &str = "1.0.0";

/// Inner prefix over the canonical record, beneath `id.sign`'s application domain.
///
/// Both are needed and neither substitutes for the other: the outer domain stops a pointer
/// signature being replayed as a *session* signature, and this stops it being replayed as a
/// different *application* record — a model manifest, say, that happened to canonicalize to
/// the same bytes.
pub const POINTER_DOMAIN: &[u8] = b"otwono-pointer-v1:";

/// A signed binding from a name to a content id, at a point in a sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pointer {
    pub schema_version: String,
    /// The owner, in text form. Exactly one node may write a given pointer.
    pub node_id: String,
    /// Which service's namespace the name lives in: `wiki`, `profile`, `forum`.
    pub service: String,
    pub name: String,
    /// Strictly increasing, chosen by the owner. The rollback defence.
    pub sequence: u64,
    /// What the name points at now. `None` is a tombstone — the owner saying this no longer
    /// exists, signed and sequenced like any other update (ADR-0027 §4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    /// When the owner says it published this. Shown to people; **never** used for ordering.
    pub published_at_ms: u64,
    /// Base64 Ed25519 signature. Empty on a record that has not been signed yet.
    #[serde(default)]
    pub signature: String,
}

impl Pointer {
    /// A new, unsigned pointer.
    pub fn new(
        node_id: &NodeId,
        service: impl Into<String>,
        name: impl Into<String>,
        sequence: u64,
        content_id: Option<String>,
        published_at_ms: u64,
    ) -> Pointer {
        Pointer {
            schema_version: SCHEMA_VERSION.to_string(),
            node_id: node_id.to_text(),
            service: service.into(),
            name: name.into(),
            sequence,
            content_id,
            published_at_ms,
            signature: String::new(),
        }
    }

    /// Whether this says the thing is gone.
    pub fn is_tombstone(&self) -> bool {
        self.content_id.is_none()
    }

    /// The bytes a signature covers: `APPLICATION_DOMAIN || POINTER_DOMAIN || canonical`.
    ///
    /// The application domain is included because that is what `id.sign` prepends before
    /// signing, so a verifier that omitted it would reject every genuine signature. Building
    /// the full message here — rather than expecting callers to remember — is what keeps the
    /// signing path and the verifying path from disagreeing.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, PointerError> {
        let mut value = serde_json::to_value(self).map_err(|e| PointerError::Encoding(e.to_string()))?;
        if let Some(obj) = value.as_object_mut() {
            // A signature cannot cover itself.
            obj.remove("signature");
        }
        let mut message = APPLICATION_DOMAIN.to_vec();
        message.extend_from_slice(POINTER_DOMAIN);
        message.extend_from_slice(&canonical_json(&value));
        Ok(message)
    }

    /// The payload to hand `id.sign`, which adds the application domain itself.
    pub fn payload_for_id_sign(&self) -> Result<Vec<u8>, PointerError> {
        let mut value = serde_json::to_value(self).map_err(|e| PointerError::Encoding(e.to_string()))?;
        if let Some(obj) = value.as_object_mut() {
            obj.remove("signature");
        }
        let mut message = POINTER_DOMAIN.to_vec();
        message.extend_from_slice(&canonical_json(&value));
        Ok(message)
    }

    /// Check the signature, and that the key offered is the owner's.
    ///
    /// The second half is not optional and not the caller's job. A NodeID is
    /// `SHA-256(public key)`, so the key cannot be recovered from it and must be supplied
    /// alongside — which means a verifier that only checked the signature would accept any
    /// record from anyone, since an attacker signs with their own key and supplies it. The
    /// binding between the key and the claimed NodeID is the whole of the identity check.
    pub fn verify(&self, public_key: &[u8; 32]) -> Result<(), PointerError> {
        let claimed =
            NodeId::parse(&self.node_id).map_err(|e| PointerError::Malformed(format!("node_id: {e}")))?;
        // `matches_public_key` rather than comparing a recomputed NodeID: the identity crate
        // owns what that binding means, and a second implementation of it here would be a
        // second place for it to be wrong.
        if !claimed.matches_public_key(public_key) {
            return Err(PointerError::WrongKey);
        }
        self.check_shape()?;
        let signature = data_encoding::BASE64
            .decode(self.signature.as_bytes())
            .map_err(|e| PointerError::Malformed(format!("signature is not base64: {e}")))?;
        otwono_identity::verify_signature(public_key, &self.signing_bytes()?, &signature)
            .map_err(|_| PointerError::BadSignature)
    }

    /// Structural rules that hold regardless of who signed it.
    fn check_shape(&self) -> Result<(), PointerError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(PointerError::Malformed(format!(
                "schema_version {} is not {SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        // Zero is reserved as "no sequence yet" in a reader's log, so a record may not use
        // it — otherwise a first record and an absent one would compare equal.
        if self.sequence == 0 {
            return Err(PointerError::Malformed("sequence must be at least 1".into()));
        }
        if self.service.is_empty()
            || !self
                .service
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(PointerError::Malformed(format!(
                "service {:?} is not a lowercase name",
                self.service
            )));
        }
        if self.name.is_empty() || self.name.len() > 512 {
            return Err(PointerError::Malformed("name must be 1..=512 bytes".into()));
        }
        if let Some(id) = &self.content_id {
            if id.len() != 64
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(PointerError::Malformed(
                    "content_id must be 64 lowercase hex characters".into(),
                ));
            }
        }
        Ok(())
    }

    /// The tuple that identifies this pointer.
    pub fn key(&self) -> PointerKey {
        PointerKey {
            node_id: self.node_id.clone(),
            service: self.service.clone(),
            name: self.name.clone(),
        }
    }
}

/// What a reader asked for, and what must equal what the signature covers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PointerKey {
    pub node_id: String,
    pub service: String,
    pub name: String,
}

/// What happened when a record was offered to a [`SequenceLog`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepted {
    /// Nothing had been seen for this pointer before.
    ///
    /// Named rather than folded into `Advanced`, because it is the case with **no rollback
    /// protection at all**: a reader with no memory takes what it is given. A caller that
    /// wants to warn a person, or pin a key, needs to know which of the two this was.
    FirstSeen,
    /// Strictly newer than anything seen before.
    Advanced { from: u64, to: u64 },
}

/// The highest sequence seen for each pointer.
///
/// This is the rollback defence, and it is state rather than cryptography. Losing it means
/// reverting to first-use trust — which is why it belongs somewhere durable in a real node,
/// and why this type keeps no opinion about where.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SequenceLog {
    highest: BTreeMap<PointerKey, u64>,
}

impl SequenceLog {
    pub fn new() -> SequenceLog {
        SequenceLog::default()
    }

    /// The highest sequence seen, or `None` if this pointer is new to us.
    pub fn highest_seen(&self, key: &PointerKey) -> Option<u64> {
        self.highest.get(key).copied()
    }

    pub fn len(&self) -> usize {
        self.highest.len()
    }

    pub fn is_empty(&self) -> bool {
        self.highest.is_empty()
    }

    /// Verify a record and, if it is genuinely newer, remember it.
    ///
    /// `expected` is what the reader asked for. It is checked against what the signature
    /// covers, because a pointer that only bound `content_id` and `sequence` could be lifted
    /// from `wiki/Home` and served as `profile/index` with the signature still verifying
    /// (ADR-0027 §6).
    ///
    /// A record at a sequence already seen is **refused**, not ignored and not ranked
    /// against. Equal counts as refused too: re-serving the same sequence with different
    /// content would otherwise be a way to change what a name means without advancing it.
    pub fn accept(
        &mut self,
        pointer: &Pointer,
        public_key: &[u8; 32],
        expected: &PointerKey,
    ) -> Result<Accepted, PointerError> {
        if &pointer.key() != expected {
            return Err(PointerError::WrongPointer(Box::new(WrongPointer {
                asked: expected.clone(),
                got: pointer.key(),
            })));
        }
        pointer.verify(public_key)?;

        match self.highest.get(expected).copied() {
            None => {
                self.highest.insert(expected.clone(), pointer.sequence);
                Ok(Accepted::FirstSeen)
            }
            Some(seen) if pointer.sequence > seen => {
                self.highest.insert(expected.clone(), pointer.sequence);
                Ok(Accepted::Advanced {
                    from: seen,
                    to: pointer.sequence,
                })
            }
            Some(seen) => Err(PointerError::Rollback {
                seen,
                offered: pointer.sequence,
            }),
        }
    }
}

/// Which pointer was asked for, and which one arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrongPointer {
    pub asked: PointerKey,
    pub got: PointerKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerError {
    /// The record is structurally wrong, whoever signed it.
    Malformed(String),
    /// The signature does not verify.
    BadSignature,
    /// The key offered is not the one the claimed NodeID names.
    WrongKey,
    /// The record is for a different pointer than the one asked for.
    ///
    /// Boxed because it carries two whole keys: without it every `Result` in this crate
    /// would be sized for the rarest error, which is the wrong thing to optimise on a path
    /// that mostly succeeds.
    WrongPointer(Box<WrongPointer>),
    /// A correctly signed record that is older than one already seen. Not a forgery — this
    /// is the attack that signatures cannot catch (ADR-0027 §1).
    Rollback {
        seen: u64,
        offered: u64,
    },
    Encoding(String),
}

impl std::fmt::Display for PointerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PointerError::Malformed(m) => write!(f, "malformed pointer: {m}"),
            PointerError::BadSignature => write!(f, "the signature does not verify"),
            PointerError::WrongKey => {
                write!(f, "the public key offered is not the one this node_id names")
            }
            PointerError::WrongPointer(w) => write!(
                f,
                "asked for {}/{}/{} and got {}/{}/{}",
                w.asked.node_id, w.asked.service, w.asked.name, w.got.node_id, w.got.service, w.got.name
            ),
            PointerError::Rollback { seen, offered } => write!(
                f,
                "refused a rollback: sequence {offered} is not newer than {seen} already seen"
            ),
            PointerError::Encoding(m) => write!(f, "cannot encode the pointer: {m}"),
        }
    }
}

impl std::error::Error for PointerError {}

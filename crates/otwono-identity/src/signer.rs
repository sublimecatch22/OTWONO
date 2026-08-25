//! Who is allowed to sign for this node.
//!
//! A Noise handshake needs two different secrets, and they do not have to live in the
//! same process:
//!
//! * the **X25519 agreement secret**, which the Noise state machine uses directly and
//!   which therefore must be in the process doing the handshake;
//! * the **Ed25519 signing key**, which is only ever asked for two signatures — the
//!   agreement binding and the per-session proof.
//!
//! [`SessionSigner`] is that seam. `otwono-net` handshakes against the trait, so the
//! signing key can sit behind a control-plane call in another daemon (`otwono-idd`)
//! instead of being loaded into the process that talks to the network. The daemon that
//! parses hostile input never holds the key that names the node.
//!
//! Two implementations exist. [`NodeIdentity`](crate::NodeIdentity) signs locally — that
//! is what tests and single-process tools use. `otwono-netd` supplies a brokered one.

use crate::{AgreementBinding, IdentityError, NodeId};
use zeroize::Zeroizing;

/// Domain tag for the per-session signature, distinct from every other signing context.
pub const SESSION_DOMAIN: &[u8] = b"otwono-session-v1:";

/// Length of the Noise handshake hash a session proof signs.
///
/// `Noise_..._BLAKE2s` produces 32 bytes. The length is checked rather than assumed
/// because a remote signer is a signing oracle for this domain, and an oracle that will
/// sign an arbitrary-length payload is a larger one than the protocol needs.
pub const HANDSHAKE_HASH_LEN: usize = 32;

/// The message a session proof signs. Public so tests can forge one and prove it fails.
pub fn session_proof_message(handshake_hash: &[u8]) -> Vec<u8> {
    let mut m = SESSION_DOMAIN.to_vec();
    m.extend_from_slice(handshake_hash);
    m
}

/// The signing capability a Noise handshake needs, wherever the key actually lives.
///
/// `Send + Sync` are supertraits because a daemon holds one behind an `Arc` and hands it
/// to a thread per connection.
pub trait SessionSigner: Send + Sync {
    /// This node's name. Derived from the Ed25519 key, so a brokered signer has to have
    /// asked the key holder for it.
    fn node_id(&self) -> NodeId;

    /// The X25519 static secret for the Noise handshake.
    ///
    /// This one cannot be brokered: `snow` drives the key agreement itself. It is the
    /// reason the split is between *two* keys rather than moving everything to one side.
    fn agreement_secret(&self) -> Zeroizing<[u8; 32]>;

    /// The signed statement binding this node's agreement key to its NodeID.
    ///
    /// Fallible because a brokered signer has to fetch it, and because a node whose
    /// agreement key has never been bound has no honest answer to give.
    fn agreement_binding(&self) -> Result<AgreementBinding, SignerError>;

    /// Sign `SESSION_DOMAIN || handshake_hash` with the node's Ed25519 key.
    fn sign_session(&self, handshake_hash: &[u8]) -> Result<[u8; 64], SignerError>;

    /// The signed statement binding this node's *sharing* key to its NodeID (ADR-0019).
    ///
    /// Optional, and the default is honest about that: a signer that does not hold or cannot
    /// reach a sharing key has no answer, and a node with no answer is simply one that
    /// cannot be sealed to. It is not a handshake failure — Noise needs the agreement key
    /// and nothing else, and tying the two together would mean a node that could not share
    /// also could not mesh.
    fn sharing_binding(&self) -> Result<crate::SharingBinding, SignerError> {
        Err(SignerError::Unavailable(
            "this signer holds no sharing key, so nothing can be sealed to this node".into(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignerError {
    Identity(IdentityError),
    /// The key holder could not be reached, or refused.
    Unavailable(String),
    /// No agreement key has been bound to this node's signing key.
    NoAgreementBinding,
    /// A handshake hash of the wrong length was offered for signature.
    BadHandshakeHash(usize),
}

impl std::fmt::Display for SignerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignerError::Identity(e) => write!(f, "{e}"),
            SignerError::Unavailable(e) => write!(f, "the node's signing key is unavailable: {e}"),
            SignerError::NoAgreementBinding => write!(
                f,
                "this node has no agreement binding; the signing key has not vouched for an \
                 agreement key, so no peer could verify a handshake"
            ),
            SignerError::BadHandshakeHash(n) => write!(
                f,
                "a session proof signs a {HANDSHAKE_HASH_LEN}-byte handshake hash, not {n} bytes"
            ),
        }
    }
}

impl std::error::Error for SignerError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeIdentity;

    #[test]
    fn a_local_identity_signs_its_own_sessions() {
        let id = NodeIdentity::from_seeds(&[1u8; 32], &[2u8; 32], 1);
        let hash = [7u8; HANDSHAKE_HASH_LEN];
        let sig = id.sign_session(&hash).unwrap();
        crate::verify_signature(&id.public_key_bytes(), &session_proof_message(&hash), &sig)
            .expect("a node must be able to verify its own session proof");
    }

    #[test]
    fn a_handshake_hash_of_the_wrong_length_is_refused() {
        // The signer is an oracle for this domain. Bounding what it will sign bounds what
        // an attacker who reaches it can obtain.
        let id = NodeIdentity::from_seeds(&[3u8; 32], &[4u8; 32], 1);
        assert_eq!(
            id.sign_session(&[0u8; 64]),
            Err(SignerError::BadHandshakeHash(64))
        );
        assert_eq!(id.sign_session(&[]), Err(SignerError::BadHandshakeHash(0)));
    }

    #[test]
    fn the_session_domain_is_distinct_from_every_other_signing_context() {
        let session = String::from_utf8_lossy(SESSION_DOMAIN).to_string();
        for other in [
            "otwono-agreement-binding-v1:",
            "otwono-succession-v1:",
            "otwono-application-v1:",
        ] {
            assert_ne!(session, other);
            assert!(
                !other.starts_with(&session),
                "{other} must not extend the session domain"
            );
            assert!(
                !session.starts_with(other),
                "the session domain must not extend {other}"
            );
        }
    }

    #[test]
    fn a_session_signature_does_not_verify_against_the_bare_hash() {
        // Without the prefix, a signature made for a session could be presented as a
        // signature over whatever else the protocol happens to hash to 32 bytes.
        let id = NodeIdentity::from_seeds(&[5u8; 32], &[6u8; 32], 1);
        let hash = [8u8; HANDSHAKE_HASH_LEN];
        let sig = id.sign_session(&hash).unwrap();
        assert!(crate::verify_signature(&id.public_key_bytes(), &hash, &sig).is_err());
    }
}

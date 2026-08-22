//! OTWONO node identity.
//!
//! A node's identity is a long-term Ed25519 signing key, generated on first boot and
//! independent of IP address, MAC address, hostname and physical network (ADR-0006). Its
//! [`NodeId`] is a multihash of the public key, so any peer can verify the name against
//! the key with no registry and no connectivity.
//!
//! # Two keys, two jobs, two processes
//!
//! The signing key **signs**. A separate X25519 key does key agreement for Noise. The
//! signing key vouches for the agreement key with a signed binding record
//! ([`AgreementBinding`]).
//!
//! Deriving the X25519 key from the Ed25519 seed would have been one fewer thing to store,
//! and is what the birational map is for. It is not what this does, because ADR-0006 says
//! the long-term key signs and never encrypts: separate keys mean the agreement key can be
//! rotated after a suspected compromise without the node losing its name.
//!
//! Because they are separate keys they can live in separate processes, and they do
//! (ADR-0010). [`SigningIdentity`] is held by `otwono-idd`; [`AgreementKey`] is held by
//! `otwono-netd`, the daemon that parses input from the network. [`NodeIdentity`] is both
//! halves at once — what a test or a single-process tool holds, never a daemon. The seam
//! between them is [`SessionSigner`].
//!
//! # Device, not person
//!
//! This is a *device* identity. A user identity may span several of a person's nodes and
//! is a separate thing bound by a certificate. Conflating them makes device loss into
//! identity loss and blocks multi-device support later.

#![forbid(unsafe_code)]

pub mod keystore;
pub mod node_id;
pub mod signer;

pub use keystore::{
    migrate_combined, AgreementKeystore, KeystoreError, SigningKeystore, StoredAgreementKey,
    StoredSigningKey, SuccessionRecord, AGREEMENT_KEY_FILE, DEFAULT_IDENTITY_DIR, SIGNING_KEY_FILE,
};
pub use node_id::{NodeId, NodeIdError};
pub use signer::{session_proof_message, SessionSigner, SignerError, HANDSHAKE_HASH_LEN, SESSION_DOMAIN};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey as X25519Public, StaticSecret as X25519Secret};
use zeroize::Zeroizing;

pub const SCHEMA_VERSION: &str = "1.0.0";

/// The Ed25519 half: the key that *names* the node and signs on its behalf.
///
/// Held by `otwono-idd` alone. Nothing else in the system loads this key, because the set
/// of processes that can read it is the set of processes whose compromise costs the node
/// its identity permanently — a NodeID cannot be re-earned, only succeeded.
pub struct SigningIdentity {
    signing: SigningKey,
    node_id: NodeId,
    created_at_unix_ms: u64,
}

impl std::fmt::Debug for SigningIdentity {
    /// Never print key material, not even truncated. A debug line in a log is exactly how
    /// a private key ends up somewhere it should not be.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningIdentity")
            .field("node_id", &self.node_id.to_text())
            .field("created_at_unix_ms", &self.created_at_unix_ms)
            .finish_non_exhaustive()
    }
}

impl SigningIdentity {
    /// Generate a fresh signing key from the OS entropy source.
    ///
    /// On an SBC at first boot the entropy pool may not be seeded yet. `getrandom` blocks
    /// until it is rather than returning weak bytes, and a failure here is fatal: a
    /// predictable node key is worse than no node.
    pub fn generate() -> Result<Self, IdentityError> {
        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::getrandom(seed.as_mut()).map_err(|e| IdentityError::Entropy(e.to_string()))?;
        Ok(Self::from_seed(&seed, now_unix_ms()))
    }

    pub fn from_seed(seed: &[u8; 32], created_at_unix_ms: u64) -> Self {
        let signing = SigningKey::from_bytes(seed);
        let node_id = NodeId::from_public_key(&signing.verifying_key().to_bytes());
        SigningIdentity {
            signing,
            node_id,
            created_at_unix_ms,
        }
    }

    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    pub fn created_at_unix_ms(&self) -> u64 {
        self.created_at_unix_ms
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// The signing seed, for the keystore only.
    ///
    /// Returned wrapped in `Zeroizing` so a caller that drops it does not leave the seed
    /// in freed memory.
    pub(crate) fn seed(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.signing.to_bytes())
    }

    pub fn sign(&self, message: &[u8]) -> Signature {
        self.signing.sign(message)
    }

    /// The signed statement "this agreement key belongs to this node".
    ///
    /// A peer that completes a Noise handshake has proved possession of *an* X25519 key.
    /// This is what connects that key to a NodeID, and without it the handshake
    /// authenticates a key rather than a node.
    ///
    /// The agreement key is a parameter rather than a field because the process holding
    /// the signing key does not hold the agreement secret: it vouches for a public key it
    /// is told about.
    pub fn bind_agreement(&self, agreement_public_key: &[u8; 32]) -> AgreementBinding {
        let signature = self.sign(&binding_message(agreement_public_key));
        AgreementBinding {
            node_id: self.node_id,
            public_key: base64_encode(&self.public_key_bytes()),
            agreement_public_key: base64_encode(agreement_public_key),
            signature: base64_encode(&signature.to_bytes()),
        }
    }

    /// Sign a session proof. Refuses a hash of the wrong length — see
    /// [`HANDSHAKE_HASH_LEN`].
    pub fn sign_session(&self, handshake_hash: &[u8]) -> Result<[u8; 64], SignerError> {
        if handshake_hash.len() != HANDSHAKE_HASH_LEN {
            return Err(SignerError::BadHandshakeHash(handshake_hash.len()));
        }
        Ok(self.sign(&session_proof_message(handshake_hash)).to_bytes())
    }

    /// The public half, given the agreement key this node currently uses.
    pub fn to_public(&self, agreement_public_key: &[u8; 32]) -> PublicIdentity {
        PublicIdentity {
            schema_version: SCHEMA_VERSION.to_string(),
            node_id: self.node_id,
            public_key: base64_encode(&self.public_key_bytes()),
            agreement_public_key: base64_encode(agreement_public_key),
            created_at_unix_ms: self.created_at_unix_ms,
        }
    }
}

/// The X25519 half: the key Noise does key agreement with.
///
/// Held by `otwono-netd`. Losing it costs a node its current sessions and nothing else —
/// it can be replaced, and the signing key vouches for the replacement. That asymmetry
/// with [`SigningIdentity`] is the whole reason ADR-0006 keeps them separate.
pub struct AgreementKey {
    secret: X25519Secret,
    created_at_unix_ms: u64,
}

impl std::fmt::Debug for AgreementKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgreementKey")
            .field("public", &base64_encode(&self.public()))
            .field("created_at_unix_ms", &self.created_at_unix_ms)
            .finish_non_exhaustive()
    }
}

impl AgreementKey {
    pub fn generate() -> Result<Self, IdentityError> {
        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::getrandom(seed.as_mut()).map_err(|e| IdentityError::Entropy(e.to_string()))?;
        Ok(Self::from_seed(&seed, now_unix_ms()))
    }

    pub fn from_seed(seed: &[u8; 32], created_at_unix_ms: u64) -> Self {
        AgreementKey {
            secret: X25519Secret::from(*seed),
            created_at_unix_ms,
        }
    }

    pub fn created_at_unix_ms(&self) -> u64 {
        self.created_at_unix_ms
    }

    pub fn public(&self) -> [u8; 32] {
        X25519Public::from(&self.secret).to_bytes()
    }

    pub fn secret_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.secret.to_bytes())
    }
}

/// A node's complete private identity: both halves in one process.
///
/// This is what a single-process tool or a test holds. The daemons do **not**: `otwono-idd`
/// holds a [`SigningIdentity`] and `otwono-netd` holds an [`AgreementKey`], and they meet
/// over the control plane. Keeping the combined type around is deliberate — it is the
/// honest representation of "one process can do everything", and it is what makes the
/// handshake tests readable.
pub struct NodeIdentity {
    signing: SigningIdentity,
    agreement: AgreementKey,
}

impl std::fmt::Debug for NodeIdentity {
    /// Never print key material, not even truncated.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("node_id", &self.signing.node_id.to_text())
            .field("created_at_unix_ms", &self.signing.created_at_unix_ms)
            .finish_non_exhaustive()
    }
}

impl NodeIdentity {
    pub fn generate() -> Result<Self, IdentityError> {
        Ok(NodeIdentity {
            signing: SigningIdentity::generate()?,
            agreement: AgreementKey::generate()?,
        })
    }

    pub fn from_seeds(signing_seed: &[u8; 32], agreement_seed: &[u8; 32], created_at_unix_ms: u64) -> Self {
        NodeIdentity {
            signing: SigningIdentity::from_seed(signing_seed, created_at_unix_ms),
            agreement: AgreementKey::from_seed(agreement_seed, created_at_unix_ms),
        }
    }

    pub fn from_parts(signing: SigningIdentity, agreement: AgreementKey) -> Self {
        NodeIdentity { signing, agreement }
    }

    pub fn signing(&self) -> &SigningIdentity {
        &self.signing
    }

    pub fn agreement(&self) -> &AgreementKey {
        &self.agreement
    }

    pub fn node_id(&self) -> &NodeId {
        self.signing.node_id()
    }

    pub fn created_at_unix_ms(&self) -> u64 {
        self.signing.created_at_unix_ms()
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing.public_key_bytes()
    }

    pub fn agreement_public(&self) -> X25519Public {
        X25519Public::from(self.agreement.public())
    }

    pub fn sign(&self, message: &[u8]) -> Signature {
        self.signing.sign(message)
    }

    pub fn agreement_binding(&self) -> AgreementBinding {
        self.signing.bind_agreement(&self.agreement.public())
    }

    pub fn to_public(&self) -> PublicIdentity {
        self.signing.to_public(&self.agreement.public())
    }
}

/// A node that holds both halves signs for itself, with no control-plane round trip.
impl SessionSigner for NodeIdentity {
    fn node_id(&self) -> NodeId {
        *self.signing.node_id()
    }

    fn agreement_secret(&self) -> Zeroizing<[u8; 32]> {
        self.agreement.secret_bytes()
    }

    fn agreement_binding(&self) -> Result<AgreementBinding, SignerError> {
        Ok(NodeIdentity::agreement_binding(self))
    }

    fn sign_session(&self, handshake_hash: &[u8]) -> Result<[u8; 64], SignerError> {
        self.signing.sign_session(handshake_hash)
    }
}

/// What a node publishes about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicIdentity {
    pub schema_version: String,
    pub node_id: NodeId,
    /// Base64 Ed25519 public key.
    pub public_key: String,
    /// Base64 X25519 agreement public key.
    pub agreement_public_key: String,
    pub created_at_unix_ms: u64,
}

impl PublicIdentity {
    pub fn public_key_bytes(&self) -> Result<[u8; 32], IdentityError> {
        decode_key(&self.public_key)
    }

    /// Check the NodeID actually names the key. Always call this on anything received.
    pub fn is_self_consistent(&self) -> bool {
        match self.public_key_bytes() {
            Ok(k) => self.node_id.matches_public_key(&k),
            Err(_) => false,
        }
    }
}

/// A signed binding between a NodeID and an X25519 agreement key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgreementBinding {
    pub node_id: NodeId,
    pub public_key: String,
    pub agreement_public_key: String,
    pub signature: String,
}

impl AgreementBinding {
    /// Verify the binding end to end: the NodeID names the signing key, and the signing
    /// key vouches for the agreement key.
    pub fn verify(&self) -> Result<VerifiedPeer, IdentityError> {
        let public_key = decode_key(&self.public_key)?;
        if !self.node_id.matches_public_key(&public_key) {
            return Err(IdentityError::NodeIdMismatch);
        }
        let agreement = decode_key(&self.agreement_public_key)?;
        let signature_bytes = base64_decode(&self.signature)?;
        let signature_bytes: [u8; 64] = signature_bytes
            .as_slice()
            .try_into()
            .map_err(|_| IdentityError::MalformedSignature)?;

        let verifying =
            VerifyingKey::from_bytes(&public_key).map_err(|e| IdentityError::MalformedKey(e.to_string()))?;
        verifying
            .verify(
                &binding_message(&agreement),
                &Signature::from_bytes(&signature_bytes),
            )
            .map_err(|_| IdentityError::BadSignature)?;

        Ok(VerifiedPeer {
            node_id: self.node_id,
            public_key,
            agreement_public_key: agreement,
        })
    }
}

/// A peer whose NodeID, signing key and agreement key have all been checked against each
/// other. Authenticated — which is *not* the same as trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPeer {
    pub node_id: NodeId,
    pub public_key: [u8; 32],
    pub agreement_public_key: [u8; 32],
}

/// Domain-separated message signed by a binding.
///
/// The prefix stops a signature made for one purpose being replayed as another: without
/// it, a signature over a 32-byte agreement key could be presented as a signature over any
/// other 32-byte value the protocol happens to sign.
fn binding_message(agreement_public_key: &[u8; 32]) -> Vec<u8> {
    let mut m = b"otwono-agreement-binding-v1:".to_vec();
    m.extend_from_slice(agreement_public_key);
    m
}

/// Verify a detached signature made by a known peer.
pub fn verify_signature(
    public_key: &[u8; 32],
    message: &[u8],
    signature: &[u8],
) -> Result<(), IdentityError> {
    let signature: [u8; 64] = signature
        .try_into()
        .map_err(|_| IdentityError::MalformedSignature)?;
    let verifying =
        VerifyingKey::from_bytes(public_key).map_err(|e| IdentityError::MalformedKey(e.to_string()))?;
    verifying
        .verify(message, &Signature::from_bytes(&signature))
        .map_err(|_| IdentityError::BadSignature)
}

pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    data_encoding::BASE64.encode(bytes)
}

pub(crate) fn base64_decode(text: &str) -> Result<Vec<u8>, IdentityError> {
    data_encoding::BASE64
        .decode(text.as_bytes())
        .map_err(|e| IdentityError::MalformedKey(e.to_string()))
}

fn decode_key(text: &str) -> Result<[u8; 32], IdentityError> {
    base64_decode(text)?
        .as_slice()
        .try_into()
        .map_err(|_| IdentityError::MalformedKey("expected 32 bytes".into()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityError {
    Entropy(String),
    MalformedKey(String),
    MalformedSignature,
    BadSignature,
    /// The claimed NodeID is not the hash of the presented key.
    NodeIdMismatch,
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdentityError::Entropy(e) => write!(f, "no usable entropy source: {e}"),
            IdentityError::MalformedKey(e) => write!(f, "malformed key: {e}"),
            IdentityError::MalformedSignature => write!(f, "malformed signature"),
            IdentityError::BadSignature => write!(f, "signature does not verify"),
            IdentityError::NodeIdMismatch => {
                write!(
                    f,
                    "the claimed NodeID is not the hash of the presented public key"
                )
            }
        }
    }
}

impl std::error::Error for IdentityError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(seed: u8) -> NodeIdentity {
        NodeIdentity::from_seeds(&[seed; 32], &[seed.wrapping_add(100); 32], 1_700_000_000_000)
    }

    #[test]
    fn generation_produces_distinct_identities() {
        let a = NodeIdentity::generate().unwrap();
        let b = NodeIdentity::generate().unwrap();
        assert_ne!(a.node_id(), b.node_id());
    }

    #[test]
    fn the_node_id_names_the_signing_key() {
        let id = identity(1);
        assert!(id.node_id().matches_public_key(&id.public_key_bytes()));
    }

    #[test]
    fn identity_is_deterministic_in_the_seed() {
        // Restoring from a backup must reproduce the same node, not a new one.
        assert_eq!(identity(3).node_id(), identity(3).node_id());
    }

    #[test]
    fn the_signing_and_agreement_keys_are_different() {
        // ADR-0006: the long-term key signs, it does not do key agreement.
        let id = identity(2);
        assert_ne!(id.public_key_bytes(), id.agreement_public().to_bytes());
    }

    #[test]
    fn signatures_verify_and_reject_tampering() {
        let id = identity(4);
        let sig = id.sign(b"hello");
        assert!(verify_signature(&id.public_key_bytes(), b"hello", &sig.to_bytes()).is_ok());
        assert_eq!(
            verify_signature(&id.public_key_bytes(), b"hell0", &sig.to_bytes()),
            Err(IdentityError::BadSignature)
        );
    }

    #[test]
    fn a_signature_from_another_node_does_not_verify() {
        let a = identity(5);
        let b = identity(6);
        let sig = a.sign(b"payload");
        assert_eq!(
            verify_signature(&b.public_key_bytes(), b"payload", &sig.to_bytes()),
            Err(IdentityError::BadSignature)
        );
    }

    #[test]
    fn an_agreement_binding_verifies() {
        let id = identity(7);
        let peer = id.agreement_binding().verify().unwrap();
        assert_eq!(peer.node_id, *id.node_id());
        assert_eq!(peer.agreement_public_key, id.agreement_public().to_bytes());
    }

    #[test]
    fn a_binding_claiming_someone_elses_node_id_is_rejected() {
        // The attack this exists to stop: presenting your own key under a NodeID that
        // names somebody else.
        let mut binding = identity(8).agreement_binding();
        binding.node_id = *identity(9).node_id();
        assert_eq!(binding.verify(), Err(IdentityError::NodeIdMismatch));
    }

    #[test]
    fn a_binding_with_a_swapped_agreement_key_is_rejected() {
        // Substituting an agreement key you control would let you complete the handshake
        // as somebody else.
        let mut binding = identity(10).agreement_binding();
        binding.agreement_public_key = base64_encode(&identity(11).agreement_public().to_bytes());
        assert_eq!(binding.verify(), Err(IdentityError::BadSignature));
    }

    #[test]
    fn a_binding_signature_is_domain_separated() {
        // A bare signature over the agreement key must not satisfy the binding, or a
        // signature made for another purpose could be replayed as one.
        let id = identity(12);
        let mut binding = id.agreement_binding();
        let naive = id.sign(&id.agreement_public().to_bytes());
        binding.signature = base64_encode(&naive.to_bytes());
        assert_eq!(binding.verify(), Err(IdentityError::BadSignature));
    }

    #[test]
    fn public_identity_self_consistency_catches_a_mismatched_node_id() {
        let mut public = identity(13).to_public();
        assert!(public.is_self_consistent());
        public.node_id = *identity(14).node_id();
        assert!(!public.is_self_consistent());
    }

    #[test]
    fn public_identity_round_trips_as_json() {
        let public = identity(15).to_public();
        let json = serde_json::to_string(&public).unwrap();
        assert_eq!(serde_json::from_str::<PublicIdentity>(&json).unwrap(), public);
    }

    #[test]
    fn debug_output_never_contains_key_material() {
        let id = identity(16);
        let rendered = format!("{id:?}");
        assert!(rendered.contains(&id.node_id().to_text()));
        // The seed is 16 repeated; make sure no base64 of the secret leaks in.
        assert!(!rendered.to_lowercase().contains("secret"), "{rendered}");
        assert!(!rendered.contains(&base64_encode(&[16u8; 32])), "{rendered}");
    }

    #[test]
    fn malformed_signatures_are_reported_distinctly_from_bad_ones() {
        let id = identity(17);
        assert_eq!(
            verify_signature(&id.public_key_bytes(), b"x", &[0u8; 10]),
            Err(IdentityError::MalformedSignature)
        );
    }
}

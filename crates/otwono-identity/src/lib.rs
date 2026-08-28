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

pub mod sharing;
pub use sharing::{
    open_with, seal_to, sharing_binding_message, SealedKey, SharingBinding, SharingKey, SEAL_DOMAIN,
};

pub mod keystore;
pub mod node_id;
pub mod signer;

pub use keystore::{
    migrate_combined, AgreementKeystore, KeystoreError, SharingKeystore, SigningKeystore, StoredAgreementKey,
    StoredSharingKey, StoredSigningKey, SuccessionRecord, AGREEMENT_KEY_FILE, DEFAULT_IDENTITY_DIR,
    SHARING_KEY_FILE, SIGNING_KEY_FILE,
};
pub use node_id::{NodeId, NodeIdError};
pub use signer::{
    domain_separated, session_proof_message, SessionSigner, SignerError, APPLICATION_DOMAIN,
    HANDSHAKE_HASH_LEN, SESSION_DOMAIN,
};

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

    /// Vouch for this node's sharing key (ADR-0019).
    ///
    /// The same shape as [`SigningIdentity::bind_agreement`] and for the same reason: an
    /// X25519 public key says nothing about whose it is, and a recipient list names NodeIDs.
    /// Sharing to `otw1:...` without this would mean sharing to whichever key somebody
    /// claimed was theirs.
    pub fn bind_sharing(&self, sharing_public_key: &[u8; 32]) -> sharing::SharingBinding {
        let signature = self.sign(&sharing::sharing_binding_message(sharing_public_key));
        sharing::SharingBinding {
            node_id: self.node_id,
            public_key: base64_encode(&self.public_key_bytes()),
            sharing_public_key: base64_encode(sharing_public_key),
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
            sharing_binding: None,
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
    /// The node's *signed* sharing binding (ADR-0019), absent on a node that has none.
    ///
    /// The whole binding rather than a bare key, because nothing signs a `PublicIdentity`
    /// as a whole: its `node_id` can be checked against its `public_key`, and that is all.
    /// A bare `sharing_public_key` field would therefore be a key anyone could substitute,
    /// and sealing to it would seal to them. The binding carries its own signature, so
    /// [`verified_sharing_key`](PublicIdentity::verified_sharing_key) can answer the only
    /// question a sender has: which key may I seal to for this NodeID?
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing_binding: Option<SharingBinding>,
    pub created_at_unix_ms: u64,
}

impl PublicIdentity {
    pub fn public_key_bytes(&self) -> Result<[u8; 32], IdentityError> {
        decode_key(&self.public_key)
    }

    /// Check the NodeID actually names the key. Always call this on anything received.
    ///
    /// This says nothing about `agreement_public_key`, which carries no signature here —
    /// the trustworthy form of that is an [`AgreementBinding`]. It says nothing about the
    /// sharing key either; use [`verified_sharing_key`](Self::verified_sharing_key).
    pub fn is_self_consistent(&self) -> bool {
        match self.public_key_bytes() {
            Ok(k) => self.node_id.matches_public_key(&k),
            Err(_) => false,
        }
    }

    /// The key it is safe to seal a content key to for this node, if it published one.
    ///
    /// Verifies the binding and checks it belongs to *this* identity, so a published
    /// record cannot carry someone else's perfectly valid binding and have a sender seal
    /// to them instead. `Ok(None)` means the node has no sharing key — a node that cannot
    /// be shared with, which is different from one whose binding does not check out.
    pub fn verified_sharing_key(&self) -> Result<Option<[u8; 32]>, IdentityError> {
        let Some(binding) = &self.sharing_binding else {
            return Ok(None);
        };
        if binding.node_id != self.node_id {
            return Err(IdentityError::NodeIdMismatch);
        }
        binding.verify().map(Some)
    }

    /// Publish a sharing binding alongside this identity.
    #[must_use]
    pub fn with_sharing_binding(mut self, binding: SharingBinding) -> Self {
        self.sharing_binding = Some(binding);
        self
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

/// The agreement binding's message, exposed to tests so the sharing module can assert the
/// two domains differ. A shared helper would defeat the point of them being different.
#[cfg(test)]
pub(crate) fn tests_binding_message(k: &[u8; 32]) -> Vec<u8> {
    binding_message(k)
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
    fn a_published_identity_carrying_someone_elses_binding_is_refused() {
        // The binding is valid — it is just not this node's. Without the ownership check,
        // a sender would seal to the other node's key and the named recipient could never
        // open it, while the substituting party could.
        let mine = identity(16);
        let theirs = identity(17);
        let theirs_sharing = SharingKey::from_seed(&[17u8; 32], 1_700_000_000_000);
        let borrowed = theirs.signing().bind_sharing(&theirs_sharing.public());
        assert!(borrowed.verify().is_ok(), "the binding itself is genuine");

        let public = mine.to_public().with_sharing_binding(borrowed);
        assert_eq!(public.verified_sharing_key(), Err(IdentityError::NodeIdMismatch));
    }

    #[test]
    fn a_published_identity_with_a_swapped_sharing_key_is_refused() {
        let mine = identity(18);
        let sharing = SharingKey::from_seed(&[18u8; 32], 1_700_000_000_000);
        let mut binding = mine.signing().bind_sharing(&sharing.public());
        let attacker = SharingKey::from_seed(&[19u8; 32], 1_700_000_000_000);
        binding.sharing_public_key = base64_encode(&attacker.public());

        let public = mine.to_public().with_sharing_binding(binding);
        assert_eq!(public.verified_sharing_key(), Err(IdentityError::BadSignature));
    }

    #[test]
    fn a_published_identity_without_a_binding_is_unshareable_not_broken() {
        let public = identity(20).to_public();
        assert!(public.is_self_consistent());
        assert_eq!(public.verified_sharing_key(), Ok(None));
    }

    #[test]
    fn a_published_identity_round_trips_with_its_binding() {
        let mine = identity(21);
        let sharing = SharingKey::from_seed(&[21u8; 32], 1_700_000_000_000);
        let public = mine
            .to_public()
            .with_sharing_binding(mine.signing().bind_sharing(&sharing.public()));
        let json = serde_json::to_string(&public).unwrap();
        let back: PublicIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(back, public);
        assert_eq!(back.verified_sharing_key(), Ok(Some(sharing.public())));
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

/// Canonical JSON encoding, for anything whose meaning is fixed by a signature.
///
/// # Why this exists here rather than beside its first caller
///
/// Two canonicalizers is one more than the system can have. If the model manifest and the
/// pointer record each had their own, a divergence between them would not be a compile
/// error or a test failure — it would be signatures that verify in one crate and not the
/// other, discovered by a user whose data would not load. So there is one, in the crate that
/// owns signing, and everything that signs structured data uses it.
///
/// # Why it is written out rather than delegated
///
/// `serde_json`'s object type preserves insertion order or sorts, depending on the
/// `preserve_order` feature — which any transitive dependency can turn on. Delegating would
/// tie the meaning of every signature in OTWONO to a Cargo feature nobody is watching
/// (ADR-0011, and now ADR-0027 §5).
///
/// Keys are sorted, arrays keep their order (their order is data), and there is no
/// insignificant whitespace.
pub fn canonical_json(value: &serde_json::Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &serde_json::Value, out: &mut Vec<u8>) {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_json_string(key, out);
                out.push(b':');
                write_canonical(&map[*key], out);
            }
            out.push(b'}');
        }
        serde_json::Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        serde_json::Value::String(s) => write_json_string(s, out),
        // Every signed record in OTWONO has integer fields only, so the textual form of a
        // number is unambiguous. A float would not be, and adding one to a signed record
        // needs this function revisited rather than trusted.
        other => out.extend_from_slice(other.to_string().as_bytes()),
    }
}

fn write_json_string(s: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(
        serde_json::to_string(s)
            .expect("a string always serializes")
            .as_bytes(),
    );
}

#[cfg(test)]
mod canonical_tests {
    use super::canonical_json;

    #[test]
    fn keys_are_sorted_and_arrays_are_not() {
        let value = serde_json::json!({ "z": 1, "a": { "y": [3, 2], "b": "x" } });
        assert_eq!(
            String::from_utf8(canonical_json(&value)).unwrap(),
            r#"{"a":{"b":"x","y":[3,2]},"z":1}"#,
            "keys sorted, array order preserved, no whitespace"
        );
    }

    #[test]
    fn the_same_content_in_a_different_order_canonicalizes_the_same() {
        // The failure this prevents: a record re-serialized by another tool, with the same
        // content in a different order, failing to verify.
        let a = serde_json::json!({ "one": 1, "two": 2, "three": 3 });
        let b: serde_json::Value = serde_json::from_str(r#"{"three":3,"one":1,"two":2}"#).unwrap();
        assert_eq!(canonical_json(&a), canonical_json(&b));
    }

    #[test]
    fn strings_that_need_escaping_are_escaped_once() {
        let value = serde_json::json!({ "k": "a\"b\\c\nd" });
        assert_eq!(
            String::from_utf8(canonical_json(&value)).unwrap(),
            r#"{"k":"a\"b\\c\nd"}"#
        );
    }
}

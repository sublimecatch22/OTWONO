//! Noise-secured channels with identity binding.
//!
//! The handshake is `Noise_XX_25519_ChaChaPoly_BLAKE2s`: mutual authentication, forward
//! secrecy, and static keys that are not revealed to a passive observer. `snow` implements
//! the protocol; this module does the part Noise deliberately leaves open — deciding *who*
//! the authenticated key belongs to.
//!
//! # Why the handshake alone is not enough
//!
//! Noise XX proves the peer holds the private half of some X25519 static key. It says
//! nothing about which node that is. A node's name is derived from its **Ed25519** key, so
//! after the handshake each side sends a [`SessionProof`]:
//!
//! 1. an [`AgreementBinding`] — the Ed25519 key signing "this X25519 key is mine", with
//!    the NodeID being the hash of that Ed25519 key;
//! 2. a signature over this session's Noise handshake hash.
//!
//! The receiver checks all three links in the chain: the NodeID names the signing key, the
//! signing key vouches for the agreement key, and **the agreement key in the binding is the
//! static key Noise actually authenticated**. Skipping that last check is the classic
//! mistake — it would let anyone replay a binding they had merely observed.
//!
//! The handshake-hash signature is what makes the proof specific to this session. Without
//! it the binding is a standing document, and an attacker who obtained the agreement key
//! alone could present a genuine binding forever.
//!
//! # Where the keys live
//!
//! Only the X25519 secret has to be in this process — `snow` drives the key agreement.
//! The two Ed25519 signatures come from a [`SessionSigner`], which may sign locally or
//! call `otwono-idd` over the control plane (ADR-0010). That is why building a proof is
//! fallible: a node whose signing key is unreachable must abandon the handshake, never
//! continue unauthenticated.

use crate::link::{LinkAdapter, LinkError};
use otwono_identity::{AgreementBinding, SessionSigner, SignerError, VerifiedPeer};
use serde::{Deserialize, Serialize};

pub use otwono_identity::{session_proof_message, SESSION_DOMAIN};

/// The Noise pattern. Changing this is a wire-compatibility break.
pub const NOISE_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// Largest frame accepted during the handshake. Noise messages are far smaller; the cap
/// stops a peer forcing a large allocation before it has authenticated.
const MAX_HANDSHAKE_FRAME: usize = 8 * 1024;
/// Noise's own transport limit.
const MAX_NOISE_MESSAGE: usize = 65535;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionProof {
    pub binding: AgreementBinding,
    /// Base64 Ed25519 signature over `SESSION_DOMAIN || handshake_hash`.
    pub handshake_signature: String,
}

/// An authenticated, encrypted channel to one peer.
pub struct SecureChannel<L: LinkAdapter> {
    link: L,
    transport: snow::TransportState,
    peer: VerifiedPeer,
}

impl<L: LinkAdapter> std::fmt::Debug for SecureChannel<L> {
    /// Peer identity only. The transport state holds session keys and must never be
    /// rendered, however convenient that would be while debugging.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureChannel")
            .field("peer", &self.peer.node_id.to_text())
            .finish_non_exhaustive()
    }
}

impl<L: LinkAdapter> SecureChannel<L> {
    /// Open a channel as the initiator.
    pub fn initiate(link: L, signer: &dyn SessionSigner) -> Result<Self, HandshakeError> {
        Self::handshake(link, signer, true)
    }

    /// Accept a channel as the responder.
    pub fn accept(link: L, signer: &dyn SessionSigner) -> Result<Self, HandshakeError> {
        Self::handshake(link, signer, false)
    }

    fn handshake(mut link: L, signer: &dyn SessionSigner, initiator: bool) -> Result<Self, HandshakeError> {
        let params = NOISE_PATTERN
            .parse()
            .map_err(|e| HandshakeError::Noise(format!("{e:?}")))?;
        let static_secret = signer.agreement_secret();
        let builder = snow::Builder::new(params).local_private_key(static_secret.as_ref());
        let mut state = if initiator {
            builder.build_initiator()
        } else {
            builder.build_responder()
        }
        .map_err(|e| HandshakeError::Noise(format!("{e:?}")))?;

        let mut buffer = vec![0u8; MAX_HANDSHAKE_FRAME];

        // XX is three messages: -> e, <- e ee s es, -> s se
        if initiator {
            write_noise(&mut state, &mut link, &mut buffer)?;
            read_noise(&mut state, &mut link, &mut buffer)?;
            write_noise(&mut state, &mut link, &mut buffer)?;
        } else {
            read_noise(&mut state, &mut link, &mut buffer)?;
            write_noise(&mut state, &mut link, &mut buffer)?;
            read_noise(&mut state, &mut link, &mut buffer)?;
        }

        // Capture these before the state is consumed: the hash binds the proof to this
        // session, and the remote static is what the binding must match.
        let handshake_hash = state.get_handshake_hash().to_vec();
        let remote_static: [u8; 32] = state
            .get_remote_static()
            .ok_or(HandshakeError::NoRemoteStatic)?
            .try_into()
            .map_err(|_| HandshakeError::NoRemoteStatic)?;

        let mut transport = state
            .into_transport_mode()
            .map_err(|e| HandshakeError::Noise(format!("{e:?}")))?;

        // Exchange proofs over the now-encrypted channel. The initiator speaks first, so
        // both sides agree on the order and neither blocks.
        let proof = build_proof(signer, &handshake_hash)?;
        let peer = if initiator {
            send_encrypted(&mut transport, &mut link, &proof)?;
            let theirs: SessionProof = recv_encrypted(&mut transport, &mut link)?;
            verify_proof(&theirs, &remote_static, &handshake_hash)?
        } else {
            let theirs: SessionProof = recv_encrypted(&mut transport, &mut link)?;
            let peer = verify_proof(&theirs, &remote_static, &handshake_hash)?;
            send_encrypted(&mut transport, &mut link, &proof)?;
            peer
        };

        Ok(SecureChannel {
            link,
            transport,
            peer,
        })
    }

    /// The authenticated peer. Authenticated is not trusted — that is a separate,
    /// user-visible decision.
    pub fn peer(&self) -> &VerifiedPeer {
        &self.peer
    }

    pub fn send(&mut self, message: &[u8]) -> Result<(), HandshakeError> {
        let mut buffer = vec![0u8; message.len() + 16];
        let n = self
            .transport
            .write_message(message, &mut buffer)
            .map_err(|e| HandshakeError::Noise(format!("{e:?}")))?;
        self.link.send(&buffer[..n]).map_err(HandshakeError::Link)
    }

    pub fn recv(&mut self) -> Result<Vec<u8>, HandshakeError> {
        let frame = self.link.recv().map_err(HandshakeError::Link)?;
        if frame.len() > MAX_NOISE_MESSAGE {
            return Err(HandshakeError::FrameTooLarge(frame.len()));
        }
        let mut buffer = vec![0u8; frame.len()];
        let n = self
            .transport
            .read_message(&frame, &mut buffer)
            .map_err(|e| HandshakeError::Noise(format!("{e:?}")))?;
        buffer.truncate(n);
        Ok(buffer)
    }
}

/// Build this side's proof.
///
/// Both halves can fail when the signer is remote: the binding is fetched and the
/// signature is a control-plane call. A node that cannot prove who it is must abandon the
/// handshake rather than continue unauthenticated.
fn build_proof(signer: &dyn SessionSigner, handshake_hash: &[u8]) -> Result<SessionProof, HandshakeError> {
    let binding = signer.agreement_binding().map_err(HandshakeError::Signer)?;
    let signature = signer
        .sign_session(handshake_hash)
        .map_err(HandshakeError::Signer)?;
    Ok(SessionProof {
        binding,
        handshake_signature: data_encoding::BASE64.encode(&signature),
    })
}

/// Check every link in the chain from NodeID to the key Noise actually authenticated.
fn verify_proof(
    proof: &SessionProof,
    remote_static: &[u8; 32],
    handshake_hash: &[u8],
) -> Result<VerifiedPeer, HandshakeError> {
    let peer = proof.binding.verify().map_err(HandshakeError::Identity)?;

    // Without this, a binding observed on the wire could be replayed by anyone.
    if &peer.agreement_public_key != remote_static {
        return Err(HandshakeError::BindingDoesNotMatchHandshake);
    }

    let signature = data_encoding::BASE64
        .decode(proof.handshake_signature.as_bytes())
        .map_err(|e| HandshakeError::Malformed(e.to_string()))?;
    otwono_identity::verify_signature(
        &peer.public_key,
        &session_proof_message(handshake_hash),
        &signature,
    )
    .map_err(|_| HandshakeError::StaleOrForgedSessionProof)?;

    Ok(peer)
}

fn write_noise<L: LinkAdapter>(
    state: &mut snow::HandshakeState,
    link: &mut L,
    buffer: &mut [u8],
) -> Result<(), HandshakeError> {
    let n = state
        .write_message(&[], buffer)
        .map_err(|e| HandshakeError::Noise(format!("{e:?}")))?;
    link.send(&buffer[..n]).map_err(HandshakeError::Link)
}

fn read_noise<L: LinkAdapter>(
    state: &mut snow::HandshakeState,
    link: &mut L,
    buffer: &mut [u8],
) -> Result<(), HandshakeError> {
    let frame = link.recv().map_err(HandshakeError::Link)?;
    if frame.len() > MAX_HANDSHAKE_FRAME {
        return Err(HandshakeError::FrameTooLarge(frame.len()));
    }
    state
        .read_message(&frame, buffer)
        .map_err(|e| HandshakeError::Noise(format!("{e:?}")))?;
    Ok(())
}

fn send_encrypted<L: LinkAdapter, T: Serialize>(
    transport: &mut snow::TransportState,
    link: &mut L,
    value: &T,
) -> Result<(), HandshakeError> {
    let json = serde_json::to_vec(value).map_err(|e| HandshakeError::Malformed(e.to_string()))?;
    let mut buffer = vec![0u8; json.len() + 16];
    let n = transport
        .write_message(&json, &mut buffer)
        .map_err(|e| HandshakeError::Noise(format!("{e:?}")))?;
    link.send(&buffer[..n]).map_err(HandshakeError::Link)
}

fn recv_encrypted<L: LinkAdapter, T: for<'de> Deserialize<'de>>(
    transport: &mut snow::TransportState,
    link: &mut L,
) -> Result<T, HandshakeError> {
    let frame = link.recv().map_err(HandshakeError::Link)?;
    if frame.len() > MAX_NOISE_MESSAGE {
        return Err(HandshakeError::FrameTooLarge(frame.len()));
    }
    let mut buffer = vec![0u8; frame.len()];
    let n = transport
        .read_message(&frame, &mut buffer)
        .map_err(|e| HandshakeError::Noise(format!("{e:?}")))?;
    serde_json::from_slice(&buffer[..n]).map_err(|e| HandshakeError::Malformed(e.to_string()))
}

#[derive(Debug)]
pub enum HandshakeError {
    Noise(String),
    Link(LinkError),
    Identity(otwono_identity::IdentityError),
    /// This node could not produce its own proof — the signing key is unreachable.
    Signer(SignerError),
    Malformed(String),
    NoRemoteStatic,
    /// The peer's binding names an agreement key that is not the one it authenticated with.
    BindingDoesNotMatchHandshake,
    /// The session signature does not cover this handshake — replayed or forged.
    StaleOrForgedSessionProof,
    FrameTooLarge(usize),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::Noise(e) => write!(f, "noise handshake failed: {e}"),
            HandshakeError::Link(e) => write!(f, "{e}"),
            HandshakeError::Identity(e) => write!(f, "peer identity invalid: {e}"),
            HandshakeError::Signer(e) => write!(f, "cannot prove this node's own identity: {e}"),
            HandshakeError::Malformed(e) => write!(f, "malformed handshake payload: {e}"),
            HandshakeError::NoRemoteStatic => {
                write!(f, "the handshake completed without a remote static key")
            }
            HandshakeError::BindingDoesNotMatchHandshake => write!(
                f,
                "the peer's identity binding names a different agreement key than the one it \
                 authenticated with; the binding was replayed"
            ),
            HandshakeError::StaleOrForgedSessionProof => {
                write!(f, "the peer's session signature does not cover this handshake")
            }
            HandshakeError::FrameTooLarge(n) => write!(f, "frame of {n} bytes is too large"),
        }
    }
}

impl std::error::Error for HandshakeError {}

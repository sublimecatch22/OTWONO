//! OTWONO Node Mesh transport.
//!
//! The bottom of the ONM stack (docs/network/NODE-NETWORK.md): a uniform [`link`]
//! abstraction over every medium, and a [`secure`] channel that authenticates the node
//! behind a Noise handshake rather than merely the key.

#![forbid(unsafe_code)]

pub mod content;
pub mod discovery;
pub mod link;
pub mod memory;
pub mod peer;
pub mod secure;
pub mod tcp;

pub use content::{
    decode, encode, is_hex_digest, max_body_bytes, max_chunks_per_page, ChunkEntry, ChunkPart, ManifestPage,
    ProtocolError, Request, Response, MAX_CHUNKS_PER_REQUEST, MAX_CHUNK_BYTES, MAX_NOISE_PLAINTEXT,
    PROTOCOL_VERSION,
};
pub use discovery::{Candidate, Discovery, DiscoveryError, SERVICE_TYPE};
pub use link::{BandwidthClass, DutyCycle, EnergyCost, LinkAdapter, LinkError, LinkKind, LinkProperties};
pub use memory::MemoryLink;
pub use peer::{should_initiate, PeerRecord, PeerState, PeerTable};
pub use secure::{
    session_proof_message, HandshakeError, SecureChannel, SessionProof, NOISE_PATTERN, SESSION_DOMAIN,
};
pub use tcp::TcpLink;

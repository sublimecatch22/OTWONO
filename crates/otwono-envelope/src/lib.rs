//! Addressed messages: what a relay can see, and the rules it follows (ADR-0028).
//!
//! # This crate is not the envelope's contents
//!
//! The payload is an ADR-0019 sealed object — encrypted under a fresh per-object key, that
//! key wrapped by X25519 to the recipient's sharing key — and none of that is here. Building
//! a second sealing path would be the rewrite CLAUDE.md §2.3 forbids, and the existing one
//! is verified between booted nodes.
//!
//! What *is* here is the small descriptor a carrier needs in order to do its job without
//! opening anything: who it is for, how big it is, and when it stops mattering. A relay sees
//! exactly this and no more.
//!
//! # Why there is no sender field
//!
//! A relay must know the recipient — it cannot decide whether to carry an envelope, or whom
//! to hand it to, otherwise. It has no use at all for the sender, so the sender lives inside
//! the ciphertext where only the recipient can read it (ADR-0028 §4).
//!
//! That is not anonymity and this crate does not pretend it is: a relay still learns that a
//! node receives traffic, how much, and when.

#![forbid(unsafe_code)]

use otwono_identity::NodeId;
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: &str = "1.0.0";

/// What a carrier is told about an envelope it has not opened.
///
/// Deliberately the same shape as `otwono_store::object::Replication`'s wire form: a relay
/// pass is a replication pass whose offer is filtered by address rather than by label
/// (ADR-0028 §2), and two records that mean "here is something you might agree to hold"
/// should not look different for no reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub schema_version: String,
    /// The content id of the ciphertext, as for any object.
    pub envelope_id: String,
    /// The NodeID this is for, in `otw1…` text form.
    pub recipient: String,
    /// Ciphertext size, so a carrier can decide before fetching.
    pub size_bytes: u64,
    /// When this stops existing — absolute, not a duration.
    ///
    /// A replica's TTL restarts when the object is offered again, because replication is
    /// about durability. A message is the opposite: it has a moment, and afterwards the
    /// envelope should be gone from the relay's disk too. So this is set once by the sender
    /// and a carrier may only ever bring it closer (ADR-0028 §3).
    pub expires_at_ms: u64,
}

impl Envelope {
    pub fn new(envelope_id: &str, recipient: &NodeId, size_bytes: u64, expires_at_ms: u64) -> Envelope {
        Envelope {
            schema_version: SCHEMA_VERSION.to_string(),
            envelope_id: envelope_id.to_string(),
            recipient: recipient.to_text(),
            size_bytes,
            expires_at_ms,
        }
    }

    /// Check the shape of a descriptor that arrived from a stranger.
    ///
    /// Called before anything acts on it. Every field here is chosen by whoever offered the
    /// envelope, including the recipient, so none of it is trusted until it parses.
    pub fn validate(&self) -> Result<(), EnvelopeError> {
        if self.schema_version.is_empty() {
            return Err(EnvelopeError::Malformed("no schema_version".into()));
        }
        // 64 lowercase hex, the same shape every content id in this system has. Checked
        // rather than assumed: this string reaches a filesystem lookup and a fetch request.
        let id_ok = self.envelope_id.len() == 64
            && self
                .envelope_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !id_ok {
            return Err(EnvelopeError::Malformed(
                "envelope_id must be 64 lowercase hex characters".into(),
            ));
        }
        NodeId::parse(&self.recipient).map_err(|e| EnvelopeError::Malformed(format!("recipient: {e}")))?;
        if self.size_bytes == 0 {
            return Err(EnvelopeError::Malformed(
                "size_bytes must be greater than zero".into(),
            ));
        }
        Ok(())
    }

    /// Whether this envelope is for the given node.
    ///
    /// Compared as parsed NodeIDs rather than as strings. A text form that differs only in
    /// case or padding is the same node, and a carrier that decided otherwise would silently
    /// hold mail it would never hand over.
    pub fn is_for(&self, node: &NodeId) -> bool {
        NodeId::parse(&self.recipient)
            .map(|r| &r == node)
            .unwrap_or(false)
    }

    pub fn is_expired(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_at_ms
    }
}

/// What a carrier decided about one offered envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Carry {
    /// Take it, holding it until this moment and no longer.
    ///
    /// The expiry here is what the carrier committed to, which may be sooner than the
    /// sender asked for and is never later (ADR-0028 §3).
    Accept {
        until_ms: u64,
    },
    Decline(Declined),
}

/// Why a carrier will not hold an envelope.
///
/// Named cases rather than a bool, because they are different situations for an operator
/// reading a log: one is a machine that is full, one is a message that is too late, and one
/// is a peer sending nonsense.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Declined {
    /// Already past its expiry. Nothing to do but drop it.
    Expired,
    /// Larger than this carrier will hold for anyone.
    TooLarge {
        size_bytes: u64,
        ceiling_bytes: u64,
    },
    /// No room under this carrier's budget right now.
    NoRoom {
        size_bytes: u64,
        room_bytes: u64,
    },
    Malformed(String),
}

impl Declined {
    /// The stable short name for the wire and for logs: `expired`, `too_large`, `no_room`,
    /// `malformed`. Matching on this is what a caller should do; the [`std::fmt::Display`]
    /// text is for a human and may change.
    pub fn code(&self) -> &'static str {
        match self {
            Declined::Expired => "expired",
            Declined::TooLarge { .. } => "too_large",
            Declined::NoRoom { .. } => "no_room",
            Declined::Malformed(_) => "malformed",
        }
    }
}

impl std::fmt::Display for Declined {
    /// The numbers, not just the name. `no_room` on its own tells an operator nothing about
    /// whether the envelope was enormous or the disk was full, and a refusal that cannot be
    /// diagnosed from a log line costs a whole run to reproduce.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Declined::Expired => write!(f, "expired"),
            Declined::TooLarge { size_bytes, ceiling_bytes } => {
                write!(f, "too_large: {size_bytes} bytes, ceiling {ceiling_bytes}")
            }
            Declined::NoRoom { size_bytes, room_bytes } => {
                write!(f, "no_room: {size_bytes} bytes, {room_bytes} free")
            }
            Declined::Malformed(why) => write!(f, "malformed: {why}"),
        }
    }
}

/// What a node is willing to carry for other people.
///
/// Held by the carrier, never sent by the sender — which is the whole point of ADR-0028 §2.
/// A sender offers; the carrier decides against its own limits; nothing is ever pushed onto
/// a disk whose owner did not agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CarryPolicy {
    /// Bytes available for other people's envelopes right now.
    pub room_bytes: u64,
    /// The largest single envelope this node will hold, whatever the room says. A cheap
    /// guard against one message filling a small node, as ADR-0026 §7 has for replicas.
    pub max_size_bytes: u64,
    /// The furthest ahead this carrier will commit to holding anything.
    ///
    /// A ceiling on the *duration*, applied from now. A sender that asks for a year gets
    /// this instead; a sender that asks for an hour gets an hour.
    pub max_hold_ms: u64,
}

/// The largest single envelope any carrier will hold.
///
/// A message is small. This is comfortably above any plausible text message with a modest
/// attachment, and comfortably below the point where one envelope could dominate a carrier —
/// it is a sixty-fourth of a T0 node's whole carriage budget, so even the smallest machine
/// can hold dozens of them rather than one.
///
/// Not in `FeatureGates`, deliberately. The *budget* varies with the machine (ADR-0028 §8);
/// what counts as a message does not, and a per-tier answer would mean a T0 node refusing
/// mail that a T3 node would carry, which makes delivery depend on which carrier you happen
/// to meet.
pub const MAX_ENVELOPE_BYTES: u64 = 1024 * 1024;

/// The furthest ahead any carrier will commit to holding an envelope.
///
/// A week. Long enough that a laptop shut for a holiday still collects its mail; short enough
/// that a carrier's disk is not an archive. A sender may ask for less and gets less; a sender
/// asking for more gets this (ADR-0028 §3).
pub const MAX_HOLD_MS: u64 = 7 * 24 * 60 * 60 * 1000;

impl CarryPolicy {
    /// The terms a carrier offers, given the room it has left.
    ///
    /// One constructor so the two ceilings are not restated at each call site and cannot
    /// drift apart between the daemon and its tests.
    pub fn with_room(room_bytes: u64) -> CarryPolicy {
        CarryPolicy {
            room_bytes,
            max_size_bytes: MAX_ENVELOPE_BYTES,
            max_hold_ms: MAX_HOLD_MS,
        }
    }

    /// Decide whether to carry one offered envelope.
    ///
    /// The single place ADR-0028 §3's rule lives: the committed expiry is the *earlier* of
    /// what the sender asked for and what this carrier will commit to. Writing it once means
    /// there is no second path where a relay could extend a message's life, which is the
    /// direction that turns a message network into an archive nobody asked for.
    pub fn decide(&self, envelope: &Envelope, now_ms: u64) -> Carry {
        if let Err(e) = envelope.validate() {
            return Carry::Decline(Declined::Malformed(e.to_string()));
        }
        if envelope.is_expired(now_ms) {
            return Carry::Decline(Declined::Expired);
        }
        if envelope.size_bytes > self.max_size_bytes {
            return Carry::Decline(Declined::TooLarge {
                size_bytes: envelope.size_bytes,
                ceiling_bytes: self.max_size_bytes,
            });
        }
        if envelope.size_bytes > self.room_bytes {
            return Carry::Decline(Declined::NoRoom {
                size_bytes: envelope.size_bytes,
                room_bytes: self.room_bytes,
            });
        }
        Carry::Accept {
            until_ms: envelope
                .expires_at_ms
                .min(now_ms.saturating_add(self.max_hold_ms)),
        }
    }
}

/// One envelope a carrier has taken custody of.
///
/// The difference between this and an [`Envelope`] is `until_ms`, and it is the whole of
/// ADR-0028 §10. The sender's `expires_at_ms` is a wall-clock instant on *the sender's* clock;
/// this mesh has no NTP guarantee, so comparing it to a carrier's clock later is comparing two
/// numbers that disagree for reasons nobody can see.
///
/// `until_ms` is the deadline the carrier **committed to at the moment it took custody**, and
/// it is what the sweep evaluates. The sender's expiry is a ceiling inside it and is never
/// exceeded (§3); the second term is measured from this carrier's own custody moment, so a
/// carrier with a skewed clock still drops the envelope in bounded time rather than holding
/// it for ever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Custody {
    pub schema_version: String,
    pub envelope: Envelope,
    /// When this carrier took it, on this carrier's clock.
    pub took_at_ms: u64,
    /// When this carrier will drop it, on this carrier's clock. Never later than the
    /// sender's `expires_at_ms`.
    pub until_ms: u64,
}

impl Custody {
    /// Record what a carrier committed to when it accepted an envelope.
    ///
    /// Takes the accepted deadline rather than recomputing it, so there is exactly one place
    /// the min-rule lives ([`CarryPolicy::decide`]) and no second path that could disagree
    /// with the decision that was actually made.
    pub fn taken(envelope: &Envelope, took_at_ms: u64, until_ms: u64) -> Custody {
        Custody {
            schema_version: SCHEMA_VERSION.to_string(),
            envelope: envelope.clone(),
            took_at_ms,
            // Belt and braces against a caller that computed its own deadline: the sender's
            // ceiling is re-applied here, so no code path can lengthen an envelope's life by
            // constructing a Custody directly.
            until_ms: until_ms.min(envelope.expires_at_ms),
        }
    }

    /// Whether this carrier should drop it now, on this carrier's clock.
    ///
    /// Evaluated against the committed deadline, never against the sender's field re-read
    /// later. That is the point of §10.
    pub fn is_due(&self, now_ms: u64) -> bool {
        now_ms >= self.until_ms
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    Malformed(String),
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvelopeError::Malformed(m) => write!(f, "malformed envelope: {m}"),
        }
    }
}

impl std::error::Error for EnvelopeError {}

//! Where a carrier keeps envelopes it is holding for other people (ADR-0028).
//!
//! # This store holds custody, not content
//!
//! What is here is the **custody record**: which envelope, for whom, and until when *this
//! carrier* said it would keep it. The sealed ciphertext is an ordinary object and lives
//! where objects live.
//!
//! That split is ADR-0026 §8's, and it is deliberate reuse rather than a shortcut. A carrier
//! that invented its own byte store would be re-implementing eviction, encryption and
//! refcounting to hold bytes the object layer already knows how to hold.
//!
//! ## Where the ciphertext actually goes, and why that is a defect
//!
//! ADR-0026 §8 puts it in the **cluster cache**, which has a budget, a TTL and an eviction
//! policy. `otwono-netd` does not: it calls `store.accept_shared`, which writes to the
//! permanent content store, and nothing in this repository can delete an object from there
//! — the cache has `remove` and `purge`, the CAS has neither.
//!
//! So [`Self::release`] frees the custody record and the carriage budget with it, and leaves
//! the bytes on the carrier's disk permanently. Expiry does the same. A carrier's footprint
//! therefore grows without bound, driven by what remote peers ask it to carry, which is the
//! amplification ADR-0028 §7 exists to bound.
//!
//! Recorded rather than quietly fixed here because the fix is in the cache and the store
//! daemon, not in this file: the cache inserts replicas as `REPLICATED` with no sharing
//! metadata, and a carried envelope is `SHARED` and has some. `docs/network/CARRIAGE.md` §7
//! carries the same note.
//!
//! # The deadline is the carrier's, not the sender's
//!
//! `Custody::until_ms` is what the sweep evaluates, and it is a wall-clock value on *this*
//! carrier's clock, committed when custody was taken (ADR-0028 §10). The sender's
//! `expires_at_ms` is a ceiling inside it. Re-reading the sender's field later and comparing
//! it to this carrier's clock would be comparing two clocks that a mesh with no NTP gives no
//! reason to agree.
//!
//! # Names from the wire never become paths
//!
//! An envelope id is 64 hex characters and is validated as such before anything here sees it,
//! but the on-disk name is the hash of the key anyway — the same rule the pointer store
//! follows, for the same reason: sanitising means maintaining a list of what is dangerous on
//! every filesystem this ever runs on, and the first entry missing from that list is a file
//! the node overwrites.

use otwono_envelope::{Carry, CarryPolicy, Custody, Declined, Envelope};
use std::path::{Path, PathBuf};

pub const DEFAULT_ENVELOPE_DIR: &str = "/var/lib/otwono/envelopes";

/// The custody records this node holds.
#[derive(Debug)]
pub struct EnvelopeStore {
    root: PathBuf,
}

impl EnvelopeStore {
    pub fn at(root: impl AsRef<Path>) -> Result<EnvelopeStore, EnvelopeStoreError> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(root.join("held"))?;
        Ok(EnvelopeStore { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Take custody, if this carrier's own terms allow it.
    ///
    /// The decision is [`CarryPolicy::decide`]'s and is not repeated here — one place for the
    /// rule, so no second path can disagree with the decision that was actually made. This
    /// adds durability and nothing else.
    ///
    /// Returns the refusal rather than an error when the answer is simply "not this one": a
    /// full node saying no is the normal case on a small machine and not something to
    /// escalate, exactly as `take_replica` returns `Ok(None)` (ADR-0026 §8).
    pub fn take(
        &self,
        envelope: &Envelope,
        policy: &CarryPolicy,
        now_ms: u64,
    ) -> Result<Result<Custody, Declined>, EnvelopeStoreError> {
        match policy.decide(envelope, now_ms) {
            Carry::Decline(why) => Ok(Err(why)),
            Carry::Accept { until_ms } => {
                let held = Custody::taken(envelope, now_ms, until_ms);
                write_atomically(&self.path_for(&envelope.envelope_id), &serde_json::to_vec(&held)?)?;
                Ok(Ok(held))
            }
        }
    }

    /// Every envelope still in custody, oldest deadline first, with lapsed ones swept.
    ///
    /// The sweep happens here rather than on a timer, for ADR-0026 §9's reason: a subsystem
    /// that needs a timer needs a timer that runs, and every caller of this is already a
    /// moment when the node is doing carriage work.
    pub fn held(&self, now_ms: u64) -> Result<Vec<Custody>, EnvelopeStoreError> {
        Ok(self.held_and_lapsed(now_ms)?.0)
    }

    /// The same sweep, and the ids it dropped on the way.
    ///
    /// The custody record is one of two things a lapsed envelope leaves behind; the other is
    /// the ciphertext, which since ADR-0031 is in the cache and has to be told. [`Self::held`]
    /// deleted the record and discarded which id it was, so nothing downstream could free the
    /// bytes — an expired envelope became an ordinary cache entry and sat there until budget
    /// pressure evicted it.
    ///
    /// That is the right answer for a lapsed *replica*, whose bytes the cluster still wants
    /// and which this node may itself have use for. It is the wrong one for a stranger's
    /// mail: nobody wants it, this node cannot open it, and leaving it there means a carrier's
    /// cache slowly fills with undecryptable post at the expense of what the household
    /// actually fetched.
    ///
    /// A record too damaged to parse is deleted without its id being knowable, so its bytes
    /// cannot be freed here. They lapse out of their carriage hold on the cache's own
    /// schedule and become evictable like anything else, which is the best available answer.
    pub fn held_and_lapsed(&self, now_ms: u64) -> Result<(Vec<Custody>, Vec<String>), EnvelopeStoreError> {
        let mut out = Vec::new();
        let mut lapsed = Vec::new();
        for entry in std::fs::read_dir(self.root.join("held"))? {
            let path = entry?.path();
            // A damaged record is dropped rather than failing the listing: one unreadable
            // file must not make a node unable to say what else it carries. It is also
            // deleted, because a record that cannot be read cannot be delivered and keeping
            // it would leak the budget it occupies.
            match read_custody(&path) {
                Ok(Some(held)) if !held.is_due(now_ms) => out.push(held),
                Ok(Some(due)) => {
                    lapsed.push(due.envelope.envelope_id);
                    let _ = std::fs::remove_file(&path);
                }
                Ok(None) | Err(_) => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        out.sort_by(|a, b| (a.until_ms, &a.envelope.envelope_id).cmp(&(b.until_ms, &b.envelope.envelope_id)));
        lapsed.sort();
        Ok((out, lapsed))
    }

    /// What this carrier holds for one recipient, and nothing else (ADR-0028 §9).
    ///
    /// The scoped half of the carriage question. A caller that wants the whole bag calls
    /// [`Self::held`]; this exists so the collection path cannot be turned into an
    /// enumeration of everyone the carrier serves.
    pub fn held_for(
        &self,
        recipient: &otwono_identity::NodeId,
        now_ms: u64,
    ) -> Result<Vec<Custody>, EnvelopeStoreError> {
        Ok(self
            .held(now_ms)?
            .into_iter()
            .filter(|c| c.envelope.is_for(recipient))
            .collect())
    }

    /// One custody record by envelope id, if it is still held and not lapsed.
    pub fn get(&self, envelope_id: &str, now_ms: u64) -> Result<Option<Custody>, EnvelopeStoreError> {
        match read_custody(&self.path_for(envelope_id))? {
            Some(held) if !held.is_due(now_ms) => Ok(Some(held)),
            _ => Ok(None),
        }
    }

    /// Give up custody: delivered, expired, or dropped for room.
    ///
    /// Idempotent. A carrier that hands an envelope over and then finds it already gone has
    /// nothing to worry about, and a delete that raced a sweep is not a fault.
    pub fn release(&self, envelope_id: &str) -> Result<(), EnvelopeStoreError> {
        match std::fs::remove_file(self.path_for(envelope_id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Bytes currently committed to other people's mail, for the budget check.
    pub fn bytes_held(&self, now_ms: u64) -> Result<u64, EnvelopeStoreError> {
        Ok(self.held(now_ms)?.iter().map(|c| c.envelope.size_bytes).sum())
    }

    fn path_for(&self, envelope_id: &str) -> PathBuf {
        let digest = blake3::hash(envelope_id.as_bytes());
        self.root.join("held").join(format!("{}.json", digest.to_hex()))
    }
}

fn read_custody(path: &Path) -> Result<Option<Custody>, EnvelopeStoreError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write through a temporary file and rename, durably.
///
/// The same shape as the pointer store's, and for a related reason (ADR-0027 §9): rename is
/// atomic but not durable, and a custody record that vanishes on power loss is an envelope
/// the sender believes is on its way and the recipient will never see. The counter in the
/// temporary name is because `otwono-stored` serves each connection on its own thread.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), EnvelopeStoreError> {
    use std::io::Write;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "tmp.{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let write_and_sync = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    };
    if let Err(e) = write_and_sync() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Some(dir) = path.parent() {
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    Ok(())
}

#[derive(Debug)]
pub enum EnvelopeStoreError {
    Io(String),
    Encoding(String),
}

impl std::fmt::Display for EnvelopeStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvelopeStoreError::Io(m) => write!(f, "envelope store: {m}"),
            EnvelopeStoreError::Encoding(m) => write!(f, "envelope record: {m}"),
        }
    }
}

impl std::error::Error for EnvelopeStoreError {}

impl From<std::io::Error> for EnvelopeStoreError {
    fn from(e: std::io::Error) -> Self {
        EnvelopeStoreError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for EnvelopeStoreError {
    fn from(e: serde_json::Error) -> Self {
        EnvelopeStoreError::Encoding(e.to_string())
    }
}

/// What a node that carries mail can be asked, wherever it keeps it (ADR-0028 §2).
///
/// The same shape as [`ReplicaHolder`](crate::ReplicaHolder), and for the same reason
/// (ADR-0026 §10): the custody store belongs to `otwono-stored` while the pass that fills it
/// runs in `otwono-netd`, so the pass is written against a trait. `EnvelopeStore` implements
/// it in-process for tests; the daemon implements it over the control plane. Neither knows
/// about the other.
///
/// `None` from [`Self::carriage_room`] means **this node carries no mail at all** — no
/// budget, no store, or the capability refused. A pass that gets it makes no carriage traffic
/// whatever, which is ADR-0028 §2's consent kept structural rather than left to a check
/// somebody could forget.
/// The answer to "will you hold this?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Took {
    /// Held, to the deadline the custody names.
    Custody(Custody),
    /// Not held, and why. A refusal is a reply, not an error (ADR-0028 §8).
    Declined(String),
}

pub trait Carrier {
    fn carriage_room(&self, candidates: &[String], now_ms: u64) -> Option<CarriageRoom>;

    /// Take custody, or say why not.
    ///
    /// [`Took::Declined`] is "not this one" -- a full node, a late envelope -- and is the
    /// normal answer on a small machine rather than something to escalate. It carries the
    /// reason because a pass that reports only "took none" is indistinguishable from a pass
    /// that was never offered anything, and that ambiguity has cost a debugging cycle.
    fn take_custody(&self, envelope: &Envelope, now_ms: u64) -> Result<Took, String>;

    /// Keep the sealed bytes this node is about to take custody of.
    ///
    /// Called **before** [`Carrier::take_custody`], and the order is the whole point:
    /// custody of bytes a node does not hold is a promise it cannot keep. A carrier that
    /// recorded custody and dropped the ciphertext would count the envelope against its
    /// budget, offer it onward, and have nothing to serve when the recipient finally came
    /// to collect — which is what the first implementation did, and what three booted nodes
    /// were needed to notice.
    ///
    /// `sharing` is the envelope the *sender* served, carrying the content key sealed to the
    /// recipient. It is stored as it arrived: a carrier cannot open it and does not try.
    fn keep(&self, envelope: &Envelope, bytes: &[u8], sharing: &crate::object::Sharing)
        -> Result<(), String>;
}

/// Where a node puts mail addressed to itself (ADR-0028 §9).
///
/// The receiving counterpart of [`Carrier`], and separate from it because the two are
/// different decisions. A carrier agrees to hold *other people's* sealed bytes and can
/// reasonably decline; a recipient is collecting its own, and the only question is whether it
/// can write them down.
///
/// Injected the way `Carrier` is, so a collection sweep cannot tell an in-process store from
/// one behind the control plane.
pub trait Inbox {
    /// Whether this node will accept mail at all right now.
    ///
    /// Checked **before** anything reaches the wire, the same structural consent
    /// [`Carrier::carriage_room`] gives carriage: a node that cannot write what it collects
    /// asks nobody, rather than fetching an envelope and discovering it has nowhere to put
    /// it. `false` covers a broker that denies the capability and a store that is down —
    /// both mean "not today", and the operator learns which from the log rather than from
    /// the network.
    fn accepting(&self) -> bool;

    /// Whether this node already holds `content_id`.
    ///
    /// The reason a sweep can run on a timer at all. Drop on delivery (ADR-0028 §7) is best
    /// effort: a carrier that refuses the release, answers something else, or never hears it
    /// keeps the envelope to its deadline and keeps offering it. Without this a recipient
    /// would re-download the same mail on every pass until then, and this is the only thing
    /// that stops it.
    fn holds(&self, content_id: &str) -> bool;

    /// Keep a collected envelope where its recipient can open it.
    ///
    /// The ciphertext and the one key that came with it, unchanged. Re-sealing would produce
    /// a different object under a key the sender never issued.
    fn keep(&self, content_id: &str, bytes: &[u8], sharing: &crate::object::Sharing) -> Result<(), String>;
}

/// What a carrier has room for, and what it is already holding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarriageRoom {
    /// Bytes still available under this node's carriage budget. May be zero, which is a node
    /// that carries in principle and has no room today — distinct from `None`, which is a
    /// node that carries nothing at all.
    pub room_bytes: u64,
    /// Which of the offered envelope ids this node already holds, so a pass does not fetch
    /// what it has.
    pub already_held: Vec<String>,
}

// Deliberately no `impl Carrier for EnvelopeStore`.
//
// A carrier must be able to *keep* the ciphertext it takes custody of, and this store holds
// custody records only — the bytes belong to the object store, which lives behind
// `otwono-stored`. An in-process implementation that recorded custody and silently kept
// nothing was what made a carrier look like it was working while holding an envelope it
// could never deliver. The only carrier is the brokered one in `otwono-netd`.

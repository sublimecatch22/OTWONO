//! Where a carrier keeps envelopes it is holding for other people (ADR-0028).
//!
//! # This store holds custody, not content
//!
//! The sealed ciphertext is an ordinary object and lives where objects live — the cluster
//! cache, with its budget, its encryption at rest and its refcounted chunks. What is here is
//! the **custody record**: which envelope, for whom, and until when *this carrier* said it
//! would keep it.
//!
//! That split is ADR-0026 §8's, and it is deliberate reuse rather than a shortcut. A carrier
//! that invented its own byte store would be re-implementing eviction, encryption and
//! refcounting to hold bytes the cache already knows how to hold.
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
        let mut out = Vec::new();
        for entry in std::fs::read_dir(self.root.join("held"))? {
            let path = entry?.path();
            // A damaged record is dropped rather than failing the listing: one unreadable
            // file must not make a node unable to say what else it carries. It is also
            // deleted, because a record that cannot be read cannot be delivered and keeping
            // it would leak the budget it occupies.
            match read_custody(&path) {
                Ok(Some(held)) if !held.is_due(now_ms) => out.push(held),
                Ok(Some(_)) | Ok(None) | Err(_) => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        out.sort_by(|a, b| (a.until_ms, &a.envelope.envelope_id).cmp(&(b.until_ms, &b.envelope.envelope_id)));
        Ok(out)
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

impl Carrier for EnvelopeStore {
    fn carriage_room(&self, candidates: &[String], now_ms: u64) -> Option<CarriageRoom> {
        // The in-process implementation has no budget of its own -- the budget is the
        // daemon's, from the capability profile -- so this reports only what is held. The
        // brokered implementation in otwono-netd is where the number comes from.
        let held = self.held(now_ms).ok()?;
        Some(CarriageRoom {
            room_bytes: u64::MAX,
            already_held: held
                .iter()
                .filter(|c| candidates.is_empty() || candidates.contains(&c.envelope.envelope_id))
                .map(|c| c.envelope.envelope_id.clone())
                .collect(),
        })
    }

    fn take_custody(&self, envelope: &Envelope, now_ms: u64) -> Result<Took, String> {
        let held = self.held(now_ms).map_err(|e| e.to_string())?;
        let committed: u64 = held.iter().map(|c| c.envelope.size_bytes).sum();
        let policy = CarryPolicy::with_room(u64::MAX - committed);
        match self.take(envelope, &policy, now_ms).map_err(|e| e.to_string())? {
            Ok(custody) => Ok(Took::Custody(custody)),
            Err(declined) => Ok(Took::Declined(declined.to_string())),
        }
    }
}

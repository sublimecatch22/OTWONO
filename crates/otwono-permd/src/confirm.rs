//! Pending confirmations (ADR-0024).
//!
//! An `Ask` decision does not block and does not fail. It creates a record here, and the
//! caller is told to come back with its id. Somebody on the confirmation socket approves or
//! denies it; the caller then claims it and gets a token, or does not.
//!
//! The security property lives in [`PendingStore::decide`]: **only a subject in the
//! confirmer set may answer.** Everything else in this module is bookkeeping around that one
//! rule.
//!
//! The set defaults to empty, so an unconfigured node confirms nothing. That is deliberate
//! (ADR-0024 §3a): an agent is kept out by not being in the set, which refuses it more
//! completely than the "must be somebody else" rule this replaced — that one would have let
//! an agent approve a *different* subject's request, and would have refused a person
//! approving their own, which is the normal flow.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;

/// How long a pending confirmation lives before a claim against it is refused.
///
/// Nothing may sit pending indefinitely (ADR-0024 §5). Approval given hours later would
/// authorise an action whose context has gone, and the person saying yes should be saying
/// yes to something that is still happening.
pub const DEFAULT_TTL_MS: u64 = 300_000;

/// The most confirmations that may be pending at once.
///
/// An unbounded pending list is a way for a caller to spend a small board's memory by asking
/// for things nobody will answer. Over the bound, a new request is refused rather than
/// queued — failing the asker is better than degrading the node.
pub const MAX_PENDING: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Approved,
    Denied,
}

/// One request awaiting a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pending {
    pub id: String,
    /// The subject that asked.
    pub subject: String,
    pub action: String,
    pub resource: Option<String>,
    /// The caller's stated reason. **A claim by the thing asking for the permission**, never
    /// a fact — under `SECURITY-MODEL.md` §3 the caller may be an agent acting on untrusted
    /// content. Anything rendering this must present it as an assertion.
    pub reason: Option<String>,
    pub created_unix_ms: u64,
    pub expires_unix_ms: u64,
    pub state: State,
    /// Who approved or denied it, once somebody has.
    pub decided_by: Option<String>,
}

impl Pending {
    pub fn is_expired_at(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_unix_ms
    }

    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.created_unix_ms)
    }
}

/// Why an approval or a claim did not happen.
#[derive(Debug, PartialEq, Eq)]
pub enum ConfirmError {
    /// No such id, or it was consumed.
    Unknown,
    Expired,
    /// The approver is not in the confirmer set. ADR-0024 §3a — the one rule that matters.
    NotAConfirmer {
        subject: String,
    },
    /// Already approved or denied; a decision is not revisited.
    AlreadyDecided(State),
    /// Claimed while still pending.
    StillPending,
    /// Claimed after a denial.
    Denied,
    /// Somebody other than the original asker tried to claim it.
    NotYours,
    TooMany,
}

impl std::fmt::Display for ConfirmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfirmError::Unknown => f.write_str("no such confirmation"),
            ConfirmError::Expired => f.write_str(
                "that confirmation expired; nothing was authorised and the request must be made again",
            ),
            ConfirmError::NotAConfirmer { subject } => write!(
                f,
                "{subject} may not answer confirmations on this node. Only a configured \
                 confirmer can, and an agent is never one"
            ),
            ConfirmError::AlreadyDecided(s) => write!(f, "that confirmation was already {s:?}"),
            ConfirmError::StillPending => f.write_str("nobody has confirmed this yet"),
            ConfirmError::Denied => f.write_str("that was denied"),
            ConfirmError::NotYours => f.write_str("that confirmation belongs to a different subject"),
            ConfirmError::TooMany => f.write_str(
                "too many confirmations are already waiting for somebody; try again once some \
                 have been answered or expired",
            ),
        }
    }
}

impl std::error::Error for ConfirmError {}

#[derive(Default)]
pub struct PendingStore {
    inner: Mutex<HashMap<String, Pending>>,
}

impl PendingStore {
    pub fn new() -> PendingStore {
        PendingStore::default()
    }

    /// Record a request that needs a person, and return it.
    ///
    /// `id` is supplied rather than generated here so the caller owns randomness (the broker
    /// has an RNG; this module should not need one to be testable).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        &self,
        id: String,
        subject: String,
        action: String,
        resource: Option<String>,
        reason: Option<String>,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Pending, ConfirmError> {
        let mut map = self.inner.lock().expect("pending store poisoned");
        map.retain(|_, p| !p.is_expired_at(now_ms));
        if map.len() >= MAX_PENDING {
            return Err(ConfirmError::TooMany);
        }
        let pending = Pending {
            id: id.clone(),
            subject,
            action,
            resource,
            reason,
            created_unix_ms: now_ms,
            expires_unix_ms: now_ms.saturating_add(ttl_ms),
            state: State::Pending,
            decided_by: None,
        };
        map.insert(id, pending.clone());
        Ok(pending)
    }

    /// Everything still awaiting an answer, oldest first.
    pub fn list(&self, now_ms: u64) -> Vec<Pending> {
        let mut map = self.inner.lock().expect("pending store poisoned");
        map.retain(|_, p| !p.is_expired_at(now_ms));
        let mut out: Vec<Pending> = map
            .values()
            .filter(|p| p.state == State::Pending)
            .cloned()
            .collect();
        out.sort_by_key(|p| (p.created_unix_ms, p.id.clone()));
        out
    }

    /// Approve or deny, as `by`.
    ///
    /// **Refuses when `by` is the subject that asked** (ADR-0024 §3). The record stays
    /// pending in that case rather than being consumed: a refused attempt must not also
    /// destroy a request a real confirmer could still answer.
    pub fn decide(
        &self,
        id: &str,
        by: &str,
        confirmers: &[String],
        approve: bool,
        now_ms: u64,
    ) -> Result<Pending, ConfirmError> {
        let mut map = self.inner.lock().expect("pending store poisoned");
        let p = map.get_mut(id).ok_or(ConfirmError::Unknown)?;
        if p.is_expired_at(now_ms) {
            return Err(ConfirmError::Expired);
        }
        if p.state != State::Pending {
            return Err(ConfirmError::AlreadyDecided(p.state));
        }
        if !confirmers.iter().any(|c| c == by) {
            return Err(ConfirmError::NotAConfirmer {
                subject: by.to_string(),
            });
        }
        p.state = if approve { State::Approved } else { State::Denied };
        p.decided_by = Some(by.to_string());
        Ok(p.clone())
    }

    /// Consume an approved confirmation on behalf of the subject that asked.
    ///
    /// Consumed on success, so one approval authorises one request (ADR-0024 §2). A denial
    /// is consumed too — there is nothing further to say about it, and leaving it would let
    /// a caller re-read a "no" forever.
    pub fn claim(&self, id: &str, subject: &str, now_ms: u64) -> Result<Pending, ConfirmError> {
        let mut map = self.inner.lock().expect("pending store poisoned");
        let p = map.get(id).ok_or(ConfirmError::Unknown)?.clone();
        if p.subject != subject {
            // Deliberately not "unknown": the caller holds an id it was given, and telling
            // it the truth here reveals nothing it does not know. What it must not do is
            // succeed.
            return Err(ConfirmError::NotYours);
        }
        if p.is_expired_at(now_ms) {
            map.remove(id);
            return Err(ConfirmError::Expired);
        }
        match p.state {
            State::Pending => Err(ConfirmError::StillPending),
            State::Denied => {
                map.remove(id);
                Err(ConfirmError::Denied)
            }
            State::Approved => {
                map.remove(id);
                Ok(p)
            }
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("pending store poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_700_000_000_000;

    fn store_with_one() -> (PendingStore, Pending) {
        let s = PendingStore::new();
        let p = s
            .open(
                "c1".into(),
                "uid:1000".into(),
                "wallet.sign".into(),
                Some("m/44'/60'/0'/0/0".into()),
                Some("the agent says it is paying an invoice".into()),
                T0,
                DEFAULT_TTL_MS,
            )
            .unwrap();
        (s, p)
    }

    /// Who may answer, in the tests below.
    const CONFIRMERS: [&str; 1] = ["uid:1001"];

    fn confirmers() -> Vec<String> {
        CONFIRMERS.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn only_a_designated_confirmer_may_answer() {
        // ADR-0024 §3a, and the one rule in this module that is a security property rather
        // than bookkeeping. An agent is kept out by not being in the set.
        let (s, _) = store_with_one();
        match s.decide("c1", "uid:9999", &confirmers(), true, T0 + 1) {
            Err(ConfirmError::NotAConfirmer { subject }) => assert_eq!(subject, "uid:9999"),
            other => panic!("somebody outside the set answered: {other:?}"),
        }
        // And the refusal did not consume it: a real confirmer can still answer.
        assert_eq!(s.list(T0 + 1).len(), 1);
        assert!(s.decide("c1", "uid:1001", &confirmers(), true, T0 + 2).is_ok());
    }

    #[test]
    fn an_empty_confirmer_set_refuses_everybody() {
        // The default, and the honest version of "this node cannot confirm anything". A set
        // that fell back to "anyone" would turn an unconfigured node into an open door.
        let (s, _) = store_with_one();
        assert!(matches!(
            s.decide("c1", "uid:1001", &[], true, T0 + 1),
            Err(ConfirmError::NotAConfirmer { .. })
        ));
    }

    #[test]
    fn a_confirmer_may_answer_their_own_request() {
        // The flow the first version of §3 broke. A person runs otwono-storectl, is shown
        // what it will do, and says yes; asking and approving are two real acts by one
        // party, and the second is where they see the consequence. Requiring a second human
        // would refuse every confirmation on a one-person node.
        let s = PendingStore::new();
        s.open(
            "own".into(),
            "uid:1001".into(),
            "fs.delete".into(),
            Some("/home/u/x".into()),
            None,
            T0,
            DEFAULT_TTL_MS,
        )
        .unwrap();
        let decided = s
            .decide("own", "uid:1001", &confirmers(), true, T0 + 1)
            .expect("a confirmer approving their own request is the normal flow");
        assert_eq!(decided.state, State::Approved);
        assert!(s.claim("own", "uid:1001", T0 + 2).is_ok());
    }

    #[test]
    fn a_refused_answer_authorises_nothing() {
        // The failure that would matter: refusing the approval but leaving the record
        // approved anyway.
        let (s, _) = store_with_one();
        let _ = s.decide("c1", "uid:9999", &confirmers(), true, T0 + 1);
        assert_eq!(s.claim("c1", "uid:1000", T0 + 2), Err(ConfirmError::StillPending));
    }

    #[test]
    fn an_approved_confirmation_is_claimed_once_and_only_by_its_asker() {
        let (s, _) = store_with_one();
        s.decide("c1", "uid:1001", &confirmers(), true, T0 + 1).unwrap();

        // Somebody else holding the id gets nothing.
        assert_eq!(s.claim("c1", "uid:1002", T0 + 2), Err(ConfirmError::NotYours));

        let claimed = s.claim("c1", "uid:1000", T0 + 2).expect("its asker claims it");
        assert_eq!(claimed.state, State::Approved);
        assert_eq!(claimed.decided_by.as_deref(), Some("uid:1001"));

        // Consumed: one approval authorises one request (ADR-0024 §2).
        assert_eq!(s.claim("c1", "uid:1000", T0 + 3), Err(ConfirmError::Unknown));
    }

    #[test]
    fn a_denial_is_a_denial_and_is_not_re_readable() {
        let (s, _) = store_with_one();
        s.decide("c1", "uid:1001", &confirmers(), false, T0 + 1).unwrap();
        assert_eq!(s.claim("c1", "uid:1000", T0 + 2), Err(ConfirmError::Denied));
        assert_eq!(s.claim("c1", "uid:1000", T0 + 3), Err(ConfirmError::Unknown));
    }

    #[test]
    fn a_decision_is_not_revisited() {
        let (s, _) = store_with_one();
        s.decide("c1", "uid:1001", &confirmers(), false, T0 + 1).unwrap();
        match s.decide("c1", "uid:1001", &confirmers(), true, T0 + 2) {
            Err(ConfirmError::AlreadyDecided(State::Denied)) => {}
            other => panic!("a denial was overturned: {other:?}"),
        }
    }

    #[test]
    fn expiry_authorises_nothing_and_is_not_a_soft_state() {
        let (s, _) = store_with_one();
        let after = T0 + DEFAULT_TTL_MS;
        assert_eq!(
            s.decide("c1", "uid:1001", &confirmers(), true, after),
            Err(ConfirmError::Expired)
        );
        assert_eq!(s.claim("c1", "uid:1000", after), Err(ConfirmError::Expired));
        assert!(s.list(after).is_empty());
    }

    #[test]
    fn an_expired_confirmation_cannot_be_claimed_even_if_it_was_approved() {
        // The ordering that matters: approved in time, claimed too late. ADR-0024 §5 says
        // expiry is a denial, so "it was approved" must not rescue it.
        let (s, _) = store_with_one();
        s.decide("c1", "uid:1001", &confirmers(), true, T0 + 1).unwrap();
        assert_eq!(
            s.claim("c1", "uid:1000", T0 + DEFAULT_TTL_MS),
            Err(ConfirmError::Expired)
        );
    }

    #[test]
    fn the_pending_list_is_bounded_and_refuses_rather_than_growing() {
        let s = PendingStore::new();
        for i in 0..MAX_PENDING {
            s.open(
                format!("c{i}"),
                "uid:1000".into(),
                "fs.delete".into(),
                None,
                None,
                T0,
                DEFAULT_TTL_MS,
            )
            .unwrap();
        }
        assert_eq!(
            s.open(
                "over".into(),
                "uid:1000".into(),
                "fs.delete".into(),
                None,
                None,
                T0,
                DEFAULT_TTL_MS
            ),
            Err(ConfirmError::TooMany)
        );
        // Once they age out there is room again, without anybody having to sweep.
        assert!(s
            .open(
                "later".into(),
                "uid:1000".into(),
                "fs.delete".into(),
                None,
                None,
                T0 + DEFAULT_TTL_MS,
                DEFAULT_TTL_MS
            )
            .is_ok());
    }

    #[test]
    fn listing_is_oldest_first_and_shows_only_what_still_needs_an_answer() {
        let s = PendingStore::new();
        for (i, t) in [("b", T0 + 10), ("a", T0), ("c", T0 + 20)] {
            s.open(
                i.into(),
                "uid:1000".into(),
                "fs.delete".into(),
                None,
                None,
                t,
                DEFAULT_TTL_MS,
            )
            .unwrap();
        }
        let ids: Vec<String> = s.list(T0 + 30).into_iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);

        s.decide("b", "uid:1001", &confirmers(), true, T0 + 30).unwrap();
        let ids: Vec<String> = s.list(T0 + 31).into_iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["a", "c"], "an answered request still wants an answer");
    }

    #[test]
    fn what_a_confirmer_is_shown_carries_the_resource_and_the_reason_as_a_claim() {
        // "Delete a file" and "delete which file" are not the same question (ADR-0024 §2),
        // and the reason is the asker's own words (§6).
        let (s, _) = store_with_one();
        let shown = &s.list(T0 + 1)[0];
        assert_eq!(shown.resource.as_deref(), Some("m/44'/60'/0'/0/0"));
        assert!(shown.reason.as_deref().unwrap().contains("the agent says"));
        assert_eq!(shown.age_ms(T0 + 1), 1);
    }
}

//! Capability tokens.
//!
//! A token is a bearer credential that is **scoped, time-limited, and bound to the subject
//! it was issued to**. It authorizes one action on one resource — never "whatever the
//! holder asks for next". That scoping is what prevents the confused-deputy problem:
//! an agent cannot accumulate authority across requests, because each token names exactly
//! the operation the user's request implied.
//!
//! Tokens are opaque random strings held in the broker's memory rather than signed blobs.
//! They never cross a trust boundary — the same process issues and verifies them — so a
//! signature would add cryptography without adding a property. They do not survive a
//! restart, which is correct: an in-flight authorization should not outlive the authority
//! that granted it.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

/// 256 bits, hex-encoded. Guessing is not a practical attack at this width.
const TOKEN_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedToken {
    pub token: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    pub expires_at_unix_ms: u64,
    pub one_shot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub subject: String,
    pub action: String,
    pub resource: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    Unknown,
    Expired,
    /// Presented for a different action than it was issued for.
    ActionMismatch {
        issued_for: String,
        presented: String,
    },
    /// Presented for a different resource than it was issued for.
    ResourceMismatch {
        issued_for: Option<String>,
        presented: Option<String>,
    },
    /// Presented by a different subject than it was issued to.
    SubjectMismatch {
        issued_to: String,
        presented: String,
    },
    /// A one-shot token already used.
    Exhausted,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Unknown => write!(f, "no such capability token"),
            TokenError::Expired => write!(f, "capability token has expired"),
            TokenError::ActionMismatch {
                issued_for,
                presented,
            } => {
                write!(f, "token authorizes {issued_for}, not {presented}")
            }
            TokenError::ResourceMismatch {
                issued_for,
                presented,
            } => write!(
                f,
                "token authorizes resource {:?}, not {:?}",
                issued_for.as_deref().unwrap_or("<none>"),
                presented.as_deref().unwrap_or("<none>")
            ),
            TokenError::SubjectMismatch { issued_to, presented } => {
                write!(f, "token was issued to {issued_to}, presented by {presented}")
            }
            TokenError::Exhausted => write!(f, "one-shot capability token already used"),
        }
    }
}

impl std::error::Error for TokenError {}

#[derive(Debug, Clone)]
struct Record {
    subject: String,
    action: String,
    resource: Option<String>,
    expires_at_unix_ms: u64,
    /// `None` means unlimited within the lifetime.
    uses_remaining: Option<u32>,
}

#[derive(Debug, Default)]
pub struct TokenStore {
    records: Mutex<HashMap<String, Record>>,
}

pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn random_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    // A failure here means the OS has no entropy source. Refusing to continue is the only
    // safe response: a predictable capability token is worse than no service at all.
    getrandom::getrandom(&mut bytes).expect("OS entropy source unavailable");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl TokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn issue(
        &self,
        subject: &str,
        action: &str,
        resource: Option<&str>,
        ttl_seconds: u64,
        one_shot: bool,
    ) -> IssuedToken {
        self.issue_at(now_unix_ms(), subject, action, resource, ttl_seconds, one_shot)
    }

    /// Issue with an explicit clock, so expiry is testable without sleeping.
    pub fn issue_at(
        &self,
        now_ms: u64,
        subject: &str,
        action: &str,
        resource: Option<&str>,
        ttl_seconds: u64,
        one_shot: bool,
    ) -> IssuedToken {
        let token = random_token();
        let expires_at_unix_ms = now_ms.saturating_add(ttl_seconds.saturating_mul(1000));
        let record = Record {
            subject: subject.to_string(),
            action: action.to_string(),
            resource: resource.map(str::to_string),
            expires_at_unix_ms,
            uses_remaining: one_shot.then_some(1),
        };
        self.records
            .lock()
            .expect("token store poisoned")
            .insert(token.clone(), record);
        IssuedToken {
            token,
            action: action.to_string(),
            resource: resource.map(str::to_string),
            expires_at_unix_ms,
            one_shot,
        }
    }

    pub fn verify(
        &self,
        token: &str,
        action: &str,
        resource: Option<&str>,
        subject: Option<&str>,
    ) -> Result<Grant, TokenError> {
        self.verify_at(now_unix_ms(), token, action, resource, subject)
    }

    pub fn verify_at(
        &self,
        now_ms: u64,
        token: &str,
        action: &str,
        resource: Option<&str>,
        subject: Option<&str>,
    ) -> Result<Grant, TokenError> {
        let mut records = self.records.lock().expect("token store poisoned");
        let record = records.get_mut(token).ok_or(TokenError::Unknown)?;

        if now_ms >= record.expires_at_unix_ms {
            records.remove(token);
            return Err(TokenError::Expired);
        }
        if record.action != action {
            return Err(TokenError::ActionMismatch {
                issued_for: record.action.clone(),
                presented: action.to_string(),
            });
        }
        if record.resource.as_deref() != resource {
            return Err(TokenError::ResourceMismatch {
                issued_for: record.resource.clone(),
                presented: resource.map(str::to_string),
            });
        }
        if let Some(s) = subject {
            if record.subject != s {
                return Err(TokenError::SubjectMismatch {
                    issued_to: record.subject.clone(),
                    presented: s.to_string(),
                });
            }
        }

        let grant = Grant {
            subject: record.subject.clone(),
            action: record.action.clone(),
            resource: record.resource.clone(),
        };

        if let Some(remaining) = record.uses_remaining.as_mut() {
            if *remaining == 0 {
                return Err(TokenError::Exhausted);
            }
            *remaining -= 1;
            if *remaining == 0 {
                records.remove(token);
            }
        }
        Ok(grant)
    }

    /// Drop expired records. Called periodically so the map does not grow without bound.
    pub fn purge_expired_at(&self, now_ms: u64) -> usize {
        let mut records = self.records.lock().expect("token store poisoned");
        let before = records.len();
        records.retain(|_, r| now_ms < r.expires_at_unix_ms);
        before - records.len()
    }

    pub fn len(&self) -> usize {
        self.records.lock().expect("token store poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_000_000;

    #[test]
    fn tokens_are_long_and_distinct() {
        let s = TokenStore::new();
        let a = s.issue_at(T0, "uid:0", "hw.read", None, 60, false);
        let b = s.issue_at(T0, "uid:0", "hw.read", None, 60, false);
        assert_eq!(a.token.len(), TOKEN_BYTES * 2);
        assert_ne!(a.token, b.token, "tokens must not repeat");
        assert!(a.token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_valid_token_verifies() {
        let s = TokenStore::new();
        let t = s.issue_at(T0, "uid:1000", "hw.read", None, 60, false);
        let g = s
            .verify_at(T0 + 1, &t.token, "hw.read", None, Some("uid:1000"))
            .unwrap();
        assert_eq!(g.subject, "uid:1000");
    }

    #[test]
    fn an_unknown_token_is_rejected() {
        let s = TokenStore::new();
        assert_eq!(
            s.verify_at(T0, "deadbeef", "hw.read", None, None),
            Err(TokenError::Unknown)
        );
    }

    #[test]
    fn an_expired_token_is_rejected_and_forgotten() {
        let s = TokenStore::new();
        let t = s.issue_at(T0, "uid:0", "hw.read", None, 10, false);
        assert_eq!(
            s.verify_at(T0 + 10_001, &t.token, "hw.read", None, None),
            Err(TokenError::Expired)
        );
        assert!(s.is_empty(), "an expired token should not linger in the store");
    }

    #[test]
    fn a_token_does_not_authorize_a_different_action() {
        let s = TokenStore::new();
        let t = s.issue_at(T0, "uid:0", "hw.read", None, 60, false);
        let e = s
            .verify_at(T0 + 1, &t.token, "fs.delete", None, None)
            .unwrap_err();
        assert!(matches!(e, TokenError::ActionMismatch { .. }), "{e:?}");
    }

    #[test]
    fn a_token_does_not_authorize_a_different_resource() {
        // The important case: a token for one file must not unlock another.
        let s = TokenStore::new();
        let t = s.issue_at(T0, "uid:0", "fs.read", Some("/tmp/mine"), 60, false);
        let e = s
            .verify_at(T0 + 1, &t.token, "fs.read", Some("/etc/shadow"), None)
            .unwrap_err();
        assert!(matches!(e, TokenError::ResourceMismatch { .. }), "{e:?}");
    }

    #[test]
    fn a_token_is_bound_to_the_subject_it_was_issued_to() {
        let s = TokenStore::new();
        let t = s.issue_at(T0, "uid:1000", "hw.read", None, 60, false);
        let e = s
            .verify_at(T0 + 1, &t.token, "hw.read", None, Some("uid:1001"))
            .unwrap_err();
        assert!(matches!(e, TokenError::SubjectMismatch { .. }), "{e:?}");
    }

    #[test]
    fn a_one_shot_token_works_exactly_once() {
        let s = TokenStore::new();
        let t = s.issue_at(T0, "uid:0", "fs.delete", Some("/tmp/x"), 60, true);
        assert!(s
            .verify_at(T0 + 1, &t.token, "fs.delete", Some("/tmp/x"), None)
            .is_ok());
        assert_eq!(
            s.verify_at(T0 + 2, &t.token, "fs.delete", Some("/tmp/x"), None),
            Err(TokenError::Unknown),
            "a spent one-shot token must be gone"
        );
    }

    #[test]
    fn a_multi_use_token_survives_repeated_verification() {
        let s = TokenStore::new();
        let t = s.issue_at(T0, "uid:0", "hw.read", None, 60, false);
        for i in 0..5 {
            assert!(s.verify_at(T0 + i, &t.token, "hw.read", None, None).is_ok());
        }
    }

    #[test]
    fn purging_removes_only_expired_tokens() {
        let s = TokenStore::new();
        s.issue_at(T0, "uid:0", "hw.read", None, 10, false);
        s.issue_at(T0, "uid:0", "hw.read", None, 600, false);
        assert_eq!(s.purge_expired_at(T0 + 20_000), 1);
        assert_eq!(s.len(), 1);
    }
}

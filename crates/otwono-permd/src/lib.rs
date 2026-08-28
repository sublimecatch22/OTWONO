//! OTWONO permission broker.
//!
//! `otwono-permd` is the security kernel: the one component that must be correct for the
//! permission model to hold (docs/security/SECURITY-MODEL.md). It is deliberately small.
//!
//! Four pieces:
//!
//! * [`action`] — the typed action registry. Intrinsic properties of an operation, which
//!   policy cannot override.
//! * [`policy`] — declarative rules. Fail-closed: no matching rule means deny.
//! * [`token`] — scoped, time-limited, subject-bound capability tokens.
//! * [`audit`] — an append-only hash-chained log written before any caller learns an outcome.

#![forbid(unsafe_code)]

pub mod action;
pub mod audit;
pub mod confirm;
pub mod policy;
pub mod service;
pub mod token;

pub use action::{ActionRegistry, ActionSpec, BlastRadius};
pub use audit::{AuditEntry, AuditLog, AuditRecord, VerifyReport};
pub use confirm::{ConfirmError, Pending, PendingStore, State as ConfirmState};
pub use policy::{Decision, Evaluation, Policy, PolicyError, Rule};
pub use service::{Broker, ConfirmService};
pub use token::{Grant, IssuedToken, TokenError, TokenStore};

/// Default policy directory.
pub const DEFAULT_POLICY_DIR: &str = "/etc/otwono/policy.d";
/// Default audit log path.
pub const DEFAULT_AUDIT_LOG: &str = "/var/log/otwono/audit.jsonl";
/// Default confirmation socket (ADR-0024).
///
/// Separate from the control-plane socket on purpose: the socket every daemon must reach
/// cannot also be the socket only a person may reach.
pub const DEFAULT_CONFIRM_SOCKET: &str = "/run/otwono/confirm.sock";

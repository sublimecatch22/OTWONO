//! Shared domain types for OTWONO AI.
//!
//! This crate performs no I/O. It owns the vocabulary of the product — the
//! entities, their lifecycle state machines, and the on-disk shapes of things
//! the user can export and re-import — so that the service, the orchestrator
//! and the desktop shell cannot disagree about them.

pub mod agent;
pub mod budget;
pub mod chat;
pub mod error;
pub mod ids;
pub mod knowledge;
pub mod marketplace;
pub mod permission;
pub mod project;
pub mod provider;
pub mod workspace;

pub use error::{DomainError, DomainResult};
pub use ids::{new_id, now, Timestamp};

/// Version of the exported-package schemas understood by this build.
pub const PACKAGE_SCHEMA_VERSION: u32 = 1;

/// Semantic version of the application this crate was built for.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

//! OTWONO AI runtime contracts.
//!
//! This crate is the part of the AI subsystem that has to be right *before* any inference
//! engine is integrated: what a model costs, whether this machine can afford it, and which
//! backend would run it. All of it is pure — no I/O, no hardware, no model files — so it is
//! testable against fixture capability profiles on any machine, including a CI runner with
//! no GPU (docs/ai/AI-RUNTIME.md).
//!
//! # Why this is a separate crate from the daemon
//!
//! `docs/ai/AI-RUNTIME.md` §4 names the failure mode this exists to prevent: "a confident
//! load followed by the OOM killer". That is a decision, not a mechanism, and a decision
//! that can only be exercised by actually running a model is a decision nobody tests. Here
//! it is a function from a manifest and a capability profile to an answer.
//!
//! # What it deliberately does not do
//!
//! No inference, and no dependency on anything that performs inference. This crate names
//! the backend that *should* run a model and reports which are installed on disk
//! ([`discovery`]); running one is `otwono-llama`'s job, under `otwono-aid`. The dependency
//! only ever points that way, so `cargo test -p otwono-ai` needs no engine, no C++
//! toolchain and no model file.

#![forbid(unsafe_code)]

pub mod admission;
pub mod backend;
pub mod catalog;
pub mod discovery;
pub mod install;
pub mod manifest;
pub mod signature;
pub mod supervisor;

pub use admission::{admit, fits_this_machine, Admission, AdmissionError, AdmissionRequest, Reserve};
pub use backend::{select_backend, BackendId, BackendSelection, SelectionError};
pub use catalog::{Catalog, CatalogEntry, CatalogError, CatalogProblem, DEFAULT_MODEL_DIR};
pub use discovery::{discover, installed_backends, installed_backends_in, BackendInstall};
pub use install::{
    check_provenance, hash_file, install, verify_installed, InstallError, InstallRequest, Installed,
    Provenance, Verification,
};
pub use manifest::{Footprint, ManifestError, ModelCapability, ModelFormat, ModelManifest, Signature};
pub use signature::{PublisherTrust, SignatureError, SignatureStatus, TrustError};
pub use supervisor::{BackendError, BackendHello, BackendProcess, PROTOCOL_VERSION};

pub const SCHEMA_VERSION: &str = "1.0.0";

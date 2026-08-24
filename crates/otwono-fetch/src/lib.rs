//! OTWONO egress contracts.
//!
//! This crate decides *what this node is permitted to ask for*. It performs no I/O and
//! opens no sockets, which is the point: the rules that bound outbound requests are the
//! part of the fetcher that most needs to be exhaustively tested, and they are testable
//! here on a machine with no network at all (ADR-0014).
//!
//! # The shape of a request, and why it is not a URL
//!
//! A caller names a **source** — an entry in the operator's allow-list — and a **path
//! suffix**. It never supplies a URL. Consequently it cannot choose the scheme, the host,
//! the port, the query string or the fragment, and the only thing it contributes to the
//! bytes that leave this node is a path under a prefix an operator approved.
//!
//! That residue is a covert channel and this crate does not pretend otherwise. What it
//! does is bound it: [`MAX_PATH_SUFFIX_BYTES`] of a restricted alphabet, logged per fetch.
//!
//! # Redirects are checked with the same rules
//!
//! A `3xx` is a server asking us to make a different request. It is put through
//! [`Source::admits`], which applies the same host, port, scheme and path rules the
//! original request passed. A redirect that leaves the source is a denial, not a hop.

#![forbid(unsafe_code)]

pub mod source;
pub mod spool;

pub use source::{Source, SourceError, SourceSet, DEFAULT_SOURCE_DIR, MAX_PATH_SUFFIX_BYTES, MAX_URL_BYTES};
pub use spool::{SpoolEntry, SpoolError, DEFAULT_SPOOL_DIR};

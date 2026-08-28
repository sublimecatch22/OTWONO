//! Local knowledge: authorisation, ingestion, indexing and retrieval.
//!
//! Two promises this crate keeps:
//!
//! * nothing is read that the user has not explicitly authorised, and a
//!   revocation takes effect before the next retrieval;
//! * nothing is reported as searchable until parsing *and* indexing succeeded.

pub mod chunk;
pub mod embed;
pub mod index;
pub mod injection;
pub mod parse;
pub mod retrieve;

pub use embed::{Embedder, EmbeddingSource};
pub use index::{Indexer, IngestReport};
pub use retrieve::{Retriever, SearchOptions};

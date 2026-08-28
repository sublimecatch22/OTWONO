//! Knowledge sources, documents and chunks.

use serde::{Deserialize, Serialize};

use crate::error::{DomainError, DomainResult};
use crate::ids::Timestamp;

/// File types the MVP parser understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentFormat {
    Text,
    Markdown,
    Pdf,
    Docx,
    Csv,
    SourceCode,
}

impl DocumentFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Markdown => "markdown",
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Csv => "csv",
            Self::SourceCode => "source_code",
        }
    }

    /// Map an extension to a format. Returns `None` for anything the MVP does
    /// not parse, so ingestion can skip it honestly rather than storing noise.
    pub fn from_extension(ext: &str) -> Option<Self> {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        Some(match ext.as_str() {
            "txt" | "text" | "log" => Self::Text,
            "md" | "markdown" | "mdx" => Self::Markdown,
            "pdf" => Self::Pdf,
            "docx" => Self::Docx,
            "csv" | "tsv" => Self::Csv,
            "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "java" | "kt" | "rb" | "php"
            | "c" | "h" | "cpp" | "hpp" | "cs" | "swift" | "sh" | "sql" | "html" | "css"
            | "scss" | "json" | "yaml" | "yml" | "toml" | "xml" => Self::SourceCode,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestState {
    /// Discovered on disk, not yet read.
    Pending,
    Parsing,
    Indexing,
    /// Parsing *and* indexing both succeeded. Only now may the UI say the file
    /// is searchable.
    Indexed,
    Failed,
    /// The file disappeared or the grant was revoked; chunks were removed.
    Removed,
    /// Recognised but deliberately not parsed (unsupported format, too large).
    Skipped,
}

impl IngestState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Parsing => "parsing",
            Self::Indexing => "indexing",
            Self::Indexed => "indexed",
            Self::Failed => "failed",
            Self::Removed => "removed",
            Self::Skipped => "skipped",
        }
    }

    pub const fn is_searchable(self) -> bool {
        matches!(self, Self::Indexed)
    }

    pub fn parse(value: &str) -> DomainResult<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "parsing" => Ok(Self::Parsing),
            "indexing" => Ok(Self::Indexing),
            "indexed" => Ok(Self::Indexed),
            "failed" => Ok(Self::Failed),
            "removed" => Ok(Self::Removed),
            "skipped" => Ok(Self::Skipped),
            other => Err(DomainError::validation(
                "ingest_state",
                format!("unknown {other:?}"),
            )),
        }
    }
}

/// A folder or single file the user has authorised for retrieval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeSource {
    pub id: String,
    pub label: String,
    /// Canonicalised absolute path.
    pub root_path: String,
    pub is_directory: bool,
    /// Set false by revocation; chunks are deleted and documents marked removed.
    pub authorised: bool,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    /// `lexical-fallback` when no embedding model was available. Surfaced in
    /// the UI so retrieval quality is never overstated.
    pub embedding_model: String,
    pub document_count: u32,
    pub chunk_count: u32,
    pub last_indexed_at: Option<Timestamp>,
    pub created_at: Timestamp,
}

/// The name used when no embedding model is reachable.
pub const LEXICAL_FALLBACK_MODEL: &str = "lexical-fallback";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: String,
    pub source_id: String,
    pub path: String,
    pub file_name: String,
    pub format: DocumentFormat,
    pub byte_size: u64,
    /// Content hash; an unchanged hash skips re-parsing on re-index.
    pub content_hash: String,
    pub modified_at: Option<Timestamp>,
    pub state: IngestState,
    pub error: Option<String>,
    pub chunk_count: u32,
    pub indexed_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: String,
    pub document_id: String,
    pub source_id: String,
    pub index: u32,
    pub text: String,
    /// "page 4" or "lines 120-148" — whatever the parser could determine.
    pub locator: Option<String>,
    pub token_estimate: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalHit {
    pub chunk: Chunk,
    pub file_name: String,
    pub file_path: String,
    pub score: f32,
    pub vector_score: f32,
    pub lexical_score: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_mvp_formats_are_recognised() {
        assert_eq!(
            DocumentFormat::from_extension("txt"),
            Some(DocumentFormat::Text)
        );
        assert_eq!(
            DocumentFormat::from_extension(".MD"),
            Some(DocumentFormat::Markdown)
        );
        assert_eq!(
            DocumentFormat::from_extension("pdf"),
            Some(DocumentFormat::Pdf)
        );
        assert_eq!(
            DocumentFormat::from_extension("docx"),
            Some(DocumentFormat::Docx)
        );
        assert_eq!(
            DocumentFormat::from_extension("csv"),
            Some(DocumentFormat::Csv)
        );
        assert_eq!(
            DocumentFormat::from_extension("rs"),
            Some(DocumentFormat::SourceCode)
        );
    }

    #[test]
    fn unknown_formats_are_not_guessed() {
        assert_eq!(DocumentFormat::from_extension("psd"), None);
        assert_eq!(DocumentFormat::from_extension("exe"), None);
        assert_eq!(DocumentFormat::from_extension(""), None);
    }

    #[test]
    fn only_fully_indexed_documents_are_searchable() {
        assert!(IngestState::Indexed.is_searchable());
        for state in [
            IngestState::Pending,
            IngestState::Parsing,
            IngestState::Indexing,
            IngestState::Failed,
            IngestState::Removed,
            IngestState::Skipped,
        ] {
            assert!(!state.is_searchable(), "{state:?} must not be searchable");
            assert_eq!(IngestState::parse(state.as_str()).unwrap(), state);
        }
    }
}

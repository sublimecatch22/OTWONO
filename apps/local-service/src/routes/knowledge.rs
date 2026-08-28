//! Knowledge sources: authorisation, indexing, search and revocation.

use std::path::PathBuf;

use axum::extract::{Path as AxumPath, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use otwono_knowledge::{Indexer, IngestReport, Retriever, SearchOptions};
use otwono_store::repo::activity::{ActivityRepo, NewActivity, Outcome};
use otwono_store::repo::knowledge::{KnowledgeRepo, NewSource};
use otwono_types::knowledge::{Document, KnowledgeSource, RetrievalHit};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct SourcesResponse {
    pub sources: Vec<SourceSummary>,
    /// Set when the index was built without an embedding model.
    pub retrieval_notice: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SourceSummary {
    #[serde(flatten)]
    pub source: KnowledgeSource,
    pub embedding_is_fallback: bool,
    pub embedding_detail: String,
    pub exists_on_disk: bool,
}

fn summarise(source: KnowledgeSource) -> SourceSummary {
    let is_fallback = source.embedding_model == otwono_types::knowledge::LEXICAL_FALLBACK_MODEL;
    let detail = if is_fallback {
        otwono_knowledge::EmbeddingSource::LexicalFallback.describe()
    } else {
        otwono_knowledge::EmbeddingSource::Model {
            connection_id: String::new(),
            model: source.embedding_model.clone(),
        }
        .describe()
    };
    SourceSummary {
        exists_on_disk: PathBuf::from(&source.root_path).exists(),
        embedding_is_fallback: is_fallback,
        embedding_detail: detail,
        source,
    }
}

pub async fn list_sources(State(state): State<AppState>) -> ApiResult<Json<SourcesResponse>> {
    let sources = KnowledgeRepo::new(&state.db).list_sources(false)?;
    let any_fallback = sources.iter().any(|s| {
        s.embedding_model == otwono_types::knowledge::LEXICAL_FALLBACK_MODEL && s.chunk_count > 0
    });
    Ok(Json(SourcesResponse {
        retrieval_notice: any_fallback
            .then(|| otwono_knowledge::EmbeddingSource::LexicalFallback.describe()),
        sources: sources.into_iter().map(summarise).collect(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct AuthoriseRequest {
    pub path: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub include_globs: Vec<String>,
    #[serde(default)]
    pub exclude_globs: Vec<String>,
}

/// Authorise a folder or file. The path must exist; OTWONO does not pretend to
/// have access to something it cannot see.
pub async fn authorise(
    State(state): State<AppState>,
    Json(body): Json<AuthoriseRequest>,
) -> ApiResult<Json<SourceSummary>> {
    let path = PathBuf::from(&body.path);
    let canonical = path.canonicalize().map_err(|_| {
        ApiError::BadRequest(format!(
            "OTWONO could not find {}. Check the path and try again.",
            body.path
        ))
    })?;
    let is_directory = canonical.is_dir();
    let label = body.label.unwrap_or_else(|| {
        canonical
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| canonical.to_string_lossy().to_string())
    });

    let source = KnowledgeRepo::new(&state.db)
        .authorise_source(NewSource {
            label,
            root_path: canonical.to_string_lossy().to_string(),
            is_directory,
            include_globs: body.include_globs,
            exclude_globs: body.exclude_globs,
        })
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    ActivityRepo::new(&state.db).record(
        NewActivity::user("knowledge.authorise")
            .with_target("knowledge_source", &source.id)
            .with_detail(serde_json::json!({ "path": source.root_path })),
    )?;

    Ok(Json(summarise(source)))
}

#[derive(Debug, Deserialize)]
pub struct AuthorisationChange {
    pub authorised: bool,
}

/// Revoke or restore access. Revoking deletes the chunks immediately.
pub async fn set_authorised(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<AuthorisationChange>,
) -> ApiResult<Json<SourceSummary>> {
    let repo = KnowledgeRepo::new(&state.db);
    if repo.get_source(&id)?.is_none() {
        return Err(ApiError::not_found("That knowledge source"));
    }
    repo.set_authorised(&id, body.authorised)?;

    ActivityRepo::new(&state.db).record(
        NewActivity::user(if body.authorised {
            "knowledge.restore"
        } else {
            "knowledge.revoke"
        })
        .with_target("knowledge_source", &id)
        .with_outcome(if body.authorised {
            Outcome::Ok
        } else {
            Outcome::Denied
        }),
    )?;

    repo.get_source(&id)?
        .map(|source| Json(summarise(source)))
        .ok_or_else(|| ApiError::not_found("That knowledge source"))
}

pub async fn delete_source(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let repo = KnowledgeRepo::new(&state.db);
    if repo.get_source(&id)?.is_none() {
        return Err(ApiError::not_found("That knowledge source"));
    }
    repo.delete_source(&id)?;
    ActivityRepo::new(&state.db)
        .record(NewActivity::user("knowledge.delete").with_target("knowledge_source", &id))?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Debug, Serialize)]
pub struct IndexResponse {
    #[serde(flatten)]
    pub report: IngestReport,
    pub message: String,
}

pub async fn index(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Json<IndexResponse>> {
    let embedder = state.embedder().await;
    let report = Indexer::new(&state.db, &embedder)
        .ingest_source(&id)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    ActivityRepo::new(&state.db).record(
        NewActivity::user("knowledge.index")
            .with_target("knowledge_source", &id)
            .with_outcome(if report.failed > 0 {
                Outcome::Failed
            } else {
                Outcome::Ok
            })
            .with_detail(serde_json::to_value(&report).unwrap_or_default()),
    )?;

    let mut message = format!(
        "Indexed {} file(s); {} unchanged, {} skipped, {} failed. {} passage(s) are searchable.",
        report.indexed, report.unchanged, report.skipped, report.failed, report.chunks
    );
    if report.used_fallback_embeddings {
        message.push(' ');
        message.push_str(&otwono_knowledge::EmbeddingSource::LexicalFallback.describe());
    }
    if report.truncated {
        message.push_str(
            " This folder has more files than one run indexes; run indexing again to continue.",
        );
    }

    Ok(Json(IndexResponse { report, message }))
}

pub async fn documents(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Json<Vec<Document>>> {
    Ok(Json(KnowledgeRepo::new(&state.db).list_documents(&id)?))
}

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    /// Sources to search. Empty means "nothing", never "everything".
    #[serde(default)]
    pub source_ids: Vec<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub hits: Vec<RetrievalHit>,
    pub citations: Vec<otwono_types::chat::Citation>,
    pub searched_sources: usize,
    pub used_fallback_embeddings: bool,
}

pub async fn search(
    State(state): State<AppState>,
    Json(body): Json<SearchRequest>,
) -> ApiResult<Json<SearchResponse>> {
    let embedder = state.embedder().await;
    let options = SearchOptions {
        limit: body.limit.unwrap_or(6).clamp(1, 25),
        ..Default::default()
    };
    let hits = Retriever::new(&state.db, &embedder)
        .with_options(options)
        .search(&body.query, &body.source_ids)
        .await
        .map_err(ApiError::Internal)?;

    Ok(Json(SearchResponse {
        citations: Retriever::to_citations(&hits),
        searched_sources: body.source_ids.len(),
        used_fallback_embeddings: embedder.source().is_fallback(),
        hits,
    }))
}

#[derive(Debug, Deserialize)]
pub struct BrowseQuery {
    pub path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BrowseResponse {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<BrowseEntry>,
}

#[derive(Debug, Serialize)]
pub struct BrowseEntry {
    pub name: String,
    pub path: String,
    pub is_directory: bool,
    /// True when OTWONO can parse this file type.
    pub supported: bool,
}

/// A read-only directory listing, so the Knowledge screen can offer a picker
/// inside the web view. Listing a directory is not authorisation to read the
/// files in it: that still requires an explicit grant.
pub async fn browse(Query(query): Query<BrowseQuery>) -> ApiResult<Json<BrowseResponse>> {
    let start = match query.path {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => directories::UserDirs::new()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/")),
    };
    let canonical = start
        .canonicalize()
        .map_err(|_| ApiError::BadRequest(format!("{} could not be opened.", start.display())))?;
    if !canonical.is_dir() {
        return Err(ApiError::BadRequest(format!(
            "{} is a file, not a folder.",
            canonical.display()
        )));
    }

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&canonical)
        .map_err(|e| {
            ApiError::BadRequest(format!("{} could not be read: {e}", canonical.display()))
        })?
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let is_directory = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let supported = is_directory
            || entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .and_then(otwono_types::knowledge::DocumentFormat::from_extension)
                .is_some();
        entries.push(BrowseEntry {
            path: entry.path().to_string_lossy().to_string(),
            name,
            is_directory,
            supported,
        });
    }
    entries.sort_by(|a, b| {
        b.is_directory
            .cmp(&a.is_directory)
            .then(a.name.cmp(&b.name))
    });

    Ok(Json(BrowseResponse {
        parent: canonical.parent().map(|p| p.to_string_lossy().to_string()),
        path: canonical.to_string_lossy().to_string(),
        entries,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus(files: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for (name, body) in files {
            std::fs::write(tmp.path().join(name), body).unwrap();
        }
        tmp
    }

    async fn authorised(state: &AppState, tmp: &tempfile::TempDir) -> String {
        let Json(summary) = authorise(
            State(state.clone()),
            Json(AuthoriseRequest {
                path: tmp.path().to_string_lossy().to_string(),
                label: Some("Docs".into()),
                include_globs: vec![],
                exclude_globs: vec![],
            }),
        )
        .await
        .unwrap();
        summary.source.id
    }

    #[tokio::test]
    async fn a_folder_that_does_not_exist_is_refused_clearly() {
        let state = AppState::for_tests();
        let error = authorise(
            State(state),
            Json(AuthoriseRequest {
                path: "/definitely/not/here".into(),
                label: None,
                include_globs: vec![],
                exclude_globs: vec![],
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::BadRequest(ref m) if m.contains("could not find")));
    }

    #[tokio::test]
    async fn indexing_reports_what_happened_and_names_the_fallback() {
        let state = AppState::for_tests();
        let tmp = corpus(&[
            ("policy.md", "Staff receive 25 days of annual leave."),
            ("image.png", "not parseable"),
        ]);
        let source_id = authorised(&state, &tmp).await;

        let Json(response) = index(State(state), AxumPath(source_id)).await.unwrap();
        assert_eq!(response.report.indexed, 1);
        assert_eq!(response.report.skipped, 1);
        assert!(response.report.used_fallback_embeddings);
        assert!(response.message.contains("1 file(s)"));
        assert!(response.message.contains("without an embedding model"));
    }

    #[tokio::test]
    async fn search_returns_citations_that_name_the_file_and_location() {
        let state = AppState::for_tests();
        let tmp = corpus(&[(
            "policy.md",
            "Staff receive 25 days of annual leave each year.",
        )]);
        let source_id = authorised(&state, &tmp).await;
        let _ = index(State(state.clone()), AxumPath(source_id.clone()))
            .await
            .unwrap();

        let Json(response) = search(
            State(state),
            Json(SearchRequest {
                query: "how much annual leave".into(),
                source_ids: vec![source_id],
                limit: None,
            }),
        )
        .await
        .unwrap();

        assert!(!response.hits.is_empty());
        assert_eq!(response.citations[0].file_name, "policy.md");
        assert!(response.citations[0].locator.is_some());
        assert!(response.used_fallback_embeddings);
    }

    #[tokio::test]
    async fn searching_with_no_sources_selected_returns_nothing() {
        let state = AppState::for_tests();
        let tmp = corpus(&[("policy.md", "Annual leave is 25 days.")]);
        let source_id = authorised(&state, &tmp).await;
        let _ = index(State(state.clone()), AxumPath(source_id))
            .await
            .unwrap();

        let Json(response) = search(
            State(state),
            Json(SearchRequest {
                query: "annual leave".into(),
                source_ids: vec![],
                limit: None,
            }),
        )
        .await
        .unwrap();
        assert!(response.hits.is_empty());
    }

    #[tokio::test]
    async fn revoking_a_source_makes_it_unsearchable_at_once() {
        let state = AppState::for_tests();
        let tmp = corpus(&[("policy.md", "Annual leave is 25 days.")]);
        let source_id = authorised(&state, &tmp).await;
        let _ = index(State(state.clone()), AxumPath(source_id.clone()))
            .await
            .unwrap();

        let _ = set_authorised(
            State(state.clone()),
            AxumPath(source_id.clone()),
            Json(AuthorisationChange { authorised: false }),
        )
        .await
        .unwrap();

        let Json(response) = search(
            State(state),
            Json(SearchRequest {
                query: "annual leave".into(),
                source_ids: vec![source_id],
                limit: None,
            }),
        )
        .await
        .unwrap();
        assert!(
            response.hits.is_empty(),
            "a revoked source must not be searchable"
        );
    }

    #[tokio::test]
    async fn document_states_are_visible_including_failures() {
        let state = AppState::for_tests();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("good.md"), "Readable text.").unwrap();
        std::fs::write(tmp.path().join("bad.md"), [0u8, 1, 2, 0]).unwrap();
        let source_id = authorised(&state, &tmp).await;
        let _ = index(State(state.clone()), AxumPath(source_id.clone()))
            .await
            .unwrap();

        let Json(documents) = documents(State(state), AxumPath(source_id)).await.unwrap();
        assert_eq!(documents.len(), 2);
        let failed = documents.iter().find(|d| d.file_name == "bad.md").unwrap();
        assert_eq!(failed.state, otwono_types::knowledge::IngestState::Failed);
        assert!(failed.error.is_some());
    }

    #[tokio::test]
    async fn browsing_lists_folders_first_and_marks_what_can_be_read() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("reports")).unwrap();
        std::fs::write(tmp.path().join("notes.md"), "x").unwrap();
        std::fs::write(tmp.path().join("photo.png"), "x").unwrap();
        std::fs::write(tmp.path().join(".hidden"), "x").unwrap();

        let Json(response) = browse(Query(BrowseQuery {
            path: Some(tmp.path().to_string_lossy().to_string()),
        }))
        .await
        .unwrap();

        assert_eq!(response.entries.len(), 3, "hidden entries are skipped");
        assert!(response.entries[0].is_directory);
        let notes = response
            .entries
            .iter()
            .find(|e| e.name == "notes.md")
            .unwrap();
        assert!(notes.supported);
        let photo = response
            .entries
            .iter()
            .find(|e| e.name == "photo.png")
            .unwrap();
        assert!(!photo.supported);
        assert!(response.parent.is_some());
    }
}

//! The ingestion pipeline.
//!
//! Walk an authorised source, decide what changed, parse, chunk, embed and
//! store — marking each document `indexed` only when all of that succeeded.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};

use otwono_store::repo::knowledge::{ChunkWithVector, KnowledgeRepo};
use otwono_store::Db;
use otwono_types::knowledge::{DocumentFormat, IngestState};

use crate::chunk::{chunk_document, ChunkOptions};
use crate::embed::Embedder;
use crate::parse;

/// Directories never walked, whatever the include rules say. These hold build
/// output and version-control internals, not user knowledge.
pub const ALWAYS_EXCLUDED: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".cache",
    ".gradle",
    "vendor",
    ".idea",
    ".vscode",
];

/// Highest number of files taken from one source in a single run, so that
/// pointing OTWONO at a home directory does not lock the application up.
pub const MAX_FILES_PER_RUN: usize = 5_000;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IngestReport {
    pub scanned: usize,
    pub indexed: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub failed: usize,
    pub removed: usize,
    pub chunks: usize,
    /// One line per failure, shown in the Knowledge screen.
    pub failures: Vec<String>,
    /// Set when the run stopped at `MAX_FILES_PER_RUN`.
    pub truncated: bool,
    pub embedding_model: String,
    pub used_fallback_embeddings: bool,
}

/// The files a walk found, the number skipped, and whether it was truncated.
pub type Discovery = (Vec<(PathBuf, DocumentFormat)>, usize, bool);

pub struct Indexer<'a> {
    db: &'a Db,
    embedder: &'a Embedder,
    options: ChunkOptions,
}

fn build_globs(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern)?);
    }
    Ok(Some(builder.build()?))
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

impl<'a> Indexer<'a> {
    pub fn new(db: &'a Db, embedder: &'a Embedder) -> Self {
        Self {
            db,
            embedder,
            options: ChunkOptions::default(),
        }
    }

    pub fn with_chunk_options(mut self, options: ChunkOptions) -> Self {
        self.options = options;
        self
    }

    /// Files inside a source that this build can parse, the number skipped,
    /// and whether the walk stopped at `MAX_FILES_PER_RUN`.
    pub fn discover(
        root: &Path,
        is_directory: bool,
        include: &[String],
        exclude: &[String],
    ) -> Result<Discovery> {
        let include = build_globs(include)?;
        let exclude = build_globs(exclude)?;
        let mut found = Vec::new();
        let mut skipped = 0usize;
        let mut truncated = false;

        if !is_directory {
            let format = root
                .extension()
                .and_then(|e| e.to_str())
                .and_then(DocumentFormat::from_extension);
            match format {
                Some(format) => found.push((root.to_path_buf(), format)),
                None => skipped += 1,
            }
            return Ok((found, skipped, false));
        }

        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                // Depth 0 is the source root itself. The user chose it
                // explicitly, so it is walked even if it is named `.notes`.
                if entry.depth() == 0 {
                    return true;
                }
                let name = entry.file_name().to_string_lossy();
                !(entry.file_type().is_dir()
                    && (ALWAYS_EXCLUDED.contains(&name.as_ref()) || name.starts_with('.')))
            })
        {
            let Ok(entry) = entry else {
                skipped += 1;
                continue;
            };
            if !entry.file_type().is_file() {
                continue;
            }
            if found.len() >= MAX_FILES_PER_RUN {
                truncated = true;
                break;
            }

            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap_or(path);
            if exclude.as_ref().is_some_and(|set| set.is_match(relative)) {
                skipped += 1;
                continue;
            }
            if include.as_ref().is_some_and(|set| !set.is_match(relative)) {
                skipped += 1;
                continue;
            }
            match path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(DocumentFormat::from_extension)
            {
                Some(format) => found.push((path.to_path_buf(), format)),
                None => skipped += 1,
            }
        }

        found.sort();
        Ok((found, skipped, truncated))
    }

    /// Index (or re-index) one source.
    pub async fn ingest_source(&self, source_id: &str) -> Result<IngestReport> {
        let repo = KnowledgeRepo::new(self.db);
        let source = repo
            .get_source(source_id)?
            .ok_or_else(|| anyhow::anyhow!("knowledge source {source_id} does not exist"))?;
        if !source.authorised {
            bail!(
                "{} is not authorised. Grant access to it in Knowledge before indexing.",
                source.root_path
            );
        }

        let root = PathBuf::from(&source.root_path);
        if !root.exists() {
            bail!(
                "{} no longer exists on disk. Remove the source, or restore the folder and try \
                 again.",
                source.root_path
            );
        }

        let (files, skipped, truncated) = Self::discover(
            &root,
            source.is_directory,
            &source.include_globs,
            &source.exclude_globs,
        )?;

        let mut report = IngestReport {
            scanned: files.len(),
            skipped,
            truncated,
            embedding_model: self.embedder.source().model_name(),
            used_fallback_embeddings: self.embedder.source().is_fallback(),
            ..Default::default()
        };

        let mut seen: Vec<String> = Vec::with_capacity(files.len());
        for (path, format) in files {
            let path_text = path.to_string_lossy().to_string();
            seen.push(path_text.clone());

            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    report.failed += 1;
                    report.failures.push(format!("{path_text}: {error}"));
                    continue;
                }
            };
            let hash = match hash_file(&path) {
                Ok(hash) => hash,
                Err(error) => {
                    report.failed += 1;
                    report.failures.push(format!("{path_text}: {error}"));
                    continue;
                }
            };
            let modified = metadata
                .modified()
                .ok()
                .map(chrono::DateTime::<chrono::Utc>::from)
                .map(|time| otwono_types::ids::format_ts(&time));

            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path_text.clone());

            let (document, changed) = repo.upsert_document(
                source_id,
                &path_text,
                &file_name,
                format,
                metadata.len(),
                &hash,
                modified.as_deref(),
            )?;

            if !changed {
                report.unchanged += 1;
                continue;
            }

            repo.set_document_state(&document.id, IngestState::Parsing, None)?;
            let segments = match parse::parse(&path, format) {
                Ok(segments) => segments,
                Err(error) => {
                    let message = error.to_string();
                    // A file we cannot read is skipped, not failed, when the
                    // reason is "we do not handle this" rather than "it broke".
                    let state = if message.contains("larger than") {
                        report.skipped += 1;
                        IngestState::Skipped
                    } else {
                        report.failed += 1;
                        report.failures.push(format!("{file_name}: {message}"));
                        IngestState::Failed
                    };
                    repo.set_document_state(&document.id, state, Some(&message))?;
                    continue;
                }
            };

            let chunks = chunk_document(&segments, self.options);
            if chunks.is_empty() {
                repo.set_document_state(
                    &document.id,
                    IngestState::Skipped,
                    Some("the file contained no indexable text"),
                )?;
                report.skipped += 1;
                continue;
            }

            repo.set_document_state(&document.id, IngestState::Indexing, None)?;
            let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
            let vectors = match self.embedder.embed(&texts).await {
                Ok(vectors) => vectors,
                Err(error) => {
                    let message = format!("embedding failed: {error}");
                    repo.set_document_state(&document.id, IngestState::Failed, Some(&message))?;
                    report.failed += 1;
                    report.failures.push(format!("{file_name}: {message}"));
                    continue;
                }
            };

            let with_vectors: Vec<ChunkWithVector> = chunks
                .iter()
                .zip(vectors)
                .map(|(chunk, vector)| ChunkWithVector {
                    index: chunk.index,
                    text: chunk.text.clone(),
                    locator: chunk.locator.clone(),
                    token_estimate: crate::chunk::estimate_tokens(&chunk.text),
                    vector,
                })
                .collect();

            let stored = repo.replace_chunks(
                &document.id,
                source_id,
                &self.embedder.source().model_name(),
                &with_vectors,
            )?;
            report.indexed += 1;
            report.chunks += stored as usize;
        }

        // Deletion propagation: anything in the index that is no longer on disk
        // goes, along with its chunks and vectors.
        for document in repo.list_documents(source_id)? {
            if !seen.contains(&document.path) {
                repo.remove_document(&document.id)?;
                report.removed += 1;
            }
        }

        repo.set_embedding_model(source_id, &self.embedder.source().model_name())?;
        repo.mark_indexed_now(source_id)?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otwono_store::repo::knowledge::NewSource;

    fn source(db: &Db, root: &Path) -> String {
        KnowledgeRepo::new(db)
            .authorise_source(NewSource {
                label: "Docs".into(),
                root_path: root.to_string_lossy().to_string(),
                is_directory: true,
                include_globs: vec![],
                exclude_globs: vec![],
            })
            .unwrap()
            .id
    }

    fn write(dir: &Path, name: &str, contents: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn a_folder_is_indexed_and_reported_accurately() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "policy.md",
            "# Leave\n\nStaff receive 25 days of annual leave.",
        );
        write(
            tmp.path(),
            "notes.txt",
            "Remember to file the quarterly return.",
        );
        write(
            tmp.path(),
            "photo.png",
            "not really a png but not parseable either",
        );

        let db = Db::open_in_memory().unwrap();
        let source_id = source(&db, tmp.path());
        let embedder = Embedder::lexical();
        let report = Indexer::new(&db, &embedder)
            .ingest_source(&source_id)
            .await
            .unwrap();

        assert_eq!(report.indexed, 2);
        assert_eq!(
            report.skipped, 1,
            "the unparseable extension is skipped, not failed"
        );
        assert_eq!(report.failed, 0);
        assert!(report.chunks >= 2);
        assert!(report.used_fallback_embeddings);
        assert_eq!(report.embedding_model, "lexical-fallback");
    }

    #[tokio::test]
    async fn re_indexing_skips_unchanged_files_and_re_reads_changed_ones() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.md", "First version of the text.");
        let db = Db::open_in_memory().unwrap();
        let source_id = source(&db, tmp.path());
        let embedder = Embedder::lexical();
        let indexer = Indexer::new(&db, &embedder);

        let first = indexer.ingest_source(&source_id).await.unwrap();
        assert_eq!(first.indexed, 1);

        let second = indexer.ingest_source(&source_id).await.unwrap();
        assert_eq!(second.indexed, 0);
        assert_eq!(second.unchanged, 1);

        write(tmp.path(), "a.md", "Second version, quite different.");
        let third = indexer.ingest_source(&source_id).await.unwrap();
        assert_eq!(third.indexed, 1);
        assert_eq!(third.unchanged, 0);
    }

    #[tokio::test]
    async fn deleting_a_file_removes_it_from_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.md", "Keep this.");
        write(tmp.path(), "b.md", "Delete this later.");
        let db = Db::open_in_memory().unwrap();
        let source_id = source(&db, tmp.path());
        let embedder = Embedder::lexical();
        let indexer = Indexer::new(&db, &embedder);
        indexer.ingest_source(&source_id).await.unwrap();

        let repo = KnowledgeRepo::new(&db);
        assert_eq!(repo.list_documents(&source_id).unwrap().len(), 2);

        std::fs::remove_file(tmp.path().join("b.md")).unwrap();
        let report = indexer.ingest_source(&source_id).await.unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(repo.list_documents(&source_id).unwrap().len(), 1);

        let remaining = repo.searchable_chunks(&[source_id]).unwrap();
        assert!(
            remaining.iter().all(|(_, _, name, _)| name == "a.md"),
            "chunks of the deleted file must be gone"
        );
    }

    #[tokio::test]
    async fn a_file_that_cannot_be_parsed_is_recorded_as_failed_with_a_reason() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("broken.md"), [0x00, 0x01, 0x02, 0x00]).unwrap();
        let db = Db::open_in_memory().unwrap();
        let source_id = source(&db, tmp.path());
        let embedder = Embedder::lexical();
        let report = Indexer::new(&db, &embedder)
            .ingest_source(&source_id)
            .await
            .unwrap();

        assert_eq!(report.failed, 1);
        assert_eq!(report.indexed, 0);
        assert!(report.failures[0].contains("broken.md"));

        let documents = KnowledgeRepo::new(&db).list_documents(&source_id).unwrap();
        assert_eq!(documents[0].state, IngestState::Failed);
        assert!(documents[0].error.is_some());
        assert!(!documents[0].state.is_searchable());
    }

    #[tokio::test]
    async fn an_unauthorised_source_is_refused_before_anything_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.md", "Secret.");
        let db = Db::open_in_memory().unwrap();
        let source_id = source(&db, tmp.path());
        let repo = KnowledgeRepo::new(&db);
        repo.set_authorised(&source_id, false).unwrap();

        let embedder = Embedder::lexical();
        let error = Indexer::new(&db, &embedder)
            .ingest_source(&source_id)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("not authorised"), "{error}");
        assert!(repo.searchable_chunks(&[source_id]).unwrap().is_empty());
    }

    #[test]
    fn a_source_root_is_walked_even_when_its_own_name_starts_with_a_dot() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join(".private-notes");
        std::fs::create_dir_all(&root).unwrap();
        write(&root, "a.md", "content");

        let (files, _, _) = Indexer::discover(&root, true, &[], &[]).unwrap();
        assert_eq!(
            files.len(),
            1,
            "the user chose this folder explicitly: {files:?}"
        );
    }

    #[test]
    fn build_directories_are_never_walked() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "keep.md", "keep");
        write(tmp.path(), "node_modules/left-pad/index.md", "noise");
        write(tmp.path(), "target/debug/build.md", "noise");
        write(tmp.path(), ".git/COMMIT_EDITMSG", "noise");

        let (files, _, _) = Indexer::discover(tmp.path(), true, &[], &[]).unwrap();
        assert_eq!(files.len(), 1, "found {files:?}");
        assert!(files[0].0.ends_with("keep.md"));
    }

    #[test]
    fn include_and_exclude_globs_are_honoured() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.md", "x");
        write(tmp.path(), "b.txt", "x");
        write(tmp.path(), "drafts/c.md", "x");

        let (only_md, _, _) =
            Indexer::discover(tmp.path(), true, &["**/*.md".into()], &[]).unwrap();
        assert_eq!(only_md.len(), 2);

        let (no_drafts, _, _) =
            Indexer::discover(tmp.path(), true, &[], &["drafts/**".into()]).unwrap();
        assert_eq!(no_drafts.len(), 2);
        assert!(no_drafts
            .iter()
            .all(|(p, _)| !p.to_string_lossy().contains("drafts")));
    }

    #[test]
    fn a_single_file_source_indexes_only_that_file() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.md", "x");
        write(tmp.path(), "b.md", "x");
        let (files, _, _) = Indexer::discover(&tmp.path().join("a.md"), false, &[], &[]).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].0.ends_with("a.md"));
    }

    #[tokio::test]
    async fn a_source_whose_folder_disappeared_says_so_plainly() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_path_buf();
        let db = Db::open_in_memory().unwrap();
        let source_id = source(&db, &path);
        drop(tmp);

        let embedder = Embedder::lexical();
        let error = Indexer::new(&db, &embedder)
            .ingest_source(&source_id)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no longer exists"), "{error}");
    }
}

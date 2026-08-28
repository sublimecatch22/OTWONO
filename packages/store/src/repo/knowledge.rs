//! Knowledge sources, documents, chunks and vectors.
//!
//! Two invariants this module upholds:
//! * a document is only `indexed` once parsing *and* vector storage succeeded;
//! * revoking a source deletes its chunks and vectors immediately, so a revoked
//!   folder cannot be retrieved from even once.

use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension, Row};

use otwono_types::knowledge::{
    Chunk, Document, DocumentFormat, IngestState, KnowledgeSource, LEXICAL_FALLBACK_MODEL,
};

use crate::Db;

const SOURCE_COLUMNS: &str = "id, label, root_path, is_directory, authorised, include_globs, \
    exclude_globs, embedding_model, last_indexed_at, created_at";

fn parse_format(value: &str) -> DocumentFormat {
    match value {
        "markdown" => DocumentFormat::Markdown,
        "pdf" => DocumentFormat::Pdf,
        "docx" => DocumentFormat::Docx,
        "csv" => DocumentFormat::Csv,
        "source_code" => DocumentFormat::SourceCode,
        _ => DocumentFormat::Text,
    }
}

fn map_document(row: &Row<'_>) -> rusqlite::Result<Document> {
    Ok(Document {
        id: row.get(0)?,
        source_id: row.get(1)?,
        path: row.get(2)?,
        file_name: row.get(3)?,
        format: parse_format(&row.get::<_, String>(4)?),
        byte_size: row.get::<_, i64>(5)? as u64,
        content_hash: row.get(6)?,
        modified_at: crate::parse_ts_opt(row.get(7)?),
        state: IngestState::parse(&row.get::<_, String>(8)?).unwrap_or(IngestState::Pending),
        error: row.get(9)?,
        chunk_count: row.get::<_, i64>(10)? as u32,
        indexed_at: crate::parse_ts_opt(row.get(11)?),
    })
}

const DOC_COLUMNS: &str = "id, source_id, path, file_name, format, byte_size, content_hash, \
    modified_at, state, error, chunk_count, indexed_at";

fn map_chunk(row: &Row<'_>) -> rusqlite::Result<Chunk> {
    Ok(Chunk {
        id: row.get(0)?,
        document_id: row.get(1)?,
        source_id: row.get(2)?,
        index: row.get::<_, i64>(3)? as u32,
        text: row.get(4)?,
        locator: row.get(5)?,
        token_estimate: row.get::<_, i64>(6)? as u32,
    })
}

const CHUNK_COLUMNS: &str =
    "id, document_id, source_id, chunk_index, text, locator, token_estimate";

#[derive(Debug, Clone)]
pub struct NewSource {
    pub label: String,
    pub root_path: String,
    pub is_directory: bool,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
}

/// A chunk together with its embedding, written atomically.
/// A chunk ready for scoring: the chunk, its embedding, and the file it came
/// from (name and path) so a hit can be cited without a second query.
pub type SearchableChunk = (Chunk, Vec<f32>, String, String);

#[derive(Debug, Clone)]
pub struct ChunkWithVector {
    pub index: u32,
    pub text: String,
    pub locator: Option<String>,
    pub token_estimate: u32,
    pub vector: Vec<f32>,
}

/// Serialise a vector as little-endian f32s.
pub fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

pub fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub struct KnowledgeRepo<'a> {
    db: &'a Db,
}

fn map_source(row: &Row<'_>) -> rusqlite::Result<KnowledgeSource> {
    Ok(KnowledgeSource {
        id: row.get(0)?,
        label: row.get(1)?,
        root_path: row.get(2)?,
        is_directory: row.get::<_, i64>(3)? != 0,
        authorised: row.get::<_, i64>(4)? != 0,
        include_globs: crate::json_column(row.get(5)?),
        exclude_globs: crate::json_column(row.get(6)?),
        embedding_model: row.get(7)?,
        document_count: 0,
        chunk_count: 0,
        last_indexed_at: crate::parse_ts_opt(row.get(8)?),
        created_at: crate::parse_ts(&row.get::<_, String>(9)?),
    })
}

impl<'a> KnowledgeRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    // ---- sources

    pub fn authorise_source(&self, new: NewSource) -> Result<KnowledgeSource> {
        if new.root_path.trim().is_empty() {
            bail!("a knowledge source needs a path");
        }
        if let Some(existing) = self.find_source_by_path(&new.root_path)? {
            // Re-authorising a previously revoked folder is allowed and is the
            // documented way to restore access.
            if !existing.authorised {
                self.set_authorised(&existing.id, true)?;
                return self
                    .get_source(&existing.id)?
                    .ok_or_else(|| anyhow::anyhow!("source vanished"));
            }
            return Ok(existing);
        }
        let id = otwono_types::new_id("src");
        self.db.conn()?.execute(
            "INSERT INTO knowledge_sources
               (id, label, root_path, is_directory, authorised, include_globs, exclude_globs,
                embedding_model, created_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8)",
            params![
                id,
                new.label,
                new.root_path,
                new.is_directory as i64,
                crate::to_json(&new.include_globs),
                crate::to_json(&new.exclude_globs),
                LEXICAL_FALLBACK_MODEL,
                crate::now_str()
            ],
        )?;
        self.get_source(&id)?
            .ok_or_else(|| anyhow::anyhow!("source not found after creation"))
    }

    pub fn get_source(&self, id: &str) -> Result<Option<KnowledgeSource>> {
        let conn = self.db.conn()?;
        let source = conn
            .query_row(
                &format!("SELECT {SOURCE_COLUMNS} FROM knowledge_sources WHERE id = ?1"),
                [id],
                map_source,
            )
            .optional()?;
        drop(conn);
        match source {
            Some(mut source) => {
                let (documents, chunks) = self.counts(&source.id)?;
                source.document_count = documents;
                source.chunk_count = chunks;
                Ok(Some(source))
            }
            None => Ok(None),
        }
    }

    pub fn find_source_by_path(&self, path: &str) -> Result<Option<KnowledgeSource>> {
        let conn = self.db.conn()?;
        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM knowledge_sources WHERE root_path = ?1",
                [path],
                |r| r.get(0),
            )
            .optional()?;
        drop(conn);
        match id {
            Some(id) => self.get_source(&id),
            None => Ok(None),
        }
    }

    pub fn list_sources(&self, only_authorised: bool) -> Result<Vec<KnowledgeSource>> {
        let conn = self.db.conn()?;
        let sql = if only_authorised {
            format!("SELECT {SOURCE_COLUMNS} FROM knowledge_sources WHERE authorised = 1 ORDER BY label")
        } else {
            format!("SELECT {SOURCE_COLUMNS} FROM knowledge_sources ORDER BY label")
        };
        let mut stmt = conn.prepare(&sql)?;
        let mut sources = stmt
            .query_map([], map_source)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        drop(conn);
        for source in &mut sources {
            let (documents, chunks) = self.counts(&source.id)?;
            source.document_count = documents;
            source.chunk_count = chunks;
        }
        Ok(sources)
    }

    fn counts(&self, source_id: &str) -> Result<(u32, u32)> {
        let conn = self.db.conn()?;
        let documents: i64 = conn.query_row(
            "SELECT COUNT(*) FROM documents WHERE source_id = ?1 AND state <> 'removed'",
            [source_id],
            |r| r.get(0),
        )?;
        let chunks: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE source_id = ?1",
            [source_id],
            |r| r.get(0),
        )?;
        Ok((documents as u32, chunks as u32))
    }

    /// Revoke or restore a source. Revoking deletes every chunk and vector
    /// belonging to it in the same transaction, and marks its documents
    /// `removed`, so nothing survives that could still be retrieved.
    pub fn set_authorised(&self, source_id: &str, authorised: bool) -> Result<()> {
        self.db.transaction(|tx| {
            tx.execute(
                "UPDATE knowledge_sources SET authorised = ?2 WHERE id = ?1",
                params![source_id, authorised as i64],
            )?;
            if !authorised {
                tx.execute(
                    "DELETE FROM chunk_vectors WHERE source_id = ?1",
                    [source_id],
                )?;
                tx.execute("DELETE FROM chunks WHERE source_id = ?1", [source_id])?;
                tx.execute(
                    "UPDATE documents SET state = 'removed', chunk_count = 0, indexed_at = NULL
                      WHERE source_id = ?1",
                    [source_id],
                )?;
            }
            Ok(())
        })
    }

    pub fn set_embedding_model(&self, source_id: &str, model: &str) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE knowledge_sources SET embedding_model = ?2 WHERE id = ?1",
            params![source_id, model],
        )?;
        Ok(())
    }

    pub fn mark_indexed_now(&self, source_id: &str) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE knowledge_sources SET last_indexed_at = ?2 WHERE id = ?1",
            params![source_id, crate::now_str()],
        )?;
        Ok(())
    }

    /// Delete a source outright, along with everything derived from it.
    pub fn delete_source(&self, source_id: &str) -> Result<()> {
        self.db
            .conn()?
            .execute("DELETE FROM knowledge_sources WHERE id = ?1", [source_id])?;
        Ok(())
    }

    // ---- documents

    /// Record a discovered file. Returns the document and whether its content
    /// changed since the last index (the signal to re-parse).
    pub fn upsert_document(
        &self,
        source_id: &str,
        path: &str,
        file_name: &str,
        format: DocumentFormat,
        byte_size: u64,
        content_hash: &str,
        modified_at: Option<&str>,
    ) -> Result<(Document, bool)> {
        let existing = {
            let conn = self.db.conn()?;
            conn.query_row(
                &format!("SELECT {DOC_COLUMNS} FROM documents WHERE source_id = ?1 AND path = ?2"),
                params![source_id, path],
                map_document,
            )
            .optional()?
        };

        if let Some(document) = existing {
            let changed = document.content_hash != content_hash || !document.state.is_searchable();
            if changed {
                self.db.conn()?.execute(
                    "UPDATE documents SET file_name = ?3, format = ?4, byte_size = ?5,
                            content_hash = ?6, modified_at = ?7, state = 'pending', error = NULL
                      WHERE source_id = ?1 AND path = ?2",
                    params![
                        source_id,
                        path,
                        file_name,
                        format.as_str(),
                        byte_size as i64,
                        content_hash,
                        modified_at
                    ],
                )?;
            }
            let reloaded = self
                .get_document(&document.id)?
                .ok_or_else(|| anyhow::anyhow!("document vanished"))?;
            return Ok((reloaded, changed));
        }

        let id = otwono_types::new_id("doc");
        self.db.conn()?.execute(
            "INSERT INTO documents
               (id, source_id, path, file_name, format, byte_size, content_hash, modified_at,
                state, chunk_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', 0)",
            params![
                id,
                source_id,
                path,
                file_name,
                format.as_str(),
                byte_size as i64,
                content_hash,
                modified_at
            ],
        )?;
        let document = self
            .get_document(&id)?
            .ok_or_else(|| anyhow::anyhow!("document not found after insert"))?;
        Ok((document, true))
    }

    pub fn get_document(&self, id: &str) -> Result<Option<Document>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                &format!("SELECT {DOC_COLUMNS} FROM documents WHERE id = ?1"),
                [id],
                map_document,
            )
            .optional()?)
    }

    pub fn list_documents(&self, source_id: &str) -> Result<Vec<Document>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {DOC_COLUMNS} FROM documents WHERE source_id = ?1 ORDER BY path"
        ))?;
        let rows = stmt.query_map([source_id], map_document)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_document_state(
        &self,
        document_id: &str,
        state: IngestState,
        error: Option<&str>,
    ) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE documents SET state = ?2, error = ?3 WHERE id = ?1",
            params![document_id, state.as_str(), error],
        )?;
        Ok(())
    }

    /// Replace a document's chunks and vectors, then mark it indexed — all in
    /// one transaction. A failure anywhere leaves the previous state intact and
    /// the document is never reported as searchable.
    pub fn replace_chunks(
        &self,
        document_id: &str,
        source_id: &str,
        model: &str,
        chunks: &[ChunkWithVector],
    ) -> Result<u32> {
        let now = crate::now_str();
        let count = chunks.len() as i64;
        self.db.transaction(|tx| {
            tx.execute(
                "DELETE FROM chunk_vectors WHERE chunk_id IN
                   (SELECT id FROM chunks WHERE document_id = ?1)",
                [document_id],
            )?;
            tx.execute("DELETE FROM chunks WHERE document_id = ?1", [document_id])?;

            for chunk in chunks {
                let chunk_id = otwono_types::new_id("chk");
                tx.execute(
                    "INSERT INTO chunks (id, document_id, source_id, chunk_index, text, locator, token_estimate)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        chunk_id, document_id, source_id, chunk.index as i64, chunk.text,
                        chunk.locator, chunk.token_estimate as i64
                    ],
                )?;
                tx.execute(
                    "INSERT INTO chunk_vectors (chunk_id, source_id, model, dim, vector)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        chunk_id, source_id, model, chunk.vector.len() as i64,
                        encode_vector(&chunk.vector)
                    ],
                )?;
            }

            tx.execute(
                "UPDATE documents SET state = 'indexed', chunk_count = ?2, indexed_at = ?3,
                        error = NULL WHERE id = ?1",
                params![document_id, count, now],
            )?;
            Ok(())
        })?;
        Ok(count as u32)
    }

    /// Remove a document that no longer exists on disk, and everything derived
    /// from it. This is deletion propagation.
    pub fn remove_document(&self, document_id: &str) -> Result<()> {
        self.db
            .conn()?
            .execute("DELETE FROM documents WHERE id = ?1", [document_id])?;
        Ok(())
    }

    /// Every chunk of every authorised source in `source_ids`, with its vector,
    /// for scoring. Only documents that reached `indexed` are returned.
    pub fn searchable_chunks(
        &self,
        source_ids: &[String],
    ) -> Result<Vec<SearchableChunk>> {
        if source_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; source_ids.len()].join(",");
        let sql = format!(
            "SELECT c.id, c.document_id, c.source_id, c.chunk_index, c.text, c.locator,
                    c.token_estimate, v.vector, d.file_name, d.path
               FROM chunks c
               JOIN chunk_vectors v ON v.chunk_id = c.id
               JOIN documents d ON d.id = c.document_id
               JOIN knowledge_sources s ON s.id = c.source_id
              WHERE c.source_id IN ({placeholders})
                AND s.authorised = 1
                AND d.state = 'indexed'"
        );
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(&sql)?;
        let binds: Vec<&dyn rusqlite::ToSql> = source_ids
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt.query_map(binds.as_slice(), |row| {
            let chunk = map_chunk(row)?;
            let vector: Vec<u8> = row.get(7)?;
            Ok((
                chunk,
                decode_vector(&vector),
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn chunks_for_document(&self, document_id: &str) -> Result<Vec<Chunk>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {CHUNK_COLUMNS} FROM chunks WHERE document_id = ?1 ORDER BY chunk_index"
        ))?;
        let rows = stmt.query_map([document_id], map_chunk)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(db: &Db) -> KnowledgeSource {
        KnowledgeRepo::new(db)
            .authorise_source(NewSource {
                label: "Docs".into(),
                root_path: "/home/u/docs".into(),
                is_directory: true,
                include_globs: vec![],
                exclude_globs: vec!["**/node_modules/**".into()],
            })
            .unwrap()
    }

    fn index_one(repo: &KnowledgeRepo<'_>, source_id: &str, name: &str, text: &str) -> Document {
        let (document, changed) = repo
            .upsert_document(
                source_id,
                &format!("/home/u/docs/{name}"),
                name,
                DocumentFormat::Markdown,
                text.len() as u64,
                "hash-1",
                None,
            )
            .unwrap();
        assert!(changed);
        repo.replace_chunks(
            &document.id,
            source_id,
            "test-embed",
            &[ChunkWithVector {
                index: 0,
                text: text.into(),
                locator: Some("lines 1-1".into()),
                token_estimate: 10,
                vector: vec![0.1, 0.2, 0.3],
            }],
        )
        .unwrap();
        repo.get_document(&document.id).unwrap().unwrap()
    }

    #[test]
    fn a_new_source_starts_authorised_with_the_fallback_embedding_model() {
        let db = Db::open_in_memory().unwrap();
        let source = source(&db);
        assert!(source.authorised);
        assert_eq!(source.embedding_model, LEXICAL_FALLBACK_MODEL);
        assert_eq!(source.document_count, 0);
    }

    #[test]
    fn authorising_the_same_path_twice_does_not_duplicate_it() {
        let db = Db::open_in_memory().unwrap();
        let first = source(&db);
        let second = source(&db);
        assert_eq!(first.id, second.id);
        assert_eq!(
            KnowledgeRepo::new(&db).list_sources(false).unwrap().len(),
            1
        );
    }

    #[test]
    fn a_document_is_only_searchable_once_indexing_finished() {
        let db = Db::open_in_memory().unwrap();
        let repo = KnowledgeRepo::new(&db);
        let source = source(&db);

        let (document, _) = repo
            .upsert_document(
                &source.id,
                "/home/u/docs/a.md",
                "a.md",
                DocumentFormat::Markdown,
                10,
                "h",
                None,
            )
            .unwrap();
        assert_eq!(document.state, IngestState::Pending);
        assert!(repo
            .searchable_chunks(std::slice::from_ref(&source.id))
            .unwrap()
            .is_empty());

        repo.set_document_state(&document.id, IngestState::Parsing, None)
            .unwrap();
        assert!(repo
            .searchable_chunks(std::slice::from_ref(&source.id))
            .unwrap()
            .is_empty());

        repo.replace_chunks(
            &document.id,
            &source.id,
            "test-embed",
            &[ChunkWithVector {
                index: 0,
                text: "hello".into(),
                locator: None,
                token_estimate: 1,
                vector: vec![1.0],
            }],
        )
        .unwrap();

        let reloaded = repo.get_document(&document.id).unwrap().unwrap();
        assert_eq!(reloaded.state, IngestState::Indexed);
        assert_eq!(reloaded.chunk_count, 1);
        assert!(reloaded.indexed_at.is_some());
        assert_eq!(repo.searchable_chunks(&[source.id]).unwrap().len(), 1);
    }

    #[test]
    fn a_failed_parse_records_the_error_and_stays_unsearchable() {
        let db = Db::open_in_memory().unwrap();
        let repo = KnowledgeRepo::new(&db);
        let source = source(&db);
        let (document, _) = repo
            .upsert_document(
                &source.id,
                "/home/u/docs/bad.pdf",
                "bad.pdf",
                DocumentFormat::Pdf,
                10,
                "h",
                None,
            )
            .unwrap();
        repo.set_document_state(&document.id, IngestState::Failed, Some("encrypted PDF"))
            .unwrap();

        let reloaded = repo.get_document(&document.id).unwrap().unwrap();
        assert_eq!(reloaded.state, IngestState::Failed);
        assert_eq!(reloaded.error.as_deref(), Some("encrypted PDF"));
        assert!(!reloaded.state.is_searchable());
        assert!(repo.searchable_chunks(&[source.id]).unwrap().is_empty());
    }

    #[test]
    fn revoking_a_source_deletes_its_chunks_and_vectors_immediately() {
        let db = Db::open_in_memory().unwrap();
        let repo = KnowledgeRepo::new(&db);
        let source = source(&db);
        index_one(&repo, &source.id, "a.md", "the answer is 42");
        assert_eq!(
            repo.searchable_chunks(std::slice::from_ref(&source.id)).unwrap().len(),
            1
        );

        repo.set_authorised(&source.id, false).unwrap();

        assert!(repo
            .searchable_chunks(std::slice::from_ref(&source.id))
            .unwrap()
            .is_empty());
        let chunk_count: i64 = db
            .conn()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        let vector_count: i64 = db
            .conn()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM chunk_vectors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(chunk_count, 0);
        assert_eq!(vector_count, 0);

        let documents = repo.list_documents(&source.id).unwrap();
        assert_eq!(documents[0].state, IngestState::Removed);
    }

    #[test]
    fn an_unchanged_file_is_not_re_parsed() {
        let db = Db::open_in_memory().unwrap();
        let repo = KnowledgeRepo::new(&db);
        let source = source(&db);
        index_one(&repo, &source.id, "a.md", "content");

        let (_, changed) = repo
            .upsert_document(
                &source.id,
                "/home/u/docs/a.md",
                "a.md",
                DocumentFormat::Markdown,
                7,
                "hash-1",
                None,
            )
            .unwrap();
        assert!(!changed, "an unchanged hash must not trigger re-parsing");

        let (document, changed) = repo
            .upsert_document(
                &source.id,
                "/home/u/docs/a.md",
                "a.md",
                DocumentFormat::Markdown,
                9,
                "hash-2",
                None,
            )
            .unwrap();
        assert!(changed, "a changed hash must trigger re-parsing");
        assert_eq!(document.state, IngestState::Pending);
    }

    #[test]
    fn re_indexing_replaces_chunks_rather_than_accumulating_them() {
        let db = Db::open_in_memory().unwrap();
        let repo = KnowledgeRepo::new(&db);
        let source = source(&db);
        let document = index_one(&repo, &source.id, "a.md", "first version");

        repo.replace_chunks(
            &document.id,
            &source.id,
            "test-embed",
            &[
                ChunkWithVector {
                    index: 0,
                    text: "second a".into(),
                    locator: None,
                    token_estimate: 2,
                    vector: vec![0.5],
                },
                ChunkWithVector {
                    index: 1,
                    text: "second b".into(),
                    locator: None,
                    token_estimate: 2,
                    vector: vec![0.6],
                },
            ],
        )
        .unwrap();

        let chunks = repo.chunks_for_document(&document.id).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "second a");
        let vector_count: i64 = db
            .conn()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM chunk_vectors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vector_count, 2);
    }

    #[test]
    fn deleting_a_document_removes_its_chunks_and_vectors() {
        let db = Db::open_in_memory().unwrap();
        let repo = KnowledgeRepo::new(&db);
        let source = source(&db);
        let document = index_one(&repo, &source.id, "a.md", "content");
        repo.remove_document(&document.id).unwrap();

        assert!(repo.searchable_chunks(&[source.id]).unwrap().is_empty());
        for table in ["chunks", "chunk_vectors"] {
            let count: i64 = db
                .conn()
                .unwrap()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0, "{table} should be empty");
        }
    }

    #[test]
    fn restoring_authorisation_lets_the_folder_be_indexed_again() {
        let db = Db::open_in_memory().unwrap();
        let repo = KnowledgeRepo::new(&db);
        let source = source(&db);
        index_one(&repo, &source.id, "a.md", "content");
        repo.set_authorised(&source.id, false).unwrap();

        let restored = repo
            .authorise_source(NewSource {
                label: "Docs".into(),
                root_path: "/home/u/docs".into(),
                is_directory: true,
                include_globs: vec![],
                exclude_globs: vec![],
            })
            .unwrap();
        assert_eq!(restored.id, source.id);
        assert!(restored.authorised);

        index_one(&repo, &source.id, "a.md", "content again");
        assert_eq!(repo.searchable_chunks(&[source.id]).unwrap().len(), 1);
    }

    #[test]
    fn vectors_survive_encoding_and_decoding_exactly() {
        let original = vec![0.0f32, -1.5, 3.25, 1e-6];
        assert_eq!(decode_vector(&encode_vector(&original)), original);
    }

    #[test]
    fn searching_no_sources_returns_nothing_rather_than_everything() {
        let db = Db::open_in_memory().unwrap();
        let repo = KnowledgeRepo::new(&db);
        let source = source(&db);
        index_one(&repo, &source.id, "a.md", "content");
        assert!(repo.searchable_chunks(&[]).unwrap().is_empty());
    }
}

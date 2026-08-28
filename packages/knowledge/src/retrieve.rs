//! Retrieval.
//!
//! Hybrid scoring: cosine similarity over stored vectors, plus a BM25-style
//! lexical score. The lexical half matters most when the index was built with
//! the fallback embedder, and it keeps exact terms (a product code, a name)
//! findable even with a real embedding model.

use std::collections::HashMap;

use anyhow::Result;

use otwono_store::repo::knowledge::KnowledgeRepo;
use otwono_store::Db;
use otwono_types::chat::Citation;
use otwono_types::knowledge::RetrievalHit;

use crate::embed::{cosine_similarity, tokenise, Embedder};

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub limit: usize,
    /// Hits scoring below this are dropped rather than padded in.
    pub min_score: f32,
    /// Weight given to the vector half. The lexical half gets `1 - this`.
    pub vector_weight: f32,
    /// At most this many chunks from any single document, so one long file
    /// cannot crowd out every other source.
    pub max_per_document: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 6,
            min_score: 0.05,
            vector_weight: 0.6,
            max_per_document: 3,
        }
    }
}

/// BM25 parameters. Standard values; the corpus is small enough that tuning
/// them would be over-fitting.
const BM25_K1: f32 = 1.5;
const BM25_B: f32 = 0.75;

pub struct Retriever<'a> {
    db: &'a Db,
    embedder: &'a Embedder,
    options: SearchOptions,
}

impl<'a> Retriever<'a> {
    pub fn new(db: &'a Db, embedder: &'a Embedder) -> Self {
        Self {
            db,
            embedder,
            options: SearchOptions::default(),
        }
    }

    pub fn with_options(mut self, options: SearchOptions) -> Self {
        self.options = options;
        self
    }

    /// Search the given sources. An empty `source_ids` returns nothing: the
    /// caller must name what the user authorised for this conversation.
    pub async fn search(&self, query: &str, source_ids: &[String]) -> Result<Vec<RetrievalHit>> {
        if query.trim().is_empty() || source_ids.is_empty() {
            return Ok(Vec::new());
        }

        let candidates = KnowledgeRepo::new(self.db).searchable_chunks(source_ids)?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let query_vector = self.embedder.embed_one(query).await?;
        let query_terms = tokenise(query);

        // Corpus statistics for BM25.
        let total_documents = candidates.len() as f32;
        let mut document_frequency: HashMap<&str, f32> = HashMap::new();
        let mut tokenised: Vec<Vec<String>> = Vec::with_capacity(candidates.len());
        let mut total_length = 0f32;
        for (chunk, _, _, _) in &candidates {
            let tokens = tokenise(&chunk.text);
            total_length += tokens.len() as f32;
            let mut seen: Vec<&str> = Vec::new();
            for term in &query_terms {
                if tokens.iter().any(|t| t == term) && !seen.contains(&term.as_str()) {
                    *document_frequency.entry(term.as_str()).or_insert(0.0) += 1.0;
                    seen.push(term.as_str());
                }
            }
            tokenised.push(tokens);
        }
        let average_length = if candidates.is_empty() {
            1.0
        } else {
            (total_length / total_documents).max(1.0)
        };

        let mut scored: Vec<RetrievalHit> = Vec::with_capacity(candidates.len());
        let mut raw_lexical: Vec<f32> = Vec::with_capacity(candidates.len());

        for (position, (chunk, vector, file_name, file_path)) in candidates.iter().enumerate() {
            let vector_score = cosine_similarity(&query_vector, vector).clamp(0.0, 1.0);

            let tokens = &tokenised[position];
            let length = tokens.len() as f32;
            let mut lexical = 0f32;
            for term in &query_terms {
                let frequency = tokens.iter().filter(|t| *t == term).count() as f32;
                if frequency == 0.0 {
                    continue;
                }
                let containing = document_frequency
                    .get(term.as_str())
                    .copied()
                    .unwrap_or(0.0);
                let idf = ((total_documents - containing + 0.5) / (containing + 0.5) + 1.0).ln();
                lexical += idf * (frequency * (BM25_K1 + 1.0))
                    / (frequency + BM25_K1 * (1.0 - BM25_B + BM25_B * length / average_length));
            }
            raw_lexical.push(lexical);

            scored.push(RetrievalHit {
                chunk: chunk.clone(),
                file_name: file_name.clone(),
                file_path: file_path.clone(),
                score: 0.0,
                vector_score,
                lexical_score: 0.0,
            });
        }

        // Normalise BM25 into 0..1 so the two halves are commensurable.
        let max_lexical = raw_lexical.iter().cloned().fold(0.0f32, f32::max);
        let vector_weight = self.options.vector_weight.clamp(0.0, 1.0);
        for (hit, lexical) in scored.iter_mut().zip(raw_lexical) {
            hit.lexical_score = if max_lexical > 0.0 {
                lexical / max_lexical
            } else {
                0.0
            };
            hit.score =
                vector_weight * hit.vector_score + (1.0 - vector_weight) * hit.lexical_score;
        }

        scored.sort_by(|a, b| b.score.total_cmp(&a.score));
        scored.retain(|hit| hit.score >= self.options.min_score);

        // Diversify: cap the number of chunks taken from any one document.
        let mut per_document: HashMap<String, usize> = HashMap::new();
        let mut selected = Vec::with_capacity(self.options.limit);
        for hit in scored {
            let count = per_document
                .entry(hit.chunk.document_id.clone())
                .or_insert(0);
            if *count >= self.options.max_per_document {
                continue;
            }
            *count += 1;
            selected.push(hit);
            if selected.len() >= self.options.limit {
                break;
            }
        }
        Ok(selected)
    }

    /// Turn hits into citations for a message.
    pub fn to_citations(hits: &[RetrievalHit]) -> Vec<Citation> {
        hits.iter()
            .map(|hit| Citation {
                source_id: hit.chunk.source_id.clone(),
                document_id: hit.chunk.document_id.clone(),
                file_name: hit.file_name.clone(),
                file_path: hit.file_path.clone(),
                chunk_index: hit.chunk.index,
                locator: hit.chunk.locator.clone(),
                excerpt: excerpt(&hit.chunk.text),
                score: hit.score,
            })
            .collect()
    }

    /// The label a citation is shown under, e.g. `handbook.pdf (page 4)`.
    pub fn citation_label(citation: &Citation) -> String {
        match &citation.locator {
            Some(locator) => format!("{} ({locator})", citation.file_name),
            None => citation.file_name.clone(),
        }
    }
}

/// A short, whole-word excerpt for display.
pub fn excerpt(text: &str) -> String {
    const LIMIT: usize = 280;
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= LIMIT {
        return collapsed;
    }
    let truncated: String = collapsed.chars().take(LIMIT).collect();
    match truncated.rfind(' ') {
        Some(position) if position > LIMIT / 2 => format!("{}…", &truncated[..position]),
        _ => format!("{truncated}…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Indexer;
    use otwono_store::repo::knowledge::{KnowledgeRepo, NewSource};
    use std::path::Path;

    async fn indexed_corpus(files: &[(&str, &str)]) -> (tempfile::TempDir, Db, String) {
        let tmp = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let path = Path::new(tmp.path()).join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, body).unwrap();
        }
        let db = Db::open_in_memory().unwrap();
        let source = KnowledgeRepo::new(&db)
            .authorise_source(NewSource {
                label: "Docs".into(),
                root_path: tmp.path().to_string_lossy().to_string(),
                is_directory: true,
                include_globs: vec![],
                exclude_globs: vec![],
            })
            .unwrap();
        let embedder = Embedder::lexical();
        Indexer::new(&db, &embedder)
            .ingest_source(&source.id)
            .await
            .unwrap();
        (tmp, db, source.id)
    }

    #[tokio::test]
    async fn a_question_finds_the_document_that_answers_it() {
        let (_tmp, db, source_id) = indexed_corpus(&[
            ("leave.md", "# Annual leave\n\nEvery employee receives 25 days of annual leave each year, plus public holidays."),
            ("bread.md", "# Sourdough\n\nA long cold fermentation develops flavour in sourdough bread."),
            ("expenses.md", "# Expenses\n\nSubmit expense claims within 30 days of the spend."),
        ]).await;

        let embedder = Embedder::lexical();
        let hits = Retriever::new(&db, &embedder)
            .search(
                "how many days of annual leave do employees get",
                &[source_id],
            )
            .await
            .unwrap();

        assert!(!hits.is_empty(), "expected at least one hit");
        assert_eq!(hits[0].file_name, "leave.md");
        assert!(hits[0].score > 0.0);
    }

    #[tokio::test]
    async fn every_hit_carries_a_citable_location() {
        let (_tmp, db, source_id) = indexed_corpus(&[(
            "handbook.md",
            &format!(
                "# Handbook\n\n{}",
                "The policy is stated clearly here. ".repeat(120)
            ),
        )])
        .await;

        let embedder = Embedder::lexical();
        let hits = Retriever::new(&db, &embedder)
            .search("what does the policy state", &[source_id])
            .await
            .unwrap();
        let citations = Retriever::to_citations(&hits);

        assert!(!citations.is_empty());
        for citation in &citations {
            assert_eq!(citation.file_name, "handbook.md");
            assert!(!citation.file_path.is_empty());
            assert!(citation.locator.is_some(), "a citation needs a location");
            assert!(!citation.excerpt.is_empty());
            assert!(Retriever::citation_label(citation).contains("handbook.md"));
            assert!(Retriever::citation_label(citation).contains('('));
        }
    }

    #[tokio::test]
    async fn searching_a_source_the_user_did_not_select_returns_nothing() {
        let (_tmp, db, _source_id) =
            indexed_corpus(&[("leave.md", "Annual leave is 25 days.")]).await;
        let embedder = Embedder::lexical();
        let retriever = Retriever::new(&db, &embedder);

        assert!(retriever
            .search("annual leave", &[])
            .await
            .unwrap()
            .is_empty());
        assert!(retriever
            .search("annual leave", &["src_not_mine".into()])
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn revoking_a_source_stops_it_being_retrieved_immediately() {
        let (_tmp, db, source_id) =
            indexed_corpus(&[("leave.md", "Annual leave is 25 days for every employee.")]).await;
        let embedder = Embedder::lexical();
        let retriever = Retriever::new(&db, &embedder);
        assert!(!retriever
            .search("annual leave", std::slice::from_ref(&source_id))
            .await
            .unwrap()
            .is_empty());

        KnowledgeRepo::new(&db)
            .set_authorised(&source_id, false)
            .unwrap();
        assert!(
            retriever
                .search("annual leave", &[source_id])
                .await
                .unwrap()
                .is_empty(),
            "a revoked source must not be retrievable, even once"
        );
    }

    #[tokio::test]
    async fn an_exact_term_is_found_even_though_the_index_has_no_semantics() {
        let (_tmp, db, source_id) = indexed_corpus(&[
            (
                "catalogue.md",
                "Product XJ-4471 ships in blue and graphite.",
            ),
            ("other.md", "Our returns policy allows 30 days."),
        ])
        .await;

        let embedder = Embedder::lexical();
        let hits = Retriever::new(&db, &embedder)
            .search("XJ-4471", &[source_id])
            .await
            .unwrap();
        assert_eq!(hits[0].file_name, "catalogue.md");
        assert!(
            hits[0].lexical_score > 0.0,
            "the lexical half should carry this"
        );
    }

    #[tokio::test]
    async fn one_long_document_cannot_crowd_out_every_other_source() {
        let long = "The annual leave policy is described here. ".repeat(400);
        let (_tmp, db, source_id) = indexed_corpus(&[
            ("long.md", &long),
            ("short.md", "Annual leave is confirmed by your manager."),
        ])
        .await;

        let embedder = Embedder::lexical();
        let hits = Retriever::new(&db, &embedder)
            .with_options(SearchOptions {
                max_per_document: 2,
                limit: 6,
                ..Default::default()
            })
            .search("annual leave policy", &[source_id])
            .await
            .unwrap();

        let from_long = hits.iter().filter(|h| h.file_name == "long.md").count();
        assert!(
            from_long <= 2,
            "expected at most two chunks from long.md, got {from_long}"
        );
        assert!(
            hits.iter().any(|h| h.file_name == "short.md"),
            "the other document should still appear: {:?}",
            hits.iter()
                .map(|h| h.file_name.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn an_unrelated_question_returns_nothing_rather_than_a_bad_guess() {
        let (_tmp, db, source_id) =
            indexed_corpus(&[("leave.md", "Annual leave is 25 days per year.")]).await;
        let embedder = Embedder::lexical();
        let hits = Retriever::new(&db, &embedder)
            .with_options(SearchOptions {
                min_score: 0.2,
                ..Default::default()
            })
            .search(
                "photosynthesis chlorophyll wavelength absorption",
                &[source_id],
            )
            .await
            .unwrap();
        assert!(
            hits.is_empty(),
            "got {:?}",
            hits.iter().map(|h| h.score).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn an_empty_query_returns_nothing() {
        let (_tmp, db, source_id) = indexed_corpus(&[("a.md", "text")]).await;
        let embedder = Embedder::lexical();
        assert!(Retriever::new(&db, &embedder)
            .search("   ", &[source_id])
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn excerpts_are_trimmed_on_a_word_boundary() {
        let long = "word ".repeat(200);
        let trimmed = excerpt(&long);
        assert!(trimmed.chars().count() <= 281);
        assert!(trimmed.ends_with('…'));
        assert!(!trimmed.contains("  "), "whitespace is collapsed");

        assert_eq!(excerpt("short text"), "short text");
    }
}

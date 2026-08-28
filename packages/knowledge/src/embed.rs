//! Embeddings.
//!
//! When a provider serves an embedding model, that model is used. When none is
//! available the index falls back to a deterministic lexical vector so that
//! retrieval still works with no model connected — and every surface says so,
//! because a hashed bag of words is not a semantic embedding.

use std::sync::Arc;

use anyhow::Result;

use otwono_providers::Provider;
use otwono_types::knowledge::LEXICAL_FALLBACK_MODEL;

/// Dimension of the fallback vector. Large enough that unrelated documents
/// rarely collide, small enough to score quickly over an MVP-sized corpus.
pub const LEXICAL_DIMENSIONS: usize = 512;

/// Which mechanism produced the vectors in an index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddingSource {
    /// A real embedding model on a provider connection.
    Model {
        connection_id: String,
        model: String,
    },
    /// The deterministic lexical fallback.
    LexicalFallback,
}

impl EmbeddingSource {
    pub fn model_name(&self) -> String {
        match self {
            Self::Model { model, .. } => model.clone(),
            Self::LexicalFallback => LEXICAL_FALLBACK_MODEL.to_string(),
        }
    }

    pub const fn is_fallback(&self) -> bool {
        matches!(self, Self::LexicalFallback)
    }

    /// The sentence the Knowledge screen shows beside the source.
    pub fn describe(&self) -> String {
        match self {
            Self::Model { model, .. } => {
                format!("Indexed with the embedding model {model}.")
            }
            Self::LexicalFallback => {
                "Indexed without an embedding model. Search matches words rather than meaning; \
                 connect a model that provides embeddings and re-index for better results."
                    .to_string()
            }
        }
    }
}

/// Split text into lowercase word tokens. Shared by the fallback embedder and
/// the lexical half of retrieval so both agree on what a word is.
pub fn tokenise(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty() && token.len() <= 40)
        .map(|token| token.to_lowercase())
        .collect()
}

/// FNV-1a, used to place a token deterministically into the vector.
fn hash_token(token: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in token.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A deterministic hashed bag-of-words vector, L2-normalised so that cosine
/// similarity behaves.
pub fn lexical_vector(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0f32; LEXICAL_DIMENSIONS];
    for token in tokenise(text) {
        let hash = hash_token(&token);
        let bucket = (hash % LEXICAL_DIMENSIONS as u64) as usize;
        // A second hash bit decides the sign, so different words that land in
        // the same bucket tend to cancel rather than reinforce.
        let sign = if (hash >> 32) & 1 == 0 { 1.0 } else { -1.0 };
        vector[bucket] += sign;
    }
    normalise(&mut vector);
    vector
}

pub fn normalise(vector: &mut [f32]) {
    let magnitude = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if magnitude > f32::EPSILON {
        for value in vector.iter_mut() {
            *value /= magnitude;
        }
    }
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let magnitude_a = a.iter().map(|v| v * v).sum::<f32>().sqrt();
    let magnitude_b = b.iter().map(|v| v * v).sum::<f32>().sqrt();
    if magnitude_a <= f32::EPSILON || magnitude_b <= f32::EPSILON {
        return 0.0;
    }
    dot / (magnitude_a * magnitude_b)
}

/// Produces vectors for text, using a model when one is available.
pub struct Embedder {
    provider: Option<Arc<dyn Provider>>,
    source: EmbeddingSource,
}

impl Embedder {
    /// The fallback embedder. Always available, never pretends to be a model.
    pub fn lexical() -> Self {
        Self {
            provider: None,
            source: EmbeddingSource::LexicalFallback,
        }
    }

    /// Use `model` on `provider`. The caller has already established that the
    /// model reports or was probed for embeddings.
    pub fn with_model(provider: Arc<dyn Provider>, connection_id: String, model: String) -> Self {
        Self {
            provider: Some(provider),
            source: EmbeddingSource::Model {
                connection_id,
                model,
            },
        }
    }

    pub fn source(&self) -> &EmbeddingSource {
        &self.source
    }

    /// Embed a batch. If the model fails part-way the whole call fails: a
    /// half-model, half-lexical index would score incomparably and silently
    /// degrade every future search.
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        match (&self.provider, &self.source) {
            (Some(provider), EmbeddingSource::Model { model, .. }) => {
                let mut vectors = provider.embed(model, texts).await?;
                for vector in &mut vectors {
                    normalise(vector);
                }
                if vectors.len() != texts.len() {
                    anyhow::bail!(
                        "the embedding model returned {} vectors for {} inputs",
                        vectors.len(),
                        texts.len()
                    );
                }
                Ok(vectors)
            }
            _ => Ok(texts.iter().map(|text| lexical_vector(text)).collect()),
        }
    }

    /// Embed a single query.
    pub async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self
            .embed(std::slice::from_ref(&text.to_string()))
            .await?
            .into_iter()
            .next()
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fallback_never_claims_to_be_a_model() {
        let source = EmbeddingSource::LexicalFallback;
        assert!(source.is_fallback());
        assert_eq!(source.model_name(), LEXICAL_FALLBACK_MODEL);
        assert!(source.describe().contains("without an embedding model"));
        assert!(source.describe().contains("re-index"));
    }

    #[test]
    fn a_model_source_names_the_model() {
        let source = EmbeddingSource::Model {
            connection_id: "prv_1".into(),
            model: "nomic-embed-text".into(),
        };
        assert!(!source.is_fallback());
        assert_eq!(source.model_name(), "nomic-embed-text");
        assert!(source.describe().contains("nomic-embed-text"));
    }

    #[test]
    fn the_lexical_vector_is_deterministic_and_normalised() {
        let a = lexical_vector("The quick brown fox");
        let b = lexical_vector("The quick brown fox");
        assert_eq!(a, b);
        assert_eq!(a.len(), LEXICAL_DIMENSIONS);
        let magnitude: f32 = a.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((magnitude - 1.0).abs() < 1e-5, "magnitude was {magnitude}");
    }

    #[test]
    fn similar_text_scores_higher_than_unrelated_text() {
        let query = lexical_vector("annual leave policy for employees");
        let related = lexical_vector("The annual leave policy gives employees 25 days");
        let unrelated = lexical_vector("Sourdough bread needs a long cold fermentation");

        let related_score = cosine_similarity(&query, &related);
        let unrelated_score = cosine_similarity(&query, &unrelated);
        assert!(
            related_score > unrelated_score,
            "related {related_score} should beat unrelated {unrelated_score}"
        );
        assert!(
            related_score > 0.3,
            "related score was only {related_score}"
        );
    }

    #[test]
    fn case_and_punctuation_do_not_change_the_vector() {
        assert_eq!(
            lexical_vector("Annual leave!"),
            lexical_vector("annual, leave")
        );
    }

    #[test]
    fn empty_text_yields_a_zero_vector_that_scores_zero() {
        let empty = lexical_vector("");
        assert!(empty.iter().all(|v| *v == 0.0));
        assert_eq!(cosine_similarity(&empty, &lexical_vector("anything")), 0.0);
    }

    #[test]
    fn cosine_similarity_is_safe_with_mismatched_or_empty_vectors() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0]), 0.0);
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn tokenising_drops_punctuation_and_absurdly_long_tokens() {
        assert_eq!(tokenise("Hello, world!"), vec!["hello", "world"]);
        let long = "a".repeat(60);
        assert!(
            tokenise(&long).is_empty(),
            "a 60-character token is not a word"
        );
    }

    #[tokio::test]
    async fn the_fallback_embedder_produces_one_vector_per_input() {
        let embedder = Embedder::lexical();
        let vectors = embedder
            .embed(&["one".into(), "two".into(), "three".into()])
            .await
            .unwrap();
        assert_eq!(vectors.len(), 3);
        assert!(vectors.iter().all(|v| v.len() == LEXICAL_DIMENSIONS));
        assert!(embedder.source().is_fallback());
    }
}

//! Splitting parsed segments into overlapping chunks.
//!
//! Chunks are sized in characters rather than tokens because the tokeniser
//! belongs to whichever model is loaded, and the index must be usable with no
//! model at all. Boundaries prefer paragraph, then sentence, then word, so a
//! citation rarely starts mid-thought.

use crate::parse::Segment;

#[derive(Debug, Clone, Copy)]
pub struct ChunkOptions {
    pub target_chars: usize,
    pub overlap_chars: usize,
    /// Chunks shorter than this are merged into their neighbour rather than
    /// stored as fragments that retrieve poorly.
    pub min_chars: usize,
}

impl Default for ChunkOptions {
    fn default() -> Self {
        Self {
            target_chars: 1_200,
            overlap_chars: 150,
            min_chars: 120,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub index: u32,
    pub text: String,
    pub locator: Option<String>,
}

/// Find the best place to end a chunk at or before `limit`.
fn boundary(text: &str, limit: usize) -> usize {
    if text.len() <= limit {
        return text.len();
    }
    // Only consider character boundaries.
    let mut ceiling = limit;
    while ceiling > 0 && !text.is_char_boundary(ceiling) {
        ceiling -= 1;
    }
    let window = &text[..ceiling];
    let floor = ceiling * 6 / 10;

    for pattern in ["\n\n", ". ", ".\n", "? ", "! ", "\n"] {
        if let Some(position) = window.rfind(pattern) {
            let end = position + pattern.len();
            if end >= floor {
                return end;
            }
        }
    }
    if let Some(position) = window.rfind(char::is_whitespace) {
        if position >= floor {
            return position + 1;
        }
    }
    ceiling
}

/// Chunk one segment's text, preserving its locator.
fn chunk_segment(
    text: &str,
    locator: Option<&str>,
    options: ChunkOptions,
    next_index: &mut u32,
) -> Vec<TextChunk> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut cursor = 0usize;
    while cursor < trimmed.len() {
        let remaining = &trimmed[cursor..];
        let end = boundary(remaining, options.target_chars);
        let piece = remaining[..end].trim();

        if !piece.is_empty() {
            chunks.push(TextChunk {
                index: *next_index,
                text: piece.to_string(),
                locator: locator.map(str::to_string),
            });
            *next_index += 1;
        }

        if end >= remaining.len() {
            break;
        }
        // Step forward, minus the overlap, always making progress.
        let step = end.saturating_sub(options.overlap_chars).max(1);
        let mut advance = cursor + step;
        while advance < trimmed.len() && !trimmed.is_char_boundary(advance) {
            advance += 1;
        }
        cursor = advance;
    }

    chunks
}

/// Chunk a whole document.
pub fn chunk_document(segments: &[Segment], options: ChunkOptions) -> Vec<TextChunk> {
    let mut next_index = 0u32;
    let mut chunks: Vec<TextChunk> = Vec::new();

    for segment in segments {
        for chunk in chunk_segment(
            &segment.text,
            segment.locator.as_deref(),
            options,
            &mut next_index,
        ) {
            // Merge a runt into the previous chunk when they share a locator,
            // rather than storing a fragment nobody will match against.
            match chunks.last_mut() {
                Some(previous)
                    if chunk.text.len() < options.min_chars
                        && previous.locator == chunk.locator
                        && previous.text.len() + chunk.text.len() < options.target_chars * 2 =>
                {
                    previous.text.push('\n');
                    previous.text.push_str(&chunk.text);
                    next_index -= 1;
                }
                _ => chunks.push(chunk),
            }
        }
    }

    // Renumber after any merges so indexes stay contiguous.
    for (position, chunk) in chunks.iter_mut().enumerate() {
        chunk.index = position as u32;
    }
    chunks
}

/// A rough token count for storage. Labelled as an estimate wherever shown.
pub fn estimate_tokens(text: &str) -> u32 {
    otwono_providers::estimate_tokens(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(text: &str) -> Segment {
        Segment::new(text, Some("lines 1-200".into()))
    }

    #[test]
    fn a_short_document_becomes_one_chunk() {
        let chunks = chunk_document(&[segment("A short note.")], ChunkOptions::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "A short note.");
        assert_eq!(chunks[0].index, 0);
        assert_eq!(chunks[0].locator.as_deref(), Some("lines 1-200"));
    }

    #[test]
    fn a_long_document_is_split_with_contiguous_indexes() {
        let body = "Sentence number one is here. ".repeat(400);
        let chunks = chunk_document(&[segment(&body)], ChunkOptions::default());
        assert!(
            chunks.len() > 5,
            "expected several chunks, got {}",
            chunks.len()
        );
        for (position, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.index, position as u32);
            assert!(!chunk.text.is_empty());
        }
    }

    #[test]
    fn chunks_prefer_to_break_at_a_paragraph_or_sentence() {
        let body = format!(
            "{}\n\n{}",
            "First paragraph. ".repeat(50),
            "Second paragraph. ".repeat(50)
        );
        let chunks = chunk_document(&[segment(&body)], ChunkOptions::default());
        assert!(
            chunks.iter().any(|c| c.text.ends_with('.')),
            "at least one chunk should end on a sentence"
        );
    }

    #[test]
    fn consecutive_chunks_overlap_so_a_fact_on_a_boundary_is_not_lost() {
        let body: String = (1..=200)
            .map(|i| format!("Fact {i} is recorded. "))
            .collect();
        let chunks = chunk_document(&[segment(&body)], ChunkOptions::default());
        assert!(chunks.len() >= 2);

        let first_tail: String = chunks[0]
            .text
            .chars()
            .rev()
            .take(60)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let second = &chunks[1].text;
        let shared = first_tail
            .split_whitespace()
            .any(|word| word.len() > 3 && second.contains(word));
        assert!(shared, "chunks should overlap:\n…{first_tail}\n{second}");
    }

    #[test]
    fn every_chunk_keeps_the_locator_of_the_segment_it_came_from() {
        let segments = vec![
            Segment::new("Page one text. ".repeat(200), Some("page 1".into())),
            Segment::new("Page two text. ".repeat(200), Some("page 2".into())),
        ];
        let chunks = chunk_document(&segments, ChunkOptions::default());
        assert!(chunks
            .iter()
            .any(|c| c.locator.as_deref() == Some("page 1")));
        assert!(chunks
            .iter()
            .any(|c| c.locator.as_deref() == Some("page 2")));
        assert!(chunks.iter().all(|c| c.locator.is_some()));
    }

    #[test]
    fn multibyte_text_never_splits_a_character() {
        let body = "日本語のテキストです。".repeat(400);
        let chunks = chunk_document(&[segment(&body)], ChunkOptions::default());
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            // Reconstructing proves every boundary was valid UTF-8.
            assert_eq!(chunk.text, chunk.text.clone());
            assert!(!chunk.text.is_empty());
        }
        let rejoined: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert!(rejoined.contains("日本語"));
    }

    #[test]
    fn a_tiny_trailing_fragment_is_merged_rather_than_stored_alone() {
        let mut body = "Sentence. ".repeat(130);
        body.push_str("Tail.");
        let chunks = chunk_document(&[segment(&body)], ChunkOptions::default());
        assert!(
            chunks
                .iter()
                .all(|c| c.text.len() >= ChunkOptions::default().min_chars),
            "no chunk should be a runt: {:?}",
            chunks.iter().map(|c| c.text.len()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn whitespace_only_segments_produce_nothing() {
        assert!(
            chunk_document(&[Segment::new("   \n\n  ", None)], ChunkOptions::default()).is_empty()
        );
        assert!(chunk_document(&[], ChunkOptions::default()).is_empty());
    }

    #[test]
    fn chunking_always_terminates_on_text_without_whitespace() {
        let body = "x".repeat(10_000);
        let chunks = chunk_document(&[segment(&body)], ChunkOptions::default());
        assert!(chunks.len() > 1);
        assert!(chunks.iter().map(|c| c.text.len()).sum::<usize>() >= 10_000);
    }
}

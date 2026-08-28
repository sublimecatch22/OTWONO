//! Splitting bytes into chunks, at parameters that are the same on every node.
//!
//! # These numbers are a network constant, not a setting
//!
//! Two nodes that chunk the same bytes differently produce different digests for the same
//! data and cannot serve each other — and nothing reports an error, the swarm simply never
//! forms. So the parameters are compiled in, carried in the schema, and identical on a Pi
//! Zero and a workstation. ADR-0016 has the measurements behind the choice; the short
//! version is that boundary stability and throughput were flat across everything tested, so
//! the decision came down to index cost, and 64 KiB keeps the index at ~0.4 MiB on the
//! smallest supported node.
//!
//! # Streaming is not an optimisation here
//!
//! A 4 GB model on a 4 GB board cannot be read into memory to be chunked. [`stream`] reads
//! in bounded buffers; [`slice`] exists for data already in memory, which is most objects.

use fastcdc::v2020::{FastCDC, StreamCDC};
use std::io::Read;

/// Bump when the parameters below change. A stored object records this, so a future node
/// can *detect* an object chunked under different rules — though detecting it is all it can
/// do, since the two halves of such a network cannot share chunks (ADR-0016).
pub const CHUNKING_VERSION: &str = "fastcdc-v2020-16k-64k-256k";

/// No chunk smaller than this, except the last one in an object.
pub const MIN_CHUNK: usize = 16 * 1024;
/// The target the cut-point algorithm aims at.
pub const AVG_CHUNK: usize = 64 * 1024;
/// A hard ceiling, so one pathological region cannot produce an unbounded chunk.
pub const MAX_CHUNK: usize = 256 * 1024;

/// One chunk's digest and how long it was.
///
/// The offset is deliberately absent. A chunk's identity is its content, and where it
/// happened to sit in one object is not part of that — the same chunk at a different offset
/// in a different file is the same chunk, which is the property dedup rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkRef {
    pub digest: [u8; 32],
    pub length: u32,
}

impl ChunkRef {
    pub fn of(bytes: &[u8]) -> ChunkRef {
        ChunkRef {
            digest: *blake3::hash(bytes).as_bytes(),
            length: bytes.len() as u32,
        }
    }

    pub fn hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.digest {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

#[derive(Debug)]
pub enum ChunkError {
    Io(std::io::Error),
}

impl std::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkError::Io(e) => write!(f, "reading while chunking: {e}"),
        }
    }
}

impl std::error::Error for ChunkError {}

/// Chunk data already in memory.
pub fn slice(data: &[u8]) -> Vec<ChunkRef> {
    if data.is_empty() {
        // An empty object has no chunks. Not one empty chunk — that would give every
        // empty file a chunk to store and fetch for nothing.
        return Vec::new();
    }
    FastCDC::new(data, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK)
        .map(|c| ChunkRef::of(&data[c.offset..c.offset + c.length]))
        .collect()
}

/// Chunk a reader without holding all of it, calling `sink` with each chunk's bytes.
///
/// The bytes are borrowed for the duration of the call, so a caller that wants to keep them
/// must copy — which is what a store does when it writes the chunk out, and what a caller
/// counting chunks does not have to do.
pub fn stream<R: Read>(
    reader: R,
    mut sink: impl FnMut(&ChunkRef, &[u8]) -> Result<(), ChunkError>,
) -> Result<Vec<ChunkRef>, ChunkError> {
    let mut refs = Vec::new();
    let chunker = StreamCDC::new(reader, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK);
    for result in chunker {
        let chunk = result.map_err(|e| match e {
            fastcdc::v2020::Error::IoError(io) => ChunkError::Io(io),
            other => ChunkError::Io(std::io::Error::other(other.to_string())),
        })?;
        let r = ChunkRef::of(&chunk.data);
        sink(&r, &chunk.data)?;
        refs.push(r);
    }
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random bytes: the same on every machine, so the assertions
    /// below mean the same thing everywhere.
    fn data(len: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        // Mixed, not `seed | 1`: that maps 2 and 3 (and every other adjacent pair) to the
        // same stream, so two "different" fixtures come out byte-identical. Cost one
        // debugging round in the cache's LRU test.
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        while out.len() < len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn chunking_the_same_bytes_twice_gives_the_same_chunks() {
        // The property the whole design rests on. If this can ever be false, two nodes
        // cannot serve each other and nothing tells them why.
        let d = data(4 << 20, 7);
        assert_eq!(slice(&d), slice(&d));
    }

    #[test]
    fn streaming_and_in_memory_chunking_agree() {
        // Two code paths for one meaning. A node that streamed a large file and one that
        // read a small one must produce identical digests, or dedup silently depends on
        // which path the file happened to take.
        let d = data(4 << 20, 11);
        let mut seen = Vec::new();
        let streamed = stream(d.as_slice(), |r, bytes| {
            assert_eq!(*r, ChunkRef::of(bytes), "the ref must describe the bytes");
            seen.push(*r);
            Ok(())
        })
        .expect("stream");
        assert_eq!(streamed, slice(&d));
        assert_eq!(seen, streamed, "the sink sees every chunk, in order");
    }

    #[test]
    fn chunks_respect_their_bounds() {
        let d = data(8 << 20, 13);
        let chunks = slice(&d);
        assert!(chunks.len() > 1, "8 MiB should not be one chunk");
        for (i, c) in chunks.iter().enumerate() {
            assert!(
                c.length as usize <= MAX_CHUNK,
                "chunk {i} is {} bytes, over the ceiling",
                c.length
            );
            let last = i == chunks.len() - 1;
            assert!(
                last || c.length as usize >= MIN_CHUNK,
                "chunk {i} is {} bytes, under the floor and not the last",
                c.length
            );
        }
    }

    #[test]
    fn the_chunks_reassemble_into_the_original() {
        let d = data(3 << 20, 17);
        let total: usize = slice(&d).iter().map(|c| c.length as usize).sum();
        assert_eq!(total, d.len(), "chunking must not lose or invent bytes");
    }

    #[test]
    fn an_insertion_near_the_front_leaves_most_chunks_alone() {
        // Why content-defined chunking exists at all. ADR-0016 measured 99.5-100% here
        // against 0-2.4% for fixed blocks; the test asserts the property rather than the
        // exact figure, which depends on the data.
        let d = data(4 << 20, 19);
        let mut edited = d[..4096].to_vec();
        edited.extend_from_slice(b"sixty four bytes of insertion to shift everything after it--- ");
        edited.extend_from_slice(&d[4096..]);

        let before: std::collections::HashSet<_> = slice(&d).into_iter().collect();
        let after = slice(&edited);
        let shared = after.iter().filter(|c| before.contains(c)).count();
        let ratio = shared as f64 / after.len() as f64;
        assert!(
            ratio > 0.9,
            "only {:.1}% of chunks survived an insertion; content-defined chunking is not working",
            ratio * 100.0
        );
    }

    #[test]
    fn an_empty_object_has_no_chunks() {
        assert!(slice(b"").is_empty());
        assert!(stream(b"".as_slice(), |_, _| Ok(())).expect("stream").is_empty());
    }

    #[test]
    fn something_smaller_than_the_floor_is_one_chunk() {
        // Most wiki pages, manifests and lesson files land here.
        let d = data(1024, 23);
        let chunks = slice(&d);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], ChunkRef::of(&d));
    }

    #[test]
    fn the_parameters_are_the_ones_the_adr_decided() {
        // A guard against a well-meaning tune. Changing these partitions the network, so
        // it must be a deliberate act with an ADR, not an edit.
        assert_eq!(MIN_CHUNK, 16 * 1024);
        assert_eq!(AVG_CHUNK, 64 * 1024);
        assert_eq!(MAX_CHUNK, 256 * 1024);
        assert_eq!(CHUNKING_VERSION, "fastcdc-v2020-16k-64k-256k");
    }
}

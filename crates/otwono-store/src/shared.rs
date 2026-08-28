//! `SHARED` objects: encrypt first, chunk the ciphertext (ADR-0019 §1).
//!
//! # Why this order
//!
//! Chunking the plaintext and encrypting each chunk would keep chunk digests over
//! plaintext, and that is a disclosure twice over. A holder who *guesses* the plaintext can
//! confirm the guess against the digest, which for anything predictable — a form, a
//! template, a known file — is a real leak. And the object's [`ContentId`] would be
//! identical to the id of the same file stored `PUBLIC`, so merely holding a `SHARED`
//! object would tell its holder which known file it is.
//!
//! Encrypting first costs deduplication and gives two shares of one file two unrelated
//! ids. Both follow from the encryption meaning anything.
//!
//! [`ContentId`]: crate::ContentId
//!
//! # Framed, not one AEAD over the whole object
//!
//! ADR-0019 says "encrypted as a whole, before chunking". A single AEAD invocation over the
//! whole object would mean holding it in memory to seal it and again to open it, which is
//! exactly what ADR-0018 exists to avoid — a `SHARED` video is not smaller than a `PUBLIC`
//! one. So the plaintext is sealed in fixed frames using the STREAM construction from the
//! `chacha20poly1305` crate, and the concatenated frames are what gets chunked.
//!
//! Every property ADR-0019 §1 asks for survives: chunk boundaries fall on ciphertext,
//! digests are over ciphertext, and the `ContentId` is over ciphertext. What framing adds
//! is that the object can be sealed and opened a megabyte at a time.
//!
//! STREAM rather than independently-nonced frames because it is the construction that
//! actually solves the two problems independent frames have: a truncated object and a
//! reordered one. Each frame's nonce carries its index, and the last frame is tagged as
//! last, so dropping the tail or swapping two frames fails to decrypt instead of decrypting
//! into a plausible shorter object. Rolling that by hand is how it gets got wrong.
//!
//! # What this module does not decide
//!
//! Who may read the object. That is the recipient list and the serving check, and it lives
//! with the object record and the daemon respectively. This module only turns bytes into
//! sealed bytes and back given a key.

use chacha20poly1305::aead::stream::{DecryptorBE32, EncryptorBE32};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305};
use std::io::{Read, Write};
use zeroize::Zeroizing;

/// Plaintext bytes per frame.
///
/// A megabyte is well above the 256 KiB maximum chunk, so framing never forces a chunk
/// boundary, and small enough that a node with 512 MiB of RAM can seal and open without
/// noticing. The tag overhead is 16 bytes per frame — 16 parts per million.
pub const FRAME_BYTES: usize = 1024 * 1024;

/// Poly1305's tag, per frame.
const TAG_BYTES: usize = 16;

/// Bytes of the nonce that are chosen at random. STREAM's BE32 variant spends the
/// remaining five on a big-endian frame counter and a last-frame flag.
pub const NONCE_PREFIX_BYTES: usize = 19;

/// Recorded in the object so a future change of scheme is detectable rather than silently
/// mis-decrypted. Anything that does not recognise this value must refuse, not guess.
pub const SHARED_ENCRYPTION: &str = "xchacha20poly1305-stream-be32-1MiB";

/// The per-object key a `SHARED` object is encrypted with.
///
/// Fresh for every object. Never derived from the storage key, the node's identity, or the
/// content: an object shared twice to different people is two unrelated objects, and that
/// is a property worth having rather than a cost.
pub struct ContentKey(Zeroizing<[u8; 32]>);

impl ContentKey {
    /// A fresh key from the OS. Infallible for the same reason [`StorageKey::generate`]
    /// is: a system that cannot produce 32 random bytes has no useful degraded mode here.
    ///
    /// [`StorageKey::generate`]: crate::StorageKey::generate
    pub fn generate() -> ContentKey {
        let mut bytes = Zeroizing::new([0u8; 32]);
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, bytes.as_mut());
        ContentKey(bytes)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> ContentKey {
        ContentKey(Zeroizing::new(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Prints nothing about the key. A content key in a log is the object in the log.
impl std::fmt::Debug for ContentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ContentKey(<redacted>)")
    }
}

#[derive(Debug)]
pub enum SharedError {
    Io(std::io::Error),
    /// A frame did not authenticate: the wrong key, damage, truncation, or reordering.
    /// Deliberately one variant — which of those it was is not something a caller can act
    /// on differently, and telling them apart is where a padding-oracle lives.
    Undecryptable,
    /// The recorded nonce prefix is not [`NONCE_PREFIX_BYTES`] bytes.
    BadNonce {
        len: usize,
    },
    /// The record names a scheme this build does not implement.
    UnknownScheme(String),
    /// Sealing failed. Cannot happen with a well-formed key and nonce, and is here because
    /// swallowing it with an unwrap would turn a library bug into a panic in a daemon.
    Unsealable,
}

impl std::fmt::Display for SharedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SharedError::Io(e) => write!(f, "{e}"),
            SharedError::Undecryptable => write!(
                f,
                "a frame did not authenticate: the wrong content key, damage, or an object \
                 that has been truncated or reordered"
            ),
            SharedError::BadNonce { len } => {
                write!(f, "a nonce prefix is {NONCE_PREFIX_BYTES} bytes, not {len}")
            }
            SharedError::Unsealable => {
                write!(f, "sealing a frame failed, which should not be possible")
            }
            SharedError::UnknownScheme(s) => write!(
                f,
                "this object is encrypted as {s:?}, which this build does not implement; \
                 refusing rather than guessing"
            ),
        }
    }
}

impl std::error::Error for SharedError {}

impl From<std::io::Error> for SharedError {
    fn from(e: std::io::Error) -> SharedError {
        SharedError::Io(e)
    }
}

/// A fresh nonce prefix. One per object, never reused with the same key.
pub fn nonce_prefix() -> [u8; NONCE_PREFIX_BYTES] {
    let mut prefix = [0u8; NONCE_PREFIX_BYTES];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut prefix);
    prefix
}

/// Ciphertext length for a given plaintext length, without producing it.
///
/// Used to state an object's sealed size before sealing it, and to check afterwards that
/// what was written is what was expected.
pub fn sealed_len(plaintext_len: u64) -> u64 {
    let frame = FRAME_BYTES as u64;
    // A zero-length plaintext still costs one frame: STREAM must emit a last frame, or
    // there would be nothing to tag as last and truncation would be undetectable.
    let frames = if plaintext_len == 0 {
        1
    } else {
        plaintext_len.div_ceil(frame)
    };
    plaintext_len + frames * TAG_BYTES as u64
}

/// Seal `plaintext` into `ciphertext`, a frame at a time. Returns the plaintext length.
pub fn seal(
    key: &ContentKey,
    prefix: &[u8; NONCE_PREFIX_BYTES],
    mut plaintext: impl Read,
    mut ciphertext: impl Write,
) -> Result<u64, SharedError> {
    let cipher = XChaCha20Poly1305::new(key.as_bytes().into());
    let mut stream = EncryptorBE32::from_aead(cipher, prefix.as_slice().into());
    let mut buf = Zeroizing::new(vec![0u8; FRAME_BYTES]);
    let mut total = 0u64;

    // One frame is read ahead of what is written, because the final frame has to be sealed
    // with last_encrypt rather than next_encrypt and there is no way to know a frame is the
    // last one until the read after it comes back empty.
    let mut pending = read_frame(&mut plaintext, &mut buf)?;
    loop {
        let next = if pending == FRAME_BYTES {
            let mut lookahead = Zeroizing::new(vec![0u8; FRAME_BYTES]);
            let n = read_frame(&mut plaintext, &mut lookahead)?;
            Some((lookahead, n))
        } else {
            None
        };
        total += pending as u64;
        match next {
            Some((lookahead, n)) if n > 0 => {
                let sealed = stream
                    .encrypt_next(&buf[..pending])
                    .map_err(|_| SharedError::Unsealable)?;
                ciphertext.write_all(&sealed)?;
                *buf = (*lookahead).clone();
                pending = n;
            }
            _ => {
                let sealed = stream
                    .encrypt_last(&buf[..pending])
                    .map_err(|_| SharedError::Unsealable)?;
                ciphertext.write_all(&sealed)?;
                break;
            }
        }
    }
    ciphertext.flush()?;
    Ok(total)
}

/// Open `ciphertext` into `plaintext`, a frame at a time. Returns the plaintext length.
///
/// Fails on a truncated object rather than returning a short one: the last frame carries a
/// flag saying it is last, so an object cut short does not authenticate.
pub fn open(
    key: &ContentKey,
    prefix: &[u8; NONCE_PREFIX_BYTES],
    mut ciphertext: impl Read,
    mut plaintext: impl Write,
) -> Result<u64, SharedError> {
    let cipher = XChaCha20Poly1305::new(key.as_bytes().into());
    let mut stream = DecryptorBE32::from_aead(cipher, prefix.as_slice().into());
    let sealed_frame = FRAME_BYTES + TAG_BYTES;
    let mut buf = vec![0u8; sealed_frame];
    let mut total = 0u64;

    let mut pending = read_frame(&mut ciphertext, &mut buf)?;
    loop {
        let next = if pending == sealed_frame {
            let mut lookahead = vec![0u8; sealed_frame];
            let n = read_frame(&mut ciphertext, &mut lookahead)?;
            Some((lookahead, n))
        } else {
            None
        };
        match next {
            Some((lookahead, n)) if n > 0 => {
                let opened = stream
                    .decrypt_next(&buf[..pending])
                    .map_err(|_| SharedError::Undecryptable)?;
                let opened = Zeroizing::new(opened);
                total += opened.len() as u64;
                plaintext.write_all(&opened)?;
                buf = lookahead;
                pending = n;
            }
            _ => {
                let opened = stream
                    .decrypt_last(&buf[..pending])
                    .map_err(|_| SharedError::Undecryptable)?;
                let opened = Zeroizing::new(opened);
                total += opened.len() as u64;
                plaintext.write_all(&opened)?;
                break;
            }
        }
    }
    plaintext.flush()?;
    Ok(total)
}

/// Fill `buf` as far as the reader will go. A short read is not the end of the input.
fn read_frame(mut r: impl Read, buf: &mut [u8]) -> Result<usize, SharedError> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Decode a recorded nonce prefix, refusing anything of the wrong length.
pub fn decode_prefix(bytes: &[u8]) -> Result<[u8; NONCE_PREFIX_BYTES], SharedError> {
    bytes
        .try_into()
        .map_err(|_| SharedError::BadNonce { len: bytes.len() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random bytes. Multiplied by an odd constant first, so adjacent
    /// seeds cannot map to the same stream — the mistake that made a cache test silently
    /// exercise one object where it claimed three.
    fn bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn round_trip(plaintext: &[u8]) -> Vec<u8> {
        let key = ContentKey::generate();
        let prefix = nonce_prefix();
        let mut sealed = Vec::new();
        let written = seal(&key, &prefix, plaintext, &mut sealed).unwrap();
        assert_eq!(written, plaintext.len() as u64);
        assert_eq!(
            sealed.len() as u64,
            sealed_len(plaintext.len() as u64),
            "sealed_len must predict what seal actually produces"
        );

        let mut opened = Vec::new();
        let read = open(&key, &prefix, sealed.as_slice(), &mut opened).unwrap();
        assert_eq!(read, plaintext.len() as u64);
        assert_eq!(opened, plaintext);
        sealed
    }

    #[test]
    fn a_single_frame_object_round_trips() {
        round_trip(&bytes(1, 4096));
    }

    #[test]
    fn an_empty_object_round_trips() {
        // Zero bytes still costs one frame, because there has to be something to tag as
        // last or a truncation to nothing would be undetectable.
        let sealed = round_trip(&[]);
        assert_eq!(sealed.len(), 16);
    }

    #[test]
    fn an_object_that_is_exactly_one_frame_round_trips() {
        // The boundary the read-ahead exists for: a full read followed by an empty one.
        round_trip(&bytes(2, FRAME_BYTES));
    }

    #[test]
    fn a_multi_frame_object_round_trips() {
        round_trip(&bytes(3, FRAME_BYTES * 2 + 1234));
    }

    #[test]
    fn the_ciphertext_does_not_contain_the_plaintext() {
        // The property the whole module exists for, stated as bluntly as it can be.
        let plaintext = b"the quarterly figures, which are not for sharing widely".repeat(200);
        let key = ContentKey::generate();
        let mut sealed = Vec::new();
        seal(&key, &nonce_prefix(), plaintext.as_slice(), &mut sealed).unwrap();
        assert!(sealed.windows(plaintext.len()).all(|w| w != plaintext.as_slice()));
        assert!(!sealed.windows(16).any(|w| w == &b"the quarterly fi"[..]));
    }

    #[test]
    fn two_seals_of_the_same_bytes_share_nothing() {
        // Why a SHARED object does not deduplicate, demonstrated rather than asserted in a
        // comment: the same file shared twice is two unrelated objects.
        let plaintext = bytes(4, 100_000);
        let key = ContentKey::generate();
        let mut first = Vec::new();
        let mut second = Vec::new();
        seal(&key, &nonce_prefix(), plaintext.as_slice(), &mut first).unwrap();
        seal(&key, &nonce_prefix(), plaintext.as_slice(), &mut second).unwrap();
        assert_ne!(first, second, "the same key with two nonces must not repeat");
    }

    #[test]
    fn the_wrong_key_does_not_open_it() {
        let plaintext = bytes(5, 50_000);
        let prefix = nonce_prefix();
        let mut sealed = Vec::new();
        seal(
            &ContentKey::generate(),
            &prefix,
            plaintext.as_slice(),
            &mut sealed,
        )
        .unwrap();

        let mut out = Vec::new();
        let err = open(&ContentKey::generate(), &prefix, sealed.as_slice(), &mut out).unwrap_err();
        assert!(matches!(err, SharedError::Undecryptable), "{err}");
    }

    #[test]
    fn the_wrong_nonce_prefix_does_not_open_it() {
        let plaintext = bytes(6, 50_000);
        let key = ContentKey::generate();
        let mut sealed = Vec::new();
        seal(&key, &nonce_prefix(), plaintext.as_slice(), &mut sealed).unwrap();

        let mut out = Vec::new();
        let err = open(&key, &nonce_prefix(), sealed.as_slice(), &mut out).unwrap_err();
        assert!(matches!(err, SharedError::Undecryptable), "{err}");
    }

    #[test]
    fn a_truncated_object_fails_rather_than_opening_short() {
        // The reason for STREAM rather than independently sealed frames. Cutting the tail
        // off an object must not yield a valid, shorter object: a reader that accepted one
        // would hand a caller a document with its last page silently removed.
        let plaintext = bytes(7, FRAME_BYTES * 3);
        let key = ContentKey::generate();
        let prefix = nonce_prefix();
        let mut sealed = Vec::new();
        seal(&key, &prefix, plaintext.as_slice(), &mut sealed).unwrap();

        let one_frame_short = &sealed[..FRAME_BYTES + 16];
        let mut out = Vec::new();
        let err = open(&key, &prefix, one_frame_short, &mut out).unwrap_err();
        assert!(matches!(err, SharedError::Undecryptable), "{err}");
    }

    #[test]
    fn reordered_frames_fail_rather_than_opening_scrambled() {
        let plaintext = bytes(8, FRAME_BYTES * 2 + 10);
        let key = ContentKey::generate();
        let prefix = nonce_prefix();
        let mut sealed = Vec::new();
        seal(&key, &prefix, plaintext.as_slice(), &mut sealed).unwrap();

        let frame = FRAME_BYTES + 16;
        let mut swapped = Vec::new();
        swapped.extend_from_slice(&sealed[frame..frame * 2]);
        swapped.extend_from_slice(&sealed[..frame]);
        swapped.extend_from_slice(&sealed[frame * 2..]);

        let mut out = Vec::new();
        let err = open(&key, &prefix, swapped.as_slice(), &mut out).unwrap_err();
        assert!(matches!(err, SharedError::Undecryptable), "{err}");
    }

    #[test]
    fn a_flipped_bit_anywhere_fails() {
        let plaintext = bytes(9, 200_000);
        let key = ContentKey::generate();
        let prefix = nonce_prefix();
        let mut sealed = Vec::new();
        seal(&key, &prefix, plaintext.as_slice(), &mut sealed).unwrap();

        for at in [0, 1, sealed.len() / 2, sealed.len() - 1] {
            let mut damaged = sealed.clone();
            damaged[at] ^= 0x01;
            let mut out = Vec::new();
            let err = open(&key, &prefix, damaged.as_slice(), &mut out).unwrap_err();
            assert!(matches!(err, SharedError::Undecryptable), "byte {at}: {err}");
        }
    }

    #[test]
    fn a_reader_that_returns_short_reads_is_handled() {
        // A pipe, a socket, or a decompressor will hand back less than asked for. Treating
        // a short read as end-of-input would seal a truncated object and call it whole.
        struct Dribble<'a>(&'a [u8]);
        impl std::io::Read for Dribble<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.0.len().min(buf.len()).min(7);
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let plaintext = bytes(10, FRAME_BYTES + 5000);
        let key = ContentKey::generate();
        let prefix = nonce_prefix();
        let mut sealed = Vec::new();
        seal(&key, &prefix, Dribble(&plaintext), &mut sealed).unwrap();
        assert_eq!(sealed.len() as u64, sealed_len(plaintext.len() as u64));

        let mut out = Vec::new();
        open(&key, &prefix, Dribble(&sealed), &mut out).unwrap();
        assert_eq!(out, plaintext);
    }

    #[test]
    fn a_content_key_never_prints_itself() {
        let key = ContentKey::from_bytes([7u8; 32]);
        let shown = format!("{key:?}");
        assert!(!shown.contains('7'), "{shown}");
        assert!(shown.contains("redacted"), "{shown}");
    }

    #[test]
    fn a_nonce_prefix_of_the_wrong_length_is_refused() {
        assert!(matches!(
            decode_prefix(&[0u8; 12]),
            Err(SharedError::BadNonce { len: 12 })
        ));
        assert!(decode_prefix(&[0u8; NONCE_PREFIX_BYTES]).is_ok());
    }

    #[test]
    fn sealed_len_matches_the_frame_arithmetic() {
        assert_eq!(sealed_len(0), 16);
        assert_eq!(sealed_len(1), 17);
        assert_eq!(sealed_len(FRAME_BYTES as u64), FRAME_BYTES as u64 + 16);
        assert_eq!(sealed_len(FRAME_BYTES as u64 + 1), FRAME_BYTES as u64 + 1 + 32);
    }
}

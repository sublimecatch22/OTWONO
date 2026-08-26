//! Wallet keys: a BIP-39 mnemonic, BIP-32/44 derivation on secp256k1, and a
//! passphrase-encrypted seed vault.
//!
//! **STATUS: IMPLEMENTED** — unit tested; not yet driven by a daemon and not booted.
//!
//! This crate holds no policy and opens no socket. It is the key material and the file
//! format, so it can be tested without a control plane and without root, which is
//! CLAUDE.md §2.4's requirement of every subsystem.
//!
//! Three things ADR-0022 settled that this crate exists to honour, and that a later reader
//! should not have to reconstruct:
//!
//! 1. **The wallet key is not the node key and is not derived from it.** Losing a machine
//!    must not lose money; one household may run several nodes against one wallet; and
//!    `id.rotate` changes the NodeID, so a key that must never change cannot come from one
//!    that does.
//! 2. **Twenty-four words, not twelve.** The entropy is free and the words are written down
//!    once. `bip32::Mnemonic` takes a fixed 32-byte entropy, so 24 is the only length this
//!    crate can express — which is the right constraint arriving for the wrong reason, and
//!    is pinned by a test so a later dependency bump cannot quietly widen it.
//! 3. **Signing is not here.** `wallet.sign` is `always_confirm`, no confirmation channel
//!    exists until Phase 7, and ADR-0022 accepted that the signing path is unusable until
//!    then *by construction*. The vault, the derivation and the public side can all be built
//!    and tested first, which is the order this crate is built in.

#![forbid(unsafe_code)]

pub mod address;
mod derive;
mod vault;

pub use address::{encode as encode_address, AddressError, Family};
pub use derive::{Account, AccountPath, DeriveError};
pub use vault::{Vault, VaultError, VaultFile, ARGON2_M_COST, ARGON2_P_COST, ARGON2_T_COST};

use zeroize::Zeroizing;

/// The number of words this wallet writes and accepts. ADR-0022 §1.
pub const MNEMONIC_WORDS: usize = 24;

/// Errors from creating or reading a mnemonic.
#[derive(Debug)]
pub enum MnemonicError {
    /// The phrase did not have exactly [`MNEMONIC_WORDS`] words.
    WrongLength(usize),
    /// The phrase parsed as words but its BIP-39 checksum did not hold.
    ///
    /// Almost always a typo or a transposition rather than an attack, and the message says
    /// so: a person recovering a wallet at this point is already having a bad day.
    BadChecksum,
}

impl std::fmt::Display for MnemonicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MnemonicError::WrongLength(n) => {
                write!(f, "a recovery phrase is {MNEMONIC_WORDS} words; this one has {n}")
            }
            MnemonicError::BadChecksum => write!(
                f,
                "those {MNEMONIC_WORDS} words do not check out; a word is misspelled, \
                 swapped with its neighbour, or in the wrong place"
            ),
        }
    }
}

impl std::error::Error for MnemonicError {}

/// A 24-word BIP-39 recovery phrase, and the seed it produces.
///
/// The phrase is the backup. It is deliberately the *only* backup this crate offers: the
/// standard one, that people already have tooling and habits and metal plates for.
pub struct Mnemonic(bip32::Mnemonic);

impl Mnemonic {
    /// A fresh phrase from the operating system's randomness.
    pub fn generate() -> Mnemonic {
        Mnemonic(bip32::Mnemonic::random(
            rand_core::OsRng,
            bip32::Language::English,
        ))
    }

    /// Read a phrase somebody typed back in.
    ///
    /// Whitespace is normalised before parsing, because a phrase written on paper and typed
    /// back will have inconsistent spacing and that is not the user's mistake to pay for.
    /// Case is not normalised: BIP-39 words are lowercase, and a phrase in another case is
    /// more likely to be from another wallet's word list than a shift key.
    pub fn parse(phrase: &str) -> Result<Mnemonic, MnemonicError> {
        let normalised = Zeroizing::new(phrase.split_whitespace().collect::<Vec<_>>().join(" "));
        let words = normalised.split_whitespace().count();
        if words != MNEMONIC_WORDS {
            return Err(MnemonicError::WrongLength(words));
        }
        bip32::Mnemonic::new(normalised.as_str(), bip32::Language::English)
            .map(Mnemonic)
            .map_err(|_| MnemonicError::BadChecksum)
    }

    /// The words, to show once and never again.
    pub fn phrase(&self) -> &str {
        self.0.phrase()
    }

    /// The 64-byte BIP-39 seed.
    ///
    /// `passphrase` is BIP-39's optional twenty-fifth word. An empty string is the
    /// overwhelmingly common case and is what other wallets will assume; a non-empty one is
    /// a second secret that is **not** recoverable from the words alone, and a UI offering
    /// it must say that in those terms.
    pub fn seed(&self, passphrase: &str) -> Zeroizing<[u8; 64]> {
        Zeroizing::new(*self.0.to_seed(passphrase).as_bytes())
    }
}

impl std::fmt::Debug for Mnemonic {
    /// Never the words. A recovery phrase in a log file is the whole wallet in a log file,
    /// and `{:?}` reaches log files by accident more than any other formatting.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Mnemonic(24 words, redacted)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_phrase_is_twenty_four_words() {
        // ADR-0022 chose 24 over the common 12. The dependency happens to permit only 24;
        // this pins the requirement so a later bump that widened it would fail here rather
        // than silently start writing shorter phrases.
        let m = Mnemonic::generate();
        assert_eq!(m.phrase().split_whitespace().count(), MNEMONIC_WORDS);
    }

    #[test]
    fn a_phrase_round_trips_through_text() {
        let m = Mnemonic::generate();
        let back = Mnemonic::parse(m.phrase()).expect("our own phrase must parse");
        assert_eq!(back.phrase(), m.phrase());
        assert_eq!(&back.seed("")[..], &m.seed("")[..]);
    }

    #[test]
    fn ragged_whitespace_is_the_users_paper_not_their_mistake() {
        let m = Mnemonic::generate();
        let ragged = format!("  {}  ", m.phrase().replace(' ', "\n  "));
        assert_eq!(Mnemonic::parse(&ragged).unwrap().phrase(), m.phrase());
    }

    #[test]
    fn a_short_phrase_is_refused_by_length_before_checksum() {
        // The distinction matters to whoever is retyping: "you are missing words" is a
        // different problem from "one of these words is wrong".
        let m = Mnemonic::generate();
        let twelve: Vec<&str> = m.phrase().split_whitespace().take(12).collect();
        match Mnemonic::parse(&twelve.join(" ")) {
            Err(MnemonicError::WrongLength(12)) => {}
            other => panic!("expected a length error, got {other:?}"),
        }
    }

    #[test]
    fn a_transposition_is_caught_by_the_checksum() {
        // The failure BIP-39's checksum exists for, and the one most likely to happen off a
        // sheet of paper. Swapping two adjacent words keeps every word valid.
        let m = Mnemonic::generate();
        let mut words: Vec<&str> = m.phrase().split_whitespace().collect();
        words.swap(3, 4);
        if words[3] == words[4] {
            return; // a repeated word makes the swap a no-op; nothing to assert
        }
        match Mnemonic::parse(&words.join(" ")) {
            Err(MnemonicError::BadChecksum) => {}
            other => panic!("expected a checksum error, got {other:?}"),
        }
    }

    #[test]
    fn the_bip39_passphrase_changes_the_seed() {
        // The twenty-fifth word is a second secret, not a flourish: the same words with a
        // passphrase are a different wallet, and nothing in the phrase reveals that.
        let m = Mnemonic::generate();
        assert_ne!(&m.seed("")[..], &m.seed("correct horse")[..]);
    }

    #[test]
    fn debug_never_prints_the_words() {
        // Exact match, not per-word substring. Asserting that no individual recovery word
        // appears in the output looks stricter and is in fact flaky: "redacted" contains
        // "act" and "words" contains "word", both BIP-39 words, so a random phrase trips it
        // about one run in forty. Pinning the whole string is deterministic and says
        // exactly what this type is allowed to reveal.
        let m = Mnemonic::generate();
        let shown = format!("{m:?}");
        assert_eq!(shown, "Mnemonic(24 words, redacted)");
        assert!(!shown.contains(m.phrase()));
    }
}

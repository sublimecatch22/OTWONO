//! BIP-32 derivation on secp256k1, addressed by BIP-44 paths.
//!
//! ADR-0022 §5: hierarchical derivation makes addresses free, so there is no excuse for
//! reusing one. **A fresh address per counterparty and per purpose** is the default, because
//! reusing a single address for every reward payment makes a household's whole contribution
//! history — how much it contributes, when it is online, who it transacts with —
//! permanently and publicly linkable to anyone who ever learns one address.
//!
//! What is deliberately absent: address *encoding*. ADR-0022 leaves the chain undecided, and
//! an address string is chain-specific — Ethereum hashes the public key with Keccak, Bitcoin
//! hashes it differently again and encodes with a network prefix. Committing to one here
//! would be deciding the chain by implementation, which is how a "not yet decided" quietly
//! becomes decided. This module goes as far as the public key, which every chain in the
//! secp256k1 family agrees on, and stops.

use bip32::{DerivationPath, PublicKey, XPrv};
use std::str::FromStr;
use zeroize::Zeroizing;

/// BIP-44's `purpose` field. 44 is the standard, and this crate writes no other.
const PURPOSE: u32 = 44;

#[derive(Debug)]
pub enum DeriveError {
    /// A coin type, account, or index outside BIP-32's non-hardened range.
    OutOfRange(&'static str, u32),
    /// The underlying derivation refused, which at this point means bad seed material.
    Bip32(String),
}

impl std::fmt::Display for DeriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeriveError::OutOfRange(what, n) => {
                write!(f, "{what} {n} is outside the range BIP-32 allows (0..2^31)")
            }
            DeriveError::Bip32(m) => write!(f, "derivation failed: {m}"),
        }
    }
}

impl std::error::Error for DeriveError {}

/// A BIP-44 path: `m/44'/coin'/account'/change/index`.
///
/// Held as fields rather than a string so a caller cannot construct a path this wallet does
/// not mean — in particular cannot silently drop a hardened marker, which would put an
/// account's private key within reach of anyone holding its extended public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountPath {
    coin: u32,
    account: u32,
    change: u32,
    index: u32,
}

impl AccountPath {
    /// The receiving chain (`change = 0`) of an account.
    pub fn receiving(coin: u32, account: u32, index: u32) -> Result<AccountPath, DeriveError> {
        Self::new(coin, account, 0, index)
    }

    pub fn new(coin: u32, account: u32, change: u32, index: u32) -> Result<AccountPath, DeriveError> {
        // The three hardened levels must fit below 2^31, because the hardened offset is
        // added to them. change and index are not hardened and use the whole range.
        for (what, n) in [("coin type", coin), ("account", account)] {
            if n >= 0x8000_0000 {
                return Err(DeriveError::OutOfRange(what, n));
            }
        }
        if change > 1 {
            return Err(DeriveError::OutOfRange("change", change));
        }
        if index >= 0x8000_0000 {
            return Err(DeriveError::OutOfRange("index", index));
        }
        Ok(AccountPath {
            coin,
            account,
            change,
            index,
        })
    }

    pub fn coin(&self) -> u32 {
        self.coin
    }

    pub fn account(&self) -> u32 {
        self.account
    }

    pub fn change(&self) -> u32 {
        self.change
    }

    pub fn index(&self) -> u32 {
        self.index
    }
}

impl std::fmt::Display for AccountPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "m/{PURPOSE}'/{}'/{}'/{}/{}",
            self.coin, self.account, self.change, self.index
        )
    }
}

/// One derived key, and the public half a caller may safely be handed.
pub struct Account {
    path: AccountPath,
    key: Zeroizing<[u8; 32]>,
    public: [u8; 33],
}

impl Account {
    /// Derive the key at `path` from a BIP-39 seed.
    pub fn derive(seed: &[u8; 64], path: AccountPath) -> Result<Account, DeriveError> {
        let text = path.to_string();
        let parsed = DerivationPath::from_str(&text).map_err(|e| DeriveError::Bip32(e.to_string()))?;
        let xprv = XPrv::derive_from_path(seed, &parsed).map_err(|e| DeriveError::Bip32(e.to_string()))?;
        let public = xprv.public_key().public_key().to_bytes();
        Ok(Account {
            path,
            key: Zeroizing::new(xprv.to_bytes()),
            public,
        })
    }

    pub fn path(&self) -> AccountPath {
        self.path
    }

    /// The compressed secp256k1 public key, 33 bytes.
    ///
    /// Every chain in this family derives its address from these bytes; how it does so is
    /// the chain's business and not this crate's (see the module note).
    pub fn public_key(&self) -> &[u8; 33] {
        &self.public
    }

    /// The public key as hex, for display and for handing to whatever encodes an address.
    pub fn public_key_hex(&self) -> String {
        data_encoding::HEXLOWER.encode(&self.public)
    }

    /// The private key.
    ///
    /// Deliberately awkward to reach and deliberately not `Clone`: nothing outside a signing
    /// path has any business with it, and signing is not built (ADR-0022 §3 — `wallet.sign`
    /// is `always_confirm`, and ADR-0024's channel refuses an approval from the uid that
    /// asked — which on a single-uid image is every approval).
    pub fn private_key_bytes(&self) -> &Zeroizing<[u8; 32]> {
        &self.key
    }
}

impl std::fmt::Debug for Account {
    /// The path and the public key. Never the private key: `{:?}` reaches logs by accident
    /// more than any other formatting, and this one would put a spendable key there.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("path", &self.path.to_string())
            .field("public_key", &self.public_key_hex())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mnemonic;

    /// The BIP-39 test vector everybody uses, with the standard "TREZOR" passphrase.
    ///
    /// Checking against the published vector rather than against ourselves is the point:
    /// a wallet that is self-consistent and disagrees with the standard produces a seed
    /// phrase no other wallet can restore, which is a way to lose money that looks like
    /// working software.
    const VECTOR_PHRASE: &str = "legal winner thank year wave sausage worth useful legal \
                                 winner thank year wave sausage worth useful legal will";
    const VECTOR_24: &str = "letter advice cage absurd amount doctor acoustic avoid letter \
                             advice cage absurd amount doctor acoustic avoid letter advice \
                             cage absurd amount doctor acoustic bless";
    const VECTOR_24_SEED: &str = "c0c519bd0e91a2ed54357d9d1ebef6f5af218a153624cf4f2da911a0ed8f7a09\
                                  e2ef61af0aca007096df430022f7a2b6fb91661a9589097069720d015e4e982f";

    #[test]
    fn the_bip39_vector_produces_the_published_seed() {
        let m = Mnemonic::parse(VECTOR_24).expect("the published vector must parse");
        let seed = m.seed("TREZOR");
        assert_eq!(data_encoding::HEXLOWER.encode(&seed[..]), VECTOR_24_SEED);
    }

    #[test]
    fn an_eighteen_word_vector_is_refused_because_this_wallet_writes_twenty_four() {
        // Not a defect in the vector: a deliberate narrowing (ADR-0022 §1). It is here so
        // the refusal is visibly a choice rather than an accident of the dependency.
        assert!(Mnemonic::parse(VECTOR_PHRASE).is_err());
    }

    /// BIP-32 test vector 1: the seed `000102...0e0f` and its `m/0'` child.
    const BIP32_SEED: &str = "000102030405060708090a0b0c0d0e0f";
    const BIP32_M0H_PUB: &str = "035a784662a4a20a65bf6aab9ae98a6c068a81c52e4b032c0fb5400c706cfccc56";

    #[test]
    fn the_bip32_vector_derives_the_published_public_key() {
        let raw = data_encoding::HEXLOWER.decode(BIP32_SEED.as_bytes()).unwrap();
        let path = DerivationPath::from_str("m/0'").unwrap();
        let xprv = XPrv::derive_from_path(&raw, &path).unwrap();
        assert_eq!(
            data_encoding::HEXLOWER.encode(&xprv.public_key().public_key().to_bytes()),
            BIP32_M0H_PUB
        );
    }

    fn a_seed() -> [u8; 64] {
        Mnemonic::parse(VECTOR_24).unwrap().seed("")[..]
            .try_into()
            .unwrap()
    }

    #[test]
    fn a_path_renders_as_bip44_with_the_hardened_levels_marked() {
        let p = AccountPath::receiving(60, 0, 0).unwrap();
        assert_eq!(p.to_string(), "m/44'/60'/0'/0/0");
        assert_eq!(
            AccountPath::new(0, 2, 1, 7).unwrap().to_string(),
            "m/44'/0'/2'/1/7"
        );
    }

    #[test]
    fn every_index_is_a_different_key() {
        // ADR-0022 §5's whole premise: addresses are free, so reuse is never forced by the
        // wallet. If two indices collided the "fresh address per purpose" default would be
        // a lie told by a UI over a wallet that cannot deliver it.
        let seed = a_seed();
        let mut seen = std::collections::HashSet::new();
        for i in 0..8 {
            let a = Account::derive(&seed, AccountPath::receiving(60, 0, i).unwrap()).unwrap();
            assert!(seen.insert(a.public_key_hex()), "index {i} repeated a key");
        }
        assert_eq!(seen.len(), 8);
    }

    #[test]
    fn accounts_and_coins_and_change_chains_are_all_separate() {
        let seed = a_seed();
        let key = |p: AccountPath| Account::derive(&seed, p).unwrap().public_key_hex();
        let base = key(AccountPath::new(60, 0, 0, 0).unwrap());
        assert_ne!(base, key(AccountPath::new(61, 0, 0, 0).unwrap()), "coin type");
        assert_ne!(base, key(AccountPath::new(60, 1, 0, 0).unwrap()), "account");
        assert_ne!(base, key(AccountPath::new(60, 0, 1, 0).unwrap()), "change");
    }

    #[test]
    fn derivation_is_deterministic_from_the_words_alone() {
        // What "the user owns the keys" means in practice: the phrase, and nothing held by
        // this machine, reproduces the key. If this ever stopped holding, a recovery on a
        // new machine would silently produce a different wallet.
        let a = Account::derive(&a_seed(), AccountPath::receiving(60, 0, 3).unwrap()).unwrap();
        let b = Account::derive(&a_seed(), AccountPath::receiving(60, 0, 3).unwrap()).unwrap();
        assert_eq!(a.public_key_hex(), b.public_key_hex());
        assert_eq!(a.private_key_bytes()[..], b.private_key_bytes()[..]);
    }

    #[test]
    fn the_bip39_passphrase_produces_a_different_wallet_entirely() {
        let plain: [u8; 64] = Mnemonic::parse(VECTOR_24).unwrap().seed("")[..]
            .try_into()
            .unwrap();
        let with: [u8; 64] = Mnemonic::parse(VECTOR_24).unwrap().seed("x")[..]
            .try_into()
            .unwrap();
        let p = AccountPath::receiving(60, 0, 0).unwrap();
        assert_ne!(
            Account::derive(&plain, p).unwrap().public_key_hex(),
            Account::derive(&with, p).unwrap().public_key_hex()
        );
    }

    #[test]
    fn out_of_range_levels_are_refused_rather_than_wrapped() {
        // Wrapping would silently derive a *different* path than the caller named, which is
        // an address a person could be told to send money to and nobody could spend from.
        assert!(AccountPath::new(0x8000_0000, 0, 0, 0).is_err());
        assert!(AccountPath::new(0, 0x8000_0000, 0, 0).is_err());
        assert!(AccountPath::new(0, 0, 2, 0).is_err());
        assert!(AccountPath::new(0, 0, 0, 0x8000_0000).is_err());
    }

    #[test]
    fn debug_never_prints_the_private_key() {
        let a = Account::derive(&a_seed(), AccountPath::receiving(60, 0, 0).unwrap()).unwrap();
        let shown = format!("{a:?}");
        let secret = data_encoding::HEXLOWER.encode(&a.private_key_bytes()[..]);
        assert!(!shown.contains(&secret), "{shown} leaked the private key");
        assert!(shown.contains(&a.public_key_hex()));
    }
}

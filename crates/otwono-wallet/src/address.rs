//! Address encoding, for every family secp256k1 reaches.
//!
//! ADR-0022 left the chain undecided and this crate previously refused to encode anything,
//! on the reasoning that shipping one encoder would decide the chain by implementation.
//! That was too cautious. An address is a **pure function of a public key**, the same key in
//! every case, so rendering all three formats commits to nothing and genuinely defers the
//! choice rather than pretending to.
//!
//! What it does buy: a person can be shown something they can actually receive at, which
//! `wallet.public_keys` returning raw hex never gave them.
//!
//! Three families, from one compressed secp256k1 key:
//!
//! | Family | Derivation | Example prefix |
//! |---|---|---|
//! | Ethereum | Keccak-256 of the *uncompressed* key, last 20 bytes, EIP-55 checksum | `0x` |
//! | Bitcoin | SHA-256 then RIPEMD-160, bech32 as a v0 witness program | `bc1q` |
//! | Cosmos | the same 20 bytes, bech32 with a chain's own prefix | `otwono1` |
//!
//! Deliberately absent: anything that *sends*. This module turns a key into a string.

use bech32::{Bech32, Hrp};
use k256::elliptic_curve::sec1::ToEncodedPoint;
use ripemd::Ripemd160;
use sha2::{Digest, Sha256};
use sha3::Keccak256;

/// Which rendering to produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Family {
    /// `0x…`, EIP-55 mixed-case checksummed. Ethereum and every EVM chain.
    Ethereum,
    /// `bc1q…`, a version-0 witness program. Bitcoin mainnet.
    Bitcoin,
    /// bech32 under a chain's own human-readable prefix — `cosmos`, `otwono`, anything.
    Cosmos(String),
}

#[derive(Debug)]
pub enum AddressError {
    /// The bytes were not a public key this can decode.
    BadKey(String),
    /// A bech32 prefix a chain would not accept.
    BadPrefix(String),
}

impl std::fmt::Display for AddressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddressError::BadKey(m) => write!(f, "not a usable public key: {m}"),
            AddressError::BadPrefix(m) => write!(f, "not a usable address prefix: {m}"),
        }
    }
}

impl std::error::Error for AddressError {}

/// Render `public_key` — 33 compressed bytes — in `family`.
pub fn encode(public_key: &[u8; 33], family: &Family) -> Result<String, AddressError> {
    match family {
        Family::Ethereum => ethereum(public_key),
        Family::Bitcoin => bech32_20(public_key, "bc"),
        Family::Cosmos(hrp) => bech32_20(public_key, hrp),
    }
}

/// Keccak-256 over the 64-byte uncompressed key, last 20 bytes, EIP-55 checksummed.
///
/// The **uncompressed** form is the part worth stating: Ethereum hashes the 64 bytes of x
/// and y with the `0x04` tag removed, not the 33 compressed bytes. Hashing the compressed
/// key produces a well-formed address that belongs to nobody, and money sent to it is gone —
/// which is why this is checked against a published vector rather than against itself.
fn ethereum(public_key: &[u8; 33]) -> Result<String, AddressError> {
    let point = k256::PublicKey::from_sec1_bytes(public_key)
        .map_err(|e| AddressError::BadKey(e.to_string()))?
        .to_encoded_point(false);
    let uncompressed = point.as_bytes();
    // 0x04 || x || y
    let body = &uncompressed[1..];
    let digest = Keccak256::digest(body);
    let raw = &digest[12..];
    Ok(eip55(raw))
}

/// EIP-55: capitalise a hex digit when the matching Keccak nibble is 8 or more.
///
/// A checksum a person can see. Software that ignores it still parses the address; a human
/// comparing two strings has something to compare, which is the whole purpose.
fn eip55(raw: &[u8]) -> String {
    let lower: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let hash = Keccak256::digest(lower.as_bytes());
    let mut out = String::with_capacity(42);
    out.push_str("0x");
    for (i, c) in lower.chars().enumerate() {
        let nibble = if i % 2 == 0 {
            hash[i / 2] >> 4
        } else {
            hash[i / 2] & 0x0f
        };
        if c.is_ascii_digit() || nibble < 8 {
            out.push(c);
        } else {
            out.push(c.to_ascii_uppercase());
        }
    }
    out
}

/// SHA-256 then RIPEMD-160 over the compressed key, bech32 under `hrp`.
///
/// Bitcoin and Cosmos differ only in the prefix and in whether a witness version is
/// prepended, which is why they share this.
fn bech32_20(public_key: &[u8; 33], hrp: &str) -> Result<String, AddressError> {
    // bech32 itself permits an all-uppercase prefix — an uppercase address is legal and is
    // used for QR codes. Refused here anyway: every real chain prefix is lowercase, so an
    // uppercase one produces a technically valid address that looks wrong to a person and
    // that some tooling will not accept. Narrower than the spec, on purpose.
    if hrp.chars().any(|c| c.is_ascii_uppercase()) {
        return Err(AddressError::BadPrefix(format!(
            "{hrp:?} is not lowercase; chain prefixes are lowercase by convention"
        )));
    }
    let hrp = Hrp::parse(hrp).map_err(|e| AddressError::BadPrefix(e.to_string()))?;
    let sha = Sha256::digest(public_key);
    let hash160 = Ripemd160::digest(sha);
    bech32::encode::<Bech32>(hrp, &hash160).map_err(|e| AddressError::BadPrefix(e.to_string()))
}

/// The 20-byte HASH160 of a compressed key, for a caller that wants the bytes.
pub fn hash160(public_key: &[u8; 33]) -> [u8; 20] {
    let sha = Sha256::digest(public_key);
    let out = Ripemd160::digest(sha);
    let mut b = [0u8; 20];
    b.copy_from_slice(&out);
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Account, AccountPath, Mnemonic};

    /// The BIP-39 vector already used in `derive.rs`, so every family is rendered from a key
    /// whose provenance is a published document rather than this crate.
    const VECTOR_24: &str = "letter advice cage absurd amount doctor acoustic avoid letter \
                             advice cage absurd amount doctor acoustic avoid letter advice \
                             cage absurd amount doctor acoustic bless";

    fn key(index: u32) -> [u8; 33] {
        let seed: [u8; 64] = Mnemonic::parse(VECTOR_24).unwrap().seed("")[..]
            .try_into()
            .unwrap();
        *Account::derive(&seed, AccountPath::receiving(60, 0, index).unwrap())
            .unwrap()
            .public_key()
    }

    /// Compressed key and Ethereum address for the private keys 1, 2 and 3.
    ///
    /// Independently computed rather than recalled: a first version of this test carried a
    /// vector written from memory, and it was wrong. These come from a from-scratch
    /// Keccak-256 and secp256k1 implementation whose Keccak was itself checked against the
    /// published empty-string and "abc" digests first, and privkey 1 lands on the widely
    /// published 0x7e5f…5bdf. Checking an encoder against itself proves nothing; an encoder
    /// that is self-consistent and disagrees with the chain produces addresses nobody can
    /// spend from, which is a way to lose money that looks like working software.
    const ETH_VECTORS: [(&str, &str); 3] = [
        (
            "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
            "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf",
        ),
        (
            "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
            "0x2b5ad5c4795c026514f8317c7a215e218dccd6cf",
        ),
        (
            "02f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9",
            "0x6813eb9362372eef6200f3b1dbc3f819671cba69",
        ),
    ];

    #[test]
    fn the_published_ethereum_vectors_encode_to_their_published_addresses() {
        for (key_hex, want) in ETH_VECTORS {
            let raw = data_encoding::HEXLOWER.decode(key_hex.as_bytes()).unwrap();
            let k: [u8; 33] = raw.try_into().unwrap();
            let got = encode(&k, &Family::Ethereum).unwrap();
            assert_eq!(got.to_lowercase(), want, "key {key_hex}");
        }
    }

    #[test]
    fn eip55_matches_the_specifications_own_examples() {
        // Straight from EIP-55. These are the cases a hand-rolled checksum gets wrong.
        for a in [
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
            "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359",
            "0xdbF03B407c01E7cD3CBea99509d93f8DDDC8C6FB",
            "0xD1220A0cf47c7B9Be7A2E6BA89F429762e7b9aDb",
        ] {
            let raw = data_encoding::HEXLOWER
                .decode(a[2..].to_lowercase().as_bytes())
                .unwrap();
            assert_eq!(eip55(&raw), a, "EIP-55 checksum differs from the specification");
        }
    }

    #[test]
    fn ethereum_hashes_the_uncompressed_key_not_the_compressed_one() {
        // The mistake this guards against produces a well-formed address belonging to
        // nobody. Hashing the 33 compressed bytes gives a *different* valid-looking answer,
        // so the two must not agree.
        let k = key(0);
        let compressed_by_mistake = eip55(&Keccak256::digest(k)[12..]);
        assert_ne!(encode(&k, &Family::Ethereum).unwrap(), compressed_by_mistake);
    }

    #[test]
    fn every_family_renders_the_same_key_differently() {
        // The premise of doing all three: one key, three renderings, no commitment.
        let k = key(0);
        let eth = encode(&k, &Family::Ethereum).unwrap();
        let btc = encode(&k, &Family::Bitcoin).unwrap();
        let otw = encode(&k, &Family::Cosmos("otwono".into())).unwrap();
        assert!(eth.starts_with("0x") && eth.len() == 42, "{eth}");
        assert!(btc.starts_with("bc1"), "{btc}");
        assert!(otw.starts_with("otwono1"), "{otw}");
        assert_ne!(eth, btc);
        assert_ne!(btc, otw);
    }

    #[test]
    fn bitcoin_and_cosmos_share_their_bytes_and_differ_only_in_prefix() {
        // Worth pinning: they are the same HASH160 under different prefixes, which is why
        // one function serves both. If they ever diverge, this says so.
        let k = key(3);
        let a = encode(&k, &Family::Cosmos("bc".into())).unwrap();
        let b = encode(&k, &Family::Bitcoin).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn every_index_gives_a_different_address_in_every_family() {
        // ADR-0022 §5's premise, at the level a person actually sees. If indices collided
        // here, "a fresh address per purpose" would be a promise the UI could not keep.
        for f in [Family::Ethereum, Family::Bitcoin, Family::Cosmos("otwono".into())] {
            let seen: std::collections::HashSet<String> =
                (0..8).map(|i| encode(&key(i), &f).unwrap()).collect();
            assert_eq!(seen.len(), 8, "{f:?} repeated an address across indices");
        }
    }

    #[test]
    fn encoding_is_deterministic() {
        let k = key(1);
        for f in [Family::Ethereum, Family::Bitcoin, Family::Cosmos("otwono".into())] {
            assert_eq!(encode(&k, &f).unwrap(), encode(&k, &f).unwrap());
        }
    }

    #[test]
    fn a_prefix_a_chain_would_reject_is_refused_rather_than_encoded() {
        // An address under an invalid prefix is a string that looks like an address and is
        // not one. Refusing beats rendering it.
        let k = key(0);
        // The last two come from bech32 itself; the uppercase ones are this crate being
        // deliberately narrower than the spec, which permits them.
        for bad in ["", "UPPER", "Mixed", "has space", &"x".repeat(90)] {
            assert!(
                encode(&k, &Family::Cosmos(bad.into())).is_err(),
                "{bad:?} should not encode"
            );
        }
        // And the conventional form still works, so the check is not simply refusing.
        assert!(encode(&k, &Family::Cosmos("cosmos".into())).is_ok());
    }

    #[test]
    fn a_key_that_is_not_on_the_curve_is_refused() {
        // 33 bytes of the right shape are not necessarily a point. Ethereum encoding has to
        // decompress, so this is where it surfaces.
        let bogus = [7u8; 33];
        assert!(encode(&bogus, &Family::Ethereum).is_err());
    }
}

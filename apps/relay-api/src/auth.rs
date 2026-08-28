//! Passwords, tokens and rate limiting for the relay.
//!
//! Passwords are hashed with Argon2id. Tokens are random, stored only as a
//! SHA-256 hash, scoped, and revocable.

use anyhow::{anyhow, bail, Result};
use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::db::RelayDb;

/// Scopes a relay token may hold. A scope outside this list is refused, and
/// none of them reach prompts, files or knowledge.
pub const ALLOWED_SCOPES: &[&str] = &[
    "profile.read",
    "profile.write",
    "projects.read",
    "tasks.read",
    "marketplace.read",
    "marketplace.write",
];

pub fn hash_password(password: &str) -> Result<String> {
    if password.chars().count() < 12 {
        bail!("a password must be at least 12 characters long");
    }
    if password.chars().count() > 512 {
        bail!("a password must be 512 characters or fewer");
    }
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow!("could not hash the password: {e}"))?
        .to_string())
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// A fresh opaque token. Returned once; only its hash is stored.
pub fn mint_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn hash_token(token: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

/// Codes are short and unambiguous when read aloud.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

pub fn mint_pairing_code() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..8)
        .map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char)
        .collect()
}

pub fn hash_pairing_code(code: &str) -> String {
    hash_token(&code.trim().to_ascii_uppercase())
}

pub fn validate_scopes(scopes: &[String]) -> Result<()> {
    if scopes.is_empty() {
        bail!("at least one scope is required");
    }
    for scope in scopes {
        if !ALLOWED_SCOPES.contains(&scope.as_str()) {
            bail!("unknown scope {scope:?}");
        }
    }
    Ok(())
}

/// Fixed-window rate limiting. Returns false when the caller is over the limit.
pub fn check_rate_limit(
    db: &RelayDb,
    bucket: &str,
    limit: u32,
    window_seconds: i64,
) -> Result<bool> {
    let now = chrono::Utc::now().timestamp();
    let window_start = (now - (now % window_seconds)).to_string();
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO rate_limits (bucket, window_start, count) VALUES (?1, ?2, 1)
         ON CONFLICT(bucket, window_start) DO UPDATE SET count = count + 1",
        rusqlite::params![bucket, window_start],
    )?;
    let count: i64 = conn.query_row(
        "SELECT count FROM rate_limits WHERE bucket = ?1 AND window_start = ?2",
        rusqlite::params![bucket, window_start],
        |row| row.get(0),
    )?;
    conn.execute(
        "DELETE FROM rate_limits WHERE CAST(window_start AS INTEGER) < ?1",
        [now - window_seconds * 10],
    )?;
    Ok(count <= limit as i64)
}

/// Only the first two octets of an address are kept, so the audit log is
/// useful for spotting abuse without recording where someone lives.
pub fn ip_prefix(address: &str) -> String {
    let head = address.split(':').next().unwrap_or(address);
    let parts: Vec<&str> = head.split('.').collect();
    if parts.len() == 4 {
        format!("{}.{}.x.x", parts[0], parts[1])
    } else {
        "unknown".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_hashed_and_verified() {
        let hash = hash_password("a-long-enough-password").unwrap();
        assert!(hash.starts_with("$argon2"));
        assert!(verify_password("a-long-enough-password", &hash));
        assert!(!verify_password("the-wrong-password", &hash));
    }

    #[test]
    fn two_hashes_of_the_same_password_differ() {
        let first = hash_password("a-long-enough-password").unwrap();
        let second = hash_password("a-long-enough-password").unwrap();
        assert_ne!(first, second, "each hash must use its own salt");
    }

    #[test]
    fn short_passwords_are_refused_with_the_rule_stated() {
        let error = hash_password("short").unwrap_err().to_string();
        assert!(error.contains("at least 12 characters"));
    }

    #[test]
    fn a_corrupt_stored_hash_never_verifies() {
        assert!(!verify_password("anything", "not-a-hash"));
        assert!(!verify_password("anything", ""));
    }

    #[test]
    fn tokens_are_long_random_and_stored_only_as_a_hash() {
        let token = mint_token();
        assert!(token.len() >= 43);
        assert_ne!(hash_token(&token), token);
        assert_eq!(hash_token(&token), hash_token(&token));
        assert_ne!(hash_token(&token), hash_token(&mint_token()));
    }

    #[test]
    fn pairing_codes_avoid_characters_that_are_easy_to_confuse() {
        for _ in 0..50 {
            let code = mint_pairing_code();
            assert_eq!(code.len(), 8);
            for character in code.chars() {
                assert!(
                    !"OI01".contains(character),
                    "{code} contains an ambiguous character"
                );
            }
        }
    }

    #[test]
    fn pairing_codes_are_matched_case_insensitively() {
        let code = mint_pairing_code();
        assert_eq!(
            hash_pairing_code(&code),
            hash_pairing_code(&code.to_lowercase())
        );
        assert_eq!(
            hash_pairing_code(&code),
            hash_pairing_code(&format!("  {code} "))
        );
    }

    #[test]
    fn no_scope_reaches_prompts_files_or_knowledge() {
        for scope in ALLOWED_SCOPES {
            for segment in scope.split('.') {
                for forbidden in [
                    "knowledge",
                    "chat",
                    "message",
                    "file",
                    "model",
                    "conversation",
                ] {
                    assert_ne!(segment, forbidden, "{scope} would expose {forbidden}");
                }
            }
        }
        assert!(validate_scopes(&["profile.read".into()]).is_ok());
        assert!(validate_scopes(&["knowledge.read".into()]).is_err());
        assert!(validate_scopes(&[]).is_err());
    }

    #[test]
    fn rate_limits_apply_per_bucket() {
        let db = RelayDb::open_in_memory().unwrap();
        for _ in 0..3 {
            assert!(check_rate_limit(&db, "signin:a", 3, 3600).unwrap());
        }
        assert!(!check_rate_limit(&db, "signin:a", 3, 3600).unwrap());
        assert!(check_rate_limit(&db, "signin:b", 3, 3600).unwrap());
    }

    #[test]
    fn only_a_coarse_address_prefix_is_kept() {
        assert_eq!(ip_prefix("203.0.113.45:51234"), "203.0.x.x");
        assert_eq!(ip_prefix("203.0.113.45"), "203.0.x.x");
        assert_eq!(ip_prefix("::1"), "unknown");
    }
}

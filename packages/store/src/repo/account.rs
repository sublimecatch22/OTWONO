//! The optional link to an OTWONO relay account, and the pairing flow that
//! establishes it.
//!
//! Pairing codes are short-lived, single-use and stored as a hash, so reading
//! the database does not reveal a code that is still valid. The tokens they
//! exchange for live in the OS credential vault, not here.

use anyhow::{bail, Result};
use base64::Engine;
use rand::Rng;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use otwono_types::marketplace::WorkerProfile;

use crate::Db;

/// How long a pairing code stays valid. Short on purpose: the user is expected
/// to be at both screens.
pub const PAIRING_CODE_TTL_SECONDS: i64 = 300;

/// Characters used for pairing codes: unambiguous when read aloud or retyped.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_LENGTH: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayLink {
    pub id: String,
    pub relay_base_url: String,
    pub account_id: Option<String>,
    pub account_email: Option<String>,
    pub display_name: Option<String>,
    /// True when a token for this link exists in the credential vault.
    pub has_token: bool,
    pub scopes: Vec<String>,
    pub linked_at: Option<String>,
    pub revoked_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PairingCode {
    /// Shown once, in the desktop app. Never stored in this form.
    pub code: String,
    pub scopes: Vec<String>,
    pub expires_at: String,
}

fn hash_code(code: &str) -> String {
    let digest = Sha256::digest(code.trim().to_ascii_uppercase().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Scopes a pairing may request. A scope outside this list is refused, so a
/// WordPress site cannot widen its own access.
pub const ALLOWED_SCOPES: &[&str] = &[
    "profile.read",
    "profile.write",
    "projects.read",
    "tasks.read",
    "marketplace.read",
    "marketplace.write",
];

pub struct AccountRepo<'a> {
    db: &'a Db,
}

impl<'a> AccountRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    // ---- pairing

    /// Mint a pairing code. Returns the plaintext code exactly once; only its
    /// hash is written.
    pub fn create_pairing_code(&self, scopes: &[String]) -> Result<PairingCode> {
        for scope in scopes {
            if !ALLOWED_SCOPES.contains(&scope.as_str()) {
                bail!("unknown scope {scope:?}");
            }
        }
        if scopes.is_empty() {
            bail!("a pairing code must grant at least one scope");
        }

        let mut rng = rand::thread_rng();
        let code: String = (0..CODE_LENGTH)
            .map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char)
            .collect();
        let expires_at = otwono_types::now() + chrono::Duration::seconds(PAIRING_CODE_TTL_SECONDS);
        let expires_at = otwono_types::ids::format_ts(&expires_at);

        self.db.conn()?.execute(
            "INSERT INTO pairing_codes (code, scopes, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                hash_code(&code),
                crate::to_json(&scopes),
                crate::now_str(),
                expires_at
            ],
        )?;

        Ok(PairingCode {
            code,
            scopes: scopes.to_vec(),
            expires_at,
        })
    }

    /// Redeem a code. Succeeds at most once, and only before it expires.
    /// Returns the scopes the code carried.
    pub fn consume_pairing_code(&self, code: &str, site: &str) -> Result<Vec<String>> {
        let hashed = hash_code(code);
        let now = crate::now_str();

        let row: Option<(String, String, Option<String>)> = {
            let conn = self.db.conn()?;
            conn.query_row(
                "SELECT scopes, expires_at, consumed_at FROM pairing_codes WHERE code = ?1",
                [&hashed],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
        };

        let Some((scopes, expires_at, consumed_at)) = row else {
            bail!("that pairing code is not valid");
        };
        if consumed_at.is_some() {
            bail!("that pairing code has already been used");
        }
        if expires_at <= now {
            bail!("that pairing code has expired; generate a new one in the desktop app");
        }

        let updated = self.db.conn()?.execute(
            "UPDATE pairing_codes SET consumed_at = ?2, paired_site = ?3
              WHERE code = ?1 AND consumed_at IS NULL",
            params![hashed, now, site],
        )?;
        if updated == 0 {
            // Lost a race with a concurrent redemption.
            bail!("that pairing code has already been used");
        }
        Ok(crate::json_column::<Vec<String>>(Some(scopes)))
    }

    /// Remove codes that can no longer be used.
    pub fn purge_expired_codes(&self) -> Result<usize> {
        Ok(self.db.conn()?.execute(
            "DELETE FROM pairing_codes WHERE expires_at <= ?1 OR consumed_at IS NOT NULL",
            [crate::now_str()],
        )?)
    }

    // ---- relay link

    pub fn upsert_link(
        &self,
        relay_base_url: &str,
        account_id: Option<&str>,
        account_email: Option<&str>,
        display_name: Option<&str>,
        scopes: &[String],
        has_token: bool,
    ) -> Result<RelayLink> {
        let existing: Option<String> = {
            let conn = self.db.conn()?;
            conn.query_row("SELECT id FROM relay_links LIMIT 1", [], |r| r.get(0))
                .optional()?
        };
        let id = existing.unwrap_or_else(|| otwono_types::new_id("lnk"));
        let now = crate::now_str();
        self.db.conn()?.execute(
            "INSERT INTO relay_links
               (id, relay_base_url, account_id, account_email, display_name, has_token, scopes,
                linked_at, revoked_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?8)
             ON CONFLICT(id) DO UPDATE SET
               relay_base_url = excluded.relay_base_url,
               account_id = excluded.account_id,
               account_email = excluded.account_email,
               display_name = excluded.display_name,
               has_token = excluded.has_token,
               scopes = excluded.scopes,
               linked_at = excluded.linked_at,
               revoked_at = NULL",
            params![
                id,
                relay_base_url.trim_end_matches('/'),
                account_id,
                account_email,
                display_name,
                has_token as i64,
                crate::to_json(&scopes),
                now
            ],
        )?;
        self.link()?
            .ok_or_else(|| anyhow::anyhow!("link not found after upsert"))
    }

    pub fn link(&self) -> Result<Option<RelayLink>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                "SELECT id, relay_base_url, account_id, account_email, display_name, has_token,
                        scopes, linked_at, revoked_at
                   FROM relay_links LIMIT 1",
                [],
                |row| {
                    Ok(RelayLink {
                        id: row.get(0)?,
                        relay_base_url: row.get(1)?,
                        account_id: row.get(2)?,
                        account_email: row.get(3)?,
                        display_name: row.get(4)?,
                        has_token: row.get::<_, i64>(5)? != 0,
                        scopes: crate::json_column(row.get(6)?),
                        linked_at: row.get(7)?,
                        revoked_at: row.get(8)?,
                    })
                },
            )
            .optional()?)
    }

    /// Break the link. The caller deletes the token from the vault; this marks
    /// the row so that nothing tries to use it in the meantime.
    pub fn revoke_link(&self) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE relay_links SET revoked_at = ?1, has_token = 0, account_id = NULL,
                    account_email = NULL, scopes = '[]'",
            [crate::now_str()],
        )?;
        Ok(())
    }

    // ---- worker profile

    pub fn save_worker_profile(&self, profile: &WorkerProfile) -> Result<()> {
        self.db.conn()?.execute(
            "INSERT INTO worker_profiles
               (account_id, headline, skills, equipment, availability, location_radius_km,
                portfolio_links, accepts_on_site, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(account_id) DO UPDATE SET
               headline = excluded.headline, skills = excluded.skills,
               equipment = excluded.equipment, availability = excluded.availability,
               location_radius_km = excluded.location_radius_km,
               portfolio_links = excluded.portfolio_links,
               accepts_on_site = excluded.accepts_on_site, updated_at = excluded.updated_at",
            params![
                profile.account_id,
                profile.headline,
                crate::to_json(&profile.skills),
                crate::to_json(&profile.equipment),
                profile.availability,
                profile.location_radius_km.map(|v| v as i64),
                crate::to_json(&profile.portfolio_links),
                profile.accepts_on_site as i64,
                crate::now_str()
            ],
        )?;
        Ok(())
    }

    pub fn worker_profile(&self, account_id: &str) -> Result<Option<WorkerProfile>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                "SELECT account_id, headline, skills, equipment, availability,
                        location_radius_km, portfolio_links, accepts_on_site
                   FROM worker_profiles WHERE account_id = ?1",
                [account_id],
                |row| {
                    Ok(WorkerProfile {
                        account_id: row.get(0)?,
                        headline: row.get(1)?,
                        skills: crate::json_column(row.get(2)?),
                        equipment: crate::json_column(row.get(3)?),
                        availability: row.get(4)?,
                        location_radius_km: row.get::<_, Option<i64>>(5)?.map(|v| v as u32),
                        portfolio_links: crate::json_column(row.get(6)?),
                        accepts_on_site: row.get::<_, i64>(7)? != 0,
                    })
                },
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes() -> Vec<String> {
        vec!["profile.read".into(), "projects.read".into()]
    }

    #[test]
    fn a_pairing_code_is_readable_only_once_and_never_stored_in_the_clear() {
        let db = Db::open_in_memory().unwrap();
        let repo = AccountRepo::new(&db);
        let issued = repo.create_pairing_code(&scopes()).unwrap();
        assert_eq!(issued.code.len(), CODE_LENGTH);

        let stored: String = db
            .conn()
            .unwrap()
            .query_row("SELECT code FROM pairing_codes", [], |r| r.get(0))
            .unwrap();
        assert_ne!(stored, issued.code, "the code must be stored hashed");
        assert!(!stored.contains(&issued.code));
    }

    #[test]
    fn a_pairing_code_works_once_and_then_never_again() {
        let db = Db::open_in_memory().unwrap();
        let repo = AccountRepo::new(&db);
        let issued = repo.create_pairing_code(&scopes()).unwrap();

        let granted = repo
            .consume_pairing_code(&issued.code, "https://example.com")
            .unwrap();
        assert_eq!(granted, scopes());

        let err = repo
            .consume_pairing_code(&issued.code, "https://example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already been used"), "{err}");
    }

    #[test]
    fn pairing_codes_are_case_insensitive_and_tolerate_surrounding_space() {
        let db = Db::open_in_memory().unwrap();
        let repo = AccountRepo::new(&db);
        let issued = repo.create_pairing_code(&scopes()).unwrap();
        let typed = format!("  {}  ", issued.code.to_ascii_lowercase());
        assert!(repo
            .consume_pairing_code(&typed, "https://example.com")
            .is_ok());
    }

    #[test]
    fn an_expired_pairing_code_is_refused_with_a_useful_message() {
        let db = Db::open_in_memory().unwrap();
        let repo = AccountRepo::new(&db);
        let issued = repo.create_pairing_code(&scopes()).unwrap();
        db.conn()
            .unwrap()
            .execute(
                "UPDATE pairing_codes SET expires_at = '2000-01-01T00:00:00.000Z'",
                [],
            )
            .unwrap();

        let err = repo
            .consume_pairing_code(&issued.code, "https://example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("expired"), "{err}");
        assert!(
            err.contains("generate a new one"),
            "the message should say what to do: {err}"
        );
    }

    #[test]
    fn an_unknown_code_is_refused_without_saying_why() {
        let db = Db::open_in_memory().unwrap();
        let repo = AccountRepo::new(&db);
        let err = repo
            .consume_pairing_code("AAAAAAAA", "https://example.com")
            .unwrap_err();
        assert_eq!(err.to_string(), "that pairing code is not valid");
    }

    #[test]
    fn a_pairing_cannot_request_a_scope_the_application_does_not_define() {
        let db = Db::open_in_memory().unwrap();
        let repo = AccountRepo::new(&db);
        assert!(repo.create_pairing_code(&["admin".into()]).is_err());
        assert!(repo
            .create_pairing_code(&["knowledge.read".into()])
            .is_err());
        assert!(repo.create_pairing_code(&[]).is_err());
    }

    #[test]
    fn no_scope_grants_access_to_prompts_files_or_knowledge() {
        // Compare whole dotted segments: "profile" contains "file" as a
        // substring but is not access to files.
        for scope in ALLOWED_SCOPES {
            for segment in scope.split('.') {
                for forbidden in [
                    "knowledge",
                    "chat",
                    "chats",
                    "message",
                    "messages",
                    "file",
                    "files",
                    "model",
                    "models",
                    "conversation",
                    "conversations",
                    "secret",
                    "secrets",
                ] {
                    assert!(
                        segment != forbidden,
                        "scope {scope:?} would expose {forbidden} to a paired site"
                    );
                }
            }
        }
    }

    #[test]
    fn expired_and_used_codes_are_purged() {
        let db = Db::open_in_memory().unwrap();
        let repo = AccountRepo::new(&db);
        let used = repo.create_pairing_code(&scopes()).unwrap();
        repo.consume_pairing_code(&used.code, "https://example.com")
            .unwrap();
        repo.create_pairing_code(&scopes()).unwrap();

        assert_eq!(repo.purge_expired_codes().unwrap(), 1);
        let remaining: i64 = db
            .conn()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM pairing_codes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn the_relay_link_records_that_a_token_exists_but_never_the_token() {
        let db = Db::open_in_memory().unwrap();
        let repo = AccountRepo::new(&db);
        let link = repo
            .upsert_link(
                "https://relay.example.com/",
                Some("acc_1"),
                Some("person@example.com"),
                Some("A Person"),
                &scopes(),
                true,
            )
            .unwrap();
        assert_eq!(link.relay_base_url, "https://relay.example.com");
        assert!(link.has_token);

        let conn = db.conn().unwrap();
        let mut stmt = conn.prepare("PRAGMA table_info(relay_links)").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(!columns.iter().any(|c| c == "token" || c == "access_token"));
    }

    #[test]
    fn revoking_the_link_clears_the_account_and_scopes() {
        let db = Db::open_in_memory().unwrap();
        let repo = AccountRepo::new(&db);
        repo.upsert_link(
            "https://relay.example.com",
            Some("acc_1"),
            None,
            None,
            &scopes(),
            true,
        )
        .unwrap();
        repo.revoke_link().unwrap();

        let link = repo.link().unwrap().unwrap();
        assert!(!link.has_token);
        assert!(link.account_id.is_none());
        assert!(link.scopes.is_empty());
        assert!(link.revoked_at.is_some());
    }

    #[test]
    fn linking_twice_updates_the_single_link_rather_than_adding_one() {
        let db = Db::open_in_memory().unwrap();
        let repo = AccountRepo::new(&db);
        repo.upsert_link(
            "https://a.example",
            Some("acc_1"),
            None,
            None,
            &scopes(),
            true,
        )
        .unwrap();
        repo.upsert_link(
            "https://b.example",
            Some("acc_2"),
            None,
            None,
            &scopes(),
            true,
        )
        .unwrap();
        let count: i64 = db
            .conn()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM relay_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            repo.link().unwrap().unwrap().account_id.as_deref(),
            Some("acc_2")
        );
    }

    #[test]
    fn a_worker_profile_round_trips() {
        let db = Db::open_in_memory().unwrap();
        let repo = AccountRepo::new(&db);
        let profile = WorkerProfile {
            account_id: "acc_1".into(),
            headline: "Photographer in Leeds".into(),
            skills: vec!["photography".into(), "editing".into()],
            equipment: vec!["DSLR".into()],
            availability: "Weekday mornings".into(),
            location_radius_km: Some(25),
            portfolio_links: vec!["https://example.com/portfolio".into()],
            accepts_on_site: true,
        };
        repo.save_worker_profile(&profile).unwrap();

        let loaded = repo.worker_profile("acc_1").unwrap().unwrap();
        assert_eq!(loaded.headline, "Photographer in Leeds");
        assert_eq!(loaded.skills.len(), 2);
        assert_eq!(loaded.location_radius_km, Some(25));
        assert!(loaded.accepts_on_site);
    }
}

//! Marketplace records and the simulated ledger.
//!
//! Moderation runs on the way in: a listing that fails cannot reach
//! `published`, and the findings are stored with the row so the creator can see
//! exactly what to change.

use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension, Row};

use otwono_types::marketplace::{
    moderate, Application, ApplicationState, Listing, ListingState, ModerationFinding,
    ModerationVerdict, SafetyClass, Submission, WorkMode,
};

use crate::Db;

const LISTING_COLUMNS: &str = "id, creator_account_id, source_task_id, title, description, \
    category, work_mode, location_hint, deliverables, acceptance_criteria, evidence_required, \
    deadline, compensation_minor, expenses_minor, currency, safety_class, state, \
    simulated_payments, assigned_application_id, created_at, updated_at";

fn map_listing(row: &Row<'_>) -> rusqlite::Result<Listing> {
    Ok(Listing {
        id: row.get(0)?,
        creator_account_id: row.get(1)?,
        source_task_id: row.get(2)?,
        title: row.get(3)?,
        description: row.get(4)?,
        category: row.get(5)?,
        work_mode: if row.get::<_, String>(6)? == "on_site" {
            WorkMode::OnSite
        } else {
            WorkMode::Remote
        },
        location_hint: row.get(7)?,
        deliverables: crate::json_column(row.get(8)?),
        acceptance_criteria: crate::json_column(row.get(9)?),
        evidence_required: crate::json_column(row.get(10)?),
        deadline: crate::parse_ts_opt(row.get(11)?),
        compensation_minor: row.get(12)?,
        expenses_minor: row.get(13)?,
        currency: row.get(14)?,
        safety_class: match row.get::<_, String>(15)?.as_str() {
            "physical_on_site" => SafetyClass::PhysicalOnSite,
            "handles_personal_data" => SafetyClass::HandlesPersonalData,
            _ => SafetyClass::Standard,
        },
        state: ListingState::parse(&row.get::<_, String>(16)?).unwrap_or(ListingState::Draft),
        simulated_payments: row.get::<_, i64>(17)? != 0,
        assigned_application_id: row.get(18)?,
        created_at: crate::parse_ts(&row.get::<_, String>(19)?),
        updated_at: crate::parse_ts(&row.get::<_, String>(20)?),
    })
}

fn safety_str(class: SafetyClass) -> &'static str {
    match class {
        SafetyClass::Standard => "standard",
        SafetyClass::PhysicalOnSite => "physical_on_site",
        SafetyClass::HandlesPersonalData => "handles_personal_data",
    }
}

#[derive(Debug, Clone, Default)]
pub struct NewListing {
    pub creator_account_id: String,
    pub source_task_id: Option<String>,
    pub title: String,
    pub description: String,
    pub category: String,
    pub work_mode: Option<WorkMode>,
    pub location_hint: Option<String>,
    pub deliverables: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub evidence_required: Vec<String>,
    pub deadline: Option<otwono_types::Timestamp>,
    pub compensation_minor: i64,
    pub expenses_minor: i64,
    pub currency: Option<String>,
    pub safety_class: Option<SafetyClass>,
}

#[derive(Debug, Clone)]
pub struct LedgerEntry {
    pub id: String,
    pub listing_id: String,
    pub entry_type: String,
    pub amount_minor: i64,
    pub currency: String,
    pub account_id: String,
    pub simulated: bool,
    pub note: String,
    pub created_at: String,
}

pub struct MarketplaceRepo<'a> {
    db: &'a Db,
}

impl<'a> MarketplaceRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// Everything a moderator reads: title, description, deliverables and
    /// criteria, joined so that a prohibited phrase cannot hide in a field the
    /// screen skips.
    fn moderation_text(new: &NewListing) -> String {
        let mut parts = vec![
            new.title.clone(),
            new.description.clone(),
            new.category.clone(),
        ];
        parts.extend(new.deliverables.iter().cloned());
        parts.extend(new.acceptance_criteria.iter().cloned());
        parts.extend(new.evidence_required.iter().cloned());
        parts.join(" \n ")
    }

    /// Create a listing as a draft, running moderation first. A refused listing
    /// is stored in `rejected` with its findings, never in a state a worker can
    /// see.
    pub fn create_listing(&self, new: NewListing) -> Result<(Listing, ModerationVerdict)> {
        if new.title.trim().is_empty() {
            bail!("a listing needs a title");
        }
        if new.compensation_minor < 0 || new.expenses_minor < 0 {
            bail!("compensation and expenses must not be negative");
        }

        let verdict = moderate(&Self::moderation_text(&new));
        let (state, findings): (ListingState, Vec<ModerationFinding>) = match &verdict {
            ModerationVerdict::Allowed => (ListingState::Draft, Vec::new()),
            ModerationVerdict::Refused { findings, .. } => {
                (ListingState::Rejected, findings.clone())
            }
        };

        let id = otwono_types::new_id("lst");
        let now = crate::now_str();
        self.db.conn()?.execute(
            "INSERT INTO listings
               (id, creator_account_id, source_task_id, title, description, category, work_mode,
                location_hint, deliverables, acceptance_criteria, evidence_required, deadline,
                compensation_minor, expenses_minor, currency, safety_class, state,
                simulated_payments, moderation_findings, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                     1, ?18, ?19, ?19)",
            params![
                id,
                new.creator_account_id,
                new.source_task_id,
                new.title.trim(),
                new.description,
                if new.category.is_empty() {
                    "general".into()
                } else {
                    new.category.clone()
                },
                match new.work_mode.unwrap_or(WorkMode::Remote) {
                    WorkMode::Remote => "remote",
                    WorkMode::OnSite => "on_site",
                },
                new.location_hint,
                crate::to_json(&new.deliverables),
                crate::to_json(&new.acceptance_criteria),
                crate::to_json(&new.evidence_required),
                new.deadline.as_ref().map(otwono_types::ids::format_ts),
                new.compensation_minor,
                new.expenses_minor,
                new.currency.unwrap_or_else(|| "USD".into()),
                safety_str(new.safety_class.unwrap_or(SafetyClass::Standard)),
                state.as_str(),
                crate::to_json(&findings),
                now
            ],
        )?;
        let listing = self
            .get_listing(&id)?
            .ok_or_else(|| anyhow::anyhow!("listing not found after creation"))?;
        Ok((listing, verdict))
    }

    pub fn get_listing(&self, id: &str) -> Result<Option<Listing>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                &format!("SELECT {LISTING_COLUMNS} FROM listings WHERE id = ?1"),
                [id],
                map_listing,
            )
            .optional()?)
    }

    pub fn moderation_findings(&self, listing_id: &str) -> Result<Vec<ModerationFinding>> {
        let conn = self.db.conn()?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT moderation_findings FROM listings WHERE id = ?1",
                [listing_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(crate::json_column(raw))
    }

    /// Listings a worker may see. Only `published` ones, never drafts or
    /// rejected ones, whoever asks.
    pub fn browse(&self, limit: u32) -> Result<Vec<Listing>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {LISTING_COLUMNS} FROM listings WHERE state = 'published'
              ORDER BY updated_at DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit.clamp(1, 200) as i64], map_listing)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Everything a given creator owns, in any state.
    pub fn listings_for_creator(&self, account_id: &str) -> Result<Vec<Listing>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {LISTING_COLUMNS} FROM listings WHERE creator_account_id = ?1
              ORDER BY updated_at DESC"
        ))?;
        let rows = stmt.query_map([account_id], map_listing)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn transition_listing(&self, listing_id: &str, to: ListingState) -> Result<Listing> {
        let listing = self
            .get_listing(listing_id)?
            .ok_or_else(|| anyhow::anyhow!("listing {listing_id} does not exist"))?;
        let next = listing.state.transition(to)?;
        self.db.conn()?.execute(
            "UPDATE listings SET state = ?2, updated_at = ?3 WHERE id = ?1",
            params![listing_id, next.as_str(), crate::now_str()],
        )?;
        self.get_listing(listing_id)?
            .ok_or_else(|| anyhow::anyhow!("listing vanished"))
    }

    // ---- applications

    pub fn apply(
        &self,
        listing_id: &str,
        worker_account_id: &str,
        proposal: &str,
        quoted_minor: i64,
    ) -> Result<Application> {
        let listing = self
            .get_listing(listing_id)?
            .ok_or_else(|| anyhow::anyhow!("listing {listing_id} does not exist"))?;
        if !listing.state.is_worker_visible() {
            bail!("this task is not open for applications");
        }
        if listing.creator_account_id == worker_account_id {
            bail!("you cannot apply to your own task");
        }
        let id = otwono_types::new_id("app");
        self.db.conn()?.execute(
            "INSERT INTO applications (id, listing_id, worker_account_id, proposal, quoted_minor, state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'submitted', ?6)",
            params![id, listing_id, worker_account_id, proposal, quoted_minor, crate::now_str()],
        )?;
        self.get_application(&id)?
            .ok_or_else(|| anyhow::anyhow!("application not found after creation"))
    }

    pub fn get_application(&self, id: &str) -> Result<Option<Application>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                "SELECT id, listing_id, worker_account_id, proposal, quoted_minor, state, created_at
                   FROM applications WHERE id = ?1",
                [id],
                |row| {
                    Ok(Application {
                        id: row.get(0)?,
                        listing_id: row.get(1)?,
                        worker_account_id: row.get(2)?,
                        proposal: row.get(3)?,
                        quoted_minor: row.get(4)?,
                        state: match row.get::<_, String>(5)?.as_str() {
                            "withdrawn" => ApplicationState::Withdrawn,
                            "declined" => ApplicationState::Declined,
                            "assigned" => ApplicationState::Assigned,
                            _ => ApplicationState::Submitted,
                        },
                        created_at: crate::parse_ts(&row.get::<_, String>(6)?),
                    })
                },
            )
            .optional()?)
    }

    pub fn applications(&self, listing_id: &str) -> Result<Vec<Application>> {
        let conn = self.db.conn()?;
        let mut stmt =
            conn.prepare("SELECT id FROM applications WHERE listing_id = ?1 ORDER BY created_at")?;
        let ids: Vec<String> = stmt
            .query_map([listing_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        drop(conn);
        ids.iter()
            .filter_map(|id| self.get_application(id).transpose())
            .collect()
    }

    /// Assign a worker. Declines the other applications and moves the listing
    /// to `assigned`, all in one transaction.
    pub fn assign(&self, listing_id: &str, application_id: &str) -> Result<Listing> {
        let listing = self
            .get_listing(listing_id)?
            .ok_or_else(|| anyhow::anyhow!("listing {listing_id} does not exist"))?;
        let next = listing.state.transition(ListingState::Assigned)?;
        let application = self
            .get_application(application_id)?
            .ok_or_else(|| anyhow::anyhow!("application {application_id} does not exist"))?;
        if application.listing_id != listing_id {
            bail!("that application belongs to a different task");
        }

        let now = crate::now_str();
        self.db.transaction(|tx| {
            tx.execute(
                "UPDATE applications SET state = 'assigned' WHERE id = ?1",
                [application_id],
            )?;
            tx.execute(
                "UPDATE applications SET state = 'declined'
                  WHERE listing_id = ?1 AND id <> ?2 AND state = 'submitted'",
                params![listing_id, application_id],
            )?;
            tx.execute(
                "UPDATE listings SET state = ?2, assigned_application_id = ?3, updated_at = ?4
                  WHERE id = ?1",
                params![listing_id, next.as_str(), application_id, now],
            )?;
            Ok(())
        })?;
        self.get_listing(listing_id)?
            .ok_or_else(|| anyhow::anyhow!("listing vanished"))
    }

    // ---- messages, submissions, ledger, reports

    pub fn post_message(
        &self,
        listing_id: &str,
        sender_account_id: &str,
        body: &str,
    ) -> Result<String> {
        if body.trim().is_empty() {
            bail!("a message cannot be empty");
        }
        let id = otwono_types::new_id("mmg");
        self.db.conn()?.execute(
            "INSERT INTO marketplace_messages (id, listing_id, sender_account_id, body, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                listing_id,
                sender_account_id,
                body.trim(),
                crate::now_str()
            ],
        )?;
        Ok(id)
    }

    pub fn messages(&self, listing_id: &str) -> Result<Vec<(String, String, String, String)>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, sender_account_id, body, created_at FROM marketplace_messages
              WHERE listing_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map([listing_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn submit(
        &self,
        listing_id: &str,
        worker_account_id: &str,
        summary: &str,
        deliverable_links: &[String],
        evidence_notes: &str,
    ) -> Result<Submission> {
        let listing = self
            .get_listing(listing_id)?
            .ok_or_else(|| anyhow::anyhow!("listing {listing_id} does not exist"))?;
        listing.state.transition(ListingState::Submitted)?;

        let id = otwono_types::new_id("sub");
        let now = crate::now_str();
        self.db.transaction(|tx| {
            tx.execute(
                "INSERT INTO submissions
                   (id, listing_id, worker_account_id, summary, deliverable_links, evidence_notes, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id, listing_id, worker_account_id, summary,
                    crate::to_json(&deliverable_links), evidence_notes, now
                ],
            )?;
            tx.execute(
                "UPDATE listings SET state = 'submitted', updated_at = ?2 WHERE id = ?1",
                params![listing_id, now],
            )?;
            Ok(())
        })?;
        Ok(Submission {
            id,
            listing_id: listing_id.into(),
            worker_account_id: worker_account_id.into(),
            summary: summary.into(),
            deliverable_links: deliverable_links.to_vec(),
            evidence_notes: evidence_notes.into(),
            created_at: crate::parse_ts(&now),
        })
    }

    /// Record a simulated ledger entry. `simulated` is not a parameter.
    pub fn record_ledger_entry(
        &self,
        listing_id: &str,
        entry_type: &str,
        amount_minor: i64,
        currency: &str,
        account_id: &str,
        note: &str,
    ) -> Result<LedgerEntry> {
        let id = otwono_types::new_id("led");
        let now = crate::now_str();
        self.db.conn()?.execute(
            "INSERT INTO marketplace_ledger
               (id, listing_id, entry_type, amount_minor, currency, account_id, simulated, note, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8)",
            params![id, listing_id, entry_type, amount_minor, currency, account_id, note, now],
        )?;
        Ok(LedgerEntry {
            id,
            listing_id: listing_id.into(),
            entry_type: entry_type.into(),
            amount_minor,
            currency: currency.into(),
            account_id: account_id.into(),
            simulated: true,
            note: note.into(),
            created_at: now,
        })
    }

    pub fn ledger(&self, account_id: &str) -> Result<Vec<LedgerEntry>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, listing_id, entry_type, amount_minor, currency, account_id, simulated, note, created_at
               FROM marketplace_ledger WHERE account_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map([account_id], |row| {
            Ok(LedgerEntry {
                id: row.get(0)?,
                listing_id: row.get(1)?,
                entry_type: row.get(2)?,
                amount_minor: row.get(3)?,
                currency: row.get(4)?,
                account_id: row.get(5)?,
                simulated: row.get::<_, i64>(6)? != 0,
                note: row.get(7)?,
                created_at: row.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn report(
        &self,
        listing_id: &str,
        reporter_account_id: &str,
        reason: &str,
        detail: &str,
    ) -> Result<String> {
        let id = otwono_types::new_id("rep");
        self.db.conn()?.execute(
            "INSERT INTO moderation_reports
               (id, listing_id, reporter_account_id, reason, detail, state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6)",
            params![
                id,
                listing_id,
                reporter_account_id,
                reason,
                detail,
                crate::now_str()
            ],
        )?;
        Ok(id)
    }

    pub fn open_reports(&self) -> Result<Vec<(String, String, String, String)>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, listing_id, reason, detail FROM moderation_reports
              WHERE state = 'open' ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Fixed-window rate limiting. Returns `false` when the caller is over the
    /// limit for the current window.
    pub fn check_rate_limit(&self, bucket: &str, limit: u32, window_seconds: i64) -> Result<bool> {
        let now = otwono_types::now().timestamp();
        let window_start = (now - (now % window_seconds)).to_string();
        let conn = self.db.conn()?;
        conn.execute(
            "INSERT INTO rate_limits (bucket, window_start, count) VALUES (?1, ?2, 1)
             ON CONFLICT(bucket, window_start) DO UPDATE SET count = count + 1",
            params![bucket, window_start],
        )?;
        let count: i64 = conn.query_row(
            "SELECT count FROM rate_limits WHERE bucket = ?1 AND window_start = ?2",
            params![bucket, window_start],
            |r| r.get(0),
        )?;
        // Old windows are pruned opportunistically rather than on a timer.
        conn.execute(
            "DELETE FROM rate_limits WHERE CAST(window_start AS INTEGER) < ?1",
            [now - (window_seconds * 10)],
        )?;
        Ok(count <= limit as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_listing(creator: &str) -> NewListing {
        NewListing {
            creator_account_id: creator.into(),
            title: "Photograph a shopfront".into(),
            description: "Take five clear photographs of the shopfront and send them.".into(),
            category: "photography".into(),
            deliverables: vec!["Five JPEG photographs".into()],
            acceptance_criteria: vec!["Front of the shop is fully visible".into()],
            compensation_minor: 40_00,
            ..Default::default()
        }
    }

    fn published(db: &Db, creator: &str) -> Listing {
        let repo = MarketplaceRepo::new(db);
        let (listing, verdict) = repo.create_listing(ok_listing(creator)).unwrap();
        assert!(verdict.is_allowed());
        repo.transition_listing(&listing.id, ListingState::AwaitingCreatorApproval)
            .unwrap();
        repo.transition_listing(&listing.id, ListingState::Published)
            .unwrap()
    }

    #[test]
    fn a_clean_listing_starts_as_a_draft_with_simulated_payments() {
        let db = Db::open_in_memory().unwrap();
        let (listing, verdict) = MarketplaceRepo::new(&db)
            .create_listing(ok_listing("acc_1"))
            .unwrap();
        assert!(verdict.is_allowed());
        assert_eq!(listing.state, ListingState::Draft);
        assert!(listing.simulated_payments);
    }

    #[test]
    fn a_prohibited_listing_is_rejected_and_never_becomes_visible() {
        let db = Db::open_in_memory().unwrap();
        let repo = MarketplaceRepo::new(&db);
        let (listing, verdict) = repo
            .create_listing(NewListing {
                description: "Follow my ex and report where they go each evening.".into(),
                ..ok_listing("acc_1")
            })
            .unwrap();
        assert!(!verdict.is_allowed());
        assert_eq!(listing.state, ListingState::Rejected);
        assert!(repo.browse(50).unwrap().is_empty());
        assert!(repo
            .transition_listing(&listing.id, ListingState::Published)
            .is_err());

        let findings = repo.moderation_findings(&listing.id).unwrap();
        assert!(!findings.is_empty());
        assert!(findings[0].explanation.contains("not permitted"));
    }

    #[test]
    fn moderation_reads_every_free_text_field_not_only_the_description() {
        let db = Db::open_in_memory().unwrap();
        let repo = MarketplaceRepo::new(&db);
        let (listing, verdict) = repo
            .create_listing(NewListing {
                deliverables: vec!["A list of harvested logins".into()],
                ..ok_listing("acc_1")
            })
            .unwrap();
        assert!(
            !verdict.is_allowed(),
            "a prohibited deliverable must be caught"
        );
        assert_eq!(listing.state, ListingState::Rejected);
    }

    #[test]
    fn a_draft_must_pass_the_creators_own_approval_before_publishing() {
        let db = Db::open_in_memory().unwrap();
        let repo = MarketplaceRepo::new(&db);
        let (listing, _) = repo.create_listing(ok_listing("acc_1")).unwrap();
        assert!(repo
            .transition_listing(&listing.id, ListingState::Published)
            .is_err());
        repo.transition_listing(&listing.id, ListingState::AwaitingCreatorApproval)
            .unwrap();
        let published = repo
            .transition_listing(&listing.id, ListingState::Published)
            .unwrap();
        assert!(published.state.is_worker_visible());
        assert_eq!(repo.browse(50).unwrap().len(), 1);
    }

    #[test]
    fn only_published_listings_accept_applications() {
        let db = Db::open_in_memory().unwrap();
        let repo = MarketplaceRepo::new(&db);
        let (draft, _) = repo.create_listing(ok_listing("acc_1")).unwrap();
        let err = repo
            .apply(&draft.id, "acc_2", "I can do this", 40_00)
            .unwrap_err();
        assert!(
            err.to_string().contains("not open for applications"),
            "{err}"
        );
    }

    #[test]
    fn a_creator_cannot_apply_to_their_own_task() {
        let db = Db::open_in_memory().unwrap();
        let listing = published(&db, "acc_1");
        let err = MarketplaceRepo::new(&db)
            .apply(&listing.id, "acc_1", "me", 10_00)
            .unwrap_err();
        assert!(err.to_string().contains("your own task"), "{err}");
    }

    #[test]
    fn one_worker_cannot_apply_twice_to_the_same_task() {
        let db = Db::open_in_memory().unwrap();
        let listing = published(&db, "acc_1");
        let repo = MarketplaceRepo::new(&db);
        repo.apply(&listing.id, "acc_2", "first", 40_00).unwrap();
        assert!(repo.apply(&listing.id, "acc_2", "second", 30_00).is_err());
    }

    #[test]
    fn assigning_a_worker_declines_the_other_applicants() {
        let db = Db::open_in_memory().unwrap();
        let listing = published(&db, "acc_1");
        let repo = MarketplaceRepo::new(&db);
        let chosen = repo
            .apply(&listing.id, "acc_2", "I can do this", 40_00)
            .unwrap();
        repo.apply(&listing.id, "acc_3", "So can I", 35_00).unwrap();

        let assigned = repo.assign(&listing.id, &chosen.id).unwrap();
        assert_eq!(assigned.state, ListingState::Assigned);
        assert_eq!(
            assigned.assigned_application_id.as_deref(),
            Some(chosen.id.as_str())
        );

        let applications = repo.applications(&listing.id).unwrap();
        assert_eq!(
            applications
                .iter()
                .filter(|a| a.state == ApplicationState::Assigned)
                .count(),
            1
        );
        assert_eq!(
            applications
                .iter()
                .filter(|a| a.state == ApplicationState::Declined)
                .count(),
            1
        );
    }

    #[test]
    fn the_full_creator_and_worker_path_reaches_a_simulated_ledger() {
        let db = Db::open_in_memory().unwrap();
        let listing = published(&db, "acc_creator");
        let repo = MarketplaceRepo::new(&db);

        let application = repo
            .apply(&listing.id, "acc_worker", "I can do this", 40_00)
            .unwrap();
        repo.post_message(&listing.id, "acc_worker", "What time works?")
            .unwrap();
        repo.post_message(&listing.id, "acc_creator", "Any weekday morning.")
            .unwrap();
        assert_eq!(repo.messages(&listing.id).unwrap().len(), 2);

        repo.assign(&listing.id, &application.id).unwrap();
        repo.submit(
            &listing.id,
            "acc_worker",
            "Photographs attached",
            &["file:///home/w/photos.zip".into()],
            "Taken on 12 March",
        )
        .unwrap();
        assert_eq!(
            repo.get_listing(&listing.id).unwrap().unwrap().state,
            ListingState::Submitted
        );

        let accepted = repo
            .transition_listing(&listing.id, ListingState::Accepted)
            .unwrap();
        assert_eq!(accepted.state, ListingState::Accepted);

        let entry = repo
            .record_ledger_entry(
                &listing.id,
                "payout",
                40_00,
                "USD",
                "acc_worker",
                "Simulated payout on acceptance",
            )
            .unwrap();
        assert!(entry.simulated);
        let ledger = repo.ledger("acc_worker").unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].amount_minor, 40_00);
        assert!(ledger[0].simulated);
    }

    #[test]
    fn a_revision_can_be_requested_and_the_work_resubmitted() {
        let db = Db::open_in_memory().unwrap();
        let listing = published(&db, "acc_creator");
        let repo = MarketplaceRepo::new(&db);
        let application = repo.apply(&listing.id, "acc_worker", "x", 10_00).unwrap();
        repo.assign(&listing.id, &application.id).unwrap();
        repo.submit(&listing.id, "acc_worker", "v1", &[], "")
            .unwrap();

        repo.transition_listing(&listing.id, ListingState::RevisionRequested)
            .unwrap();
        repo.submit(&listing.id, "acc_worker", "v2", &[], "")
            .unwrap();
        assert_eq!(
            repo.get_listing(&listing.id).unwrap().unwrap().state,
            ListingState::Submitted
        );
    }

    #[test]
    fn a_dispute_can_be_raised_and_reports_are_queued_for_a_person() {
        let db = Db::open_in_memory().unwrap();
        let listing = published(&db, "acc_creator");
        let repo = MarketplaceRepo::new(&db);
        let application = repo.apply(&listing.id, "acc_worker", "x", 10_00).unwrap();
        repo.assign(&listing.id, &application.id).unwrap();
        repo.transition_listing(&listing.id, ListingState::Disputed)
            .unwrap();

        repo.report(
            &listing.id,
            "acc_worker",
            "misleading",
            "The brief changed after assignment",
        )
        .unwrap();
        let reports = repo.open_reports().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].2, "misleading");
    }

    #[test]
    fn rate_limits_stop_a_flood_of_listings() {
        let db = Db::open_in_memory().unwrap();
        let repo = MarketplaceRepo::new(&db);
        for attempt in 1..=3 {
            assert!(
                repo.check_rate_limit("listing:acc_1", 3, 3600).unwrap(),
                "attempt {attempt} should be allowed"
            );
        }
        assert!(!repo.check_rate_limit("listing:acc_1", 3, 3600).unwrap());
        assert!(
            repo.check_rate_limit("listing:acc_2", 3, 3600).unwrap(),
            "another account has its own allowance"
        );
    }

    #[test]
    fn negative_compensation_is_refused() {
        let db = Db::open_in_memory().unwrap();
        assert!(MarketplaceRepo::new(&db)
            .create_listing(NewListing {
                compensation_minor: -1,
                ..ok_listing("acc_1")
            })
            .is_err());
    }
}

//! The invitation-only human task marketplace, with simulated payments.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use otwono_store::repo::account::AccountRepo;
use otwono_store::repo::activity::{ActivityRepo, NewActivity, Outcome};
use otwono_store::repo::marketplace::{LedgerEntry, MarketplaceRepo, NewListing};
use otwono_types::marketplace::{
    Application, Listing, ListingState, ModerationFinding, ModerationVerdict, SafetyClass,
    Submission, WorkMode, WorkerProfile,
};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub const SIMULATION_NOTICE: &str =
    "This marketplace is a development preview. Payments are simulated: no money moves and no \
     worker is really paid.";

/// The account identifier used while no relay account is linked. Everything the
/// marketplace stores is local until the user links an account and opts in.
fn local_account_id(state: &AppState) -> String {
    AccountRepo::new(&state.db)
        .link()
        .ok()
        .flatten()
        .and_then(|link| link.account_id)
        .unwrap_or_else(|| "local-user".to_string())
}

/// Rate limits, applied per account per hour.
const LISTINGS_PER_HOUR: u32 = 20;
const APPLICATIONS_PER_HOUR: u32 = 40;
const MESSAGES_PER_HOUR: u32 = 120;
const HOUR: i64 = 3_600;

#[derive(Debug, Deserialize)]
pub struct BrowseQuery {
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct BrowseResponse {
    pub listings: Vec<Listing>,
    pub notice: &'static str,
}

pub async fn browse(
    State(state): State<AppState>,
    Query(query): Query<BrowseQuery>,
) -> ApiResult<Json<BrowseResponse>> {
    Ok(Json(BrowseResponse {
        listings: MarketplaceRepo::new(&state.db).browse(query.limit.unwrap_or(50))?,
        notice: SIMULATION_NOTICE,
    }))
}

#[derive(Debug, Deserialize)]
pub struct MyWorkQuery {
    #[serde(default)]
    pub worker_account_id: Option<String>,
}

/// The listings this worker is involved in. Browsing only shows what is still
/// open, so without this a worker cannot reach the job they were given.
pub async fn my_work(
    State(state): State<AppState>,
    Query(query): Query<MyWorkQuery>,
) -> ApiResult<Json<BrowseResponse>> {
    let worker = query
        .worker_account_id
        .unwrap_or_else(|| local_account_id(&state));
    Ok(Json(BrowseResponse {
        listings: MarketplaceRepo::new(&state.db).listings_for_worker(&worker)?,
        notice: SIMULATION_NOTICE,
    }))
}

#[derive(Debug, Serialize)]
pub struct MyListingsResponse {
    pub listings: Vec<ListingWithFindings>,
    pub notice: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ListingWithFindings {
    #[serde(flatten)]
    pub listing: Listing,
    pub moderation_findings: Vec<ModerationFinding>,
    pub applications: usize,
}

pub async fn my_listings(State(state): State<AppState>) -> ApiResult<Json<MyListingsResponse>> {
    let repo = MarketplaceRepo::new(&state.db);
    let account = local_account_id(&state);
    let mut listings = Vec::new();
    for listing in repo.listings_for_creator(&account)? {
        listings.push(ListingWithFindings {
            moderation_findings: repo.moderation_findings(&listing.id)?,
            applications: repo.applications(&listing.id)?.len(),
            listing,
        });
    }
    Ok(Json(MyListingsResponse {
        listings,
        notice: SIMULATION_NOTICE,
    }))
}

#[derive(Debug, Deserialize)]
pub struct CreateListing {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub work_mode: Option<String>,
    #[serde(default)]
    pub location_hint: Option<String>,
    #[serde(default)]
    pub deliverables: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub evidence_required: Vec<String>,
    #[serde(default)]
    pub compensation_minor: i64,
    #[serde(default)]
    pub expenses_minor: i64,
    #[serde(default)]
    pub safety_class: Option<String>,
    #[serde(default)]
    pub source_task_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateListingResponse {
    pub listing: Listing,
    pub moderation: ModerationVerdict,
    pub notice: &'static str,
}

pub async fn create_listing(
    State(state): State<AppState>,
    Json(body): Json<CreateListing>,
) -> ApiResult<Json<CreateListingResponse>> {
    let repo = MarketplaceRepo::new(&state.db);
    let account = local_account_id(&state);

    if !repo.check_rate_limit(&format!("listing:{account}"), LISTINGS_PER_HOUR, HOUR)? {
        return Err(ApiError::Conflict(format!(
            "You have created {LISTINGS_PER_HOUR} listings in the last hour. Try again later."
        )));
    }

    let work_mode = match body.work_mode.as_deref() {
        Some("on_site") => Some(WorkMode::OnSite),
        Some("remote") | None => Some(WorkMode::Remote),
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "{other:?} is not a work mode. Use remote or on_site."
            )))
        }
    };
    let safety_class = match body.safety_class.as_deref() {
        Some("physical_on_site") => Some(SafetyClass::PhysicalOnSite),
        Some("handles_personal_data") => Some(SafetyClass::HandlesPersonalData),
        Some("standard") | None => Some(SafetyClass::Standard),
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "{other:?} is not a safety classification."
            )))
        }
    };

    let (listing, verdict) = repo
        .create_listing(NewListing {
            creator_account_id: account,
            source_task_id: body.source_task_id,
            title: body.title,
            description: body.description,
            category: body.category,
            work_mode,
            location_hint: body.location_hint,
            deliverables: body.deliverables,
            acceptance_criteria: body.acceptance_criteria,
            evidence_required: body.evidence_required,
            deadline: None,
            compensation_minor: body.compensation_minor,
            expenses_minor: body.expenses_minor,
            currency: None,
            safety_class,
        })
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    ActivityRepo::new(&state.db).record(
        NewActivity::user("marketplace.listing_created")
            .with_target("listing", &listing.id)
            .with_outcome(if verdict.is_allowed() {
                Outcome::Ok
            } else {
                Outcome::Denied
            })
            .with_detail(serde_json::json!({ "state": listing.state.as_str() })),
    )?;

    Ok(Json(CreateListingResponse {
        listing,
        moderation: verdict,
        notice: SIMULATION_NOTICE,
    }))
}

#[derive(Debug, Serialize)]
pub struct ListingDetail {
    #[serde(flatten)]
    pub listing: Listing,
    pub moderation_findings: Vec<ModerationFinding>,
    pub applications: Vec<Application>,
    pub messages: Vec<MarketMessage>,
    pub notice: &'static str,
}

#[derive(Debug, Serialize)]
pub struct MarketMessage {
    pub id: String,
    pub sender_account_id: String,
    pub body: String,
    pub created_at: String,
}

pub async fn get_listing(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ListingDetail>> {
    let repo = MarketplaceRepo::new(&state.db);
    let listing = repo
        .get_listing(&id)?
        .ok_or_else(|| ApiError::not_found("That task"))?;
    Ok(Json(ListingDetail {
        moderation_findings: repo.moderation_findings(&id)?,
        applications: repo.applications(&id)?,
        messages: repo
            .messages(&id)?
            .into_iter()
            .map(|(id, sender, body, created_at)| MarketMessage {
                id,
                sender_account_id: sender,
                body,
                created_at,
            })
            .collect(),
        listing,
        notice: SIMULATION_NOTICE,
    }))
}

#[derive(Debug, Deserialize)]
pub struct StateChange {
    pub state: String,
}

pub async fn transition_listing(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<StateChange>,
) -> ApiResult<Json<Listing>> {
    let target =
        ListingState::parse(&body.state).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    MarketplaceRepo::new(&state.db)
        .transition_listing(&id, target)
        .map(Json)
        .map_err(|e| ApiError::Conflict(e.to_string()))
}

#[derive(Debug, Deserialize)]
pub struct ApplyRequest {
    pub proposal: String,
    #[serde(default)]
    pub quoted_minor: i64,
    /// Present so a second account can be simulated in the development MVP.
    #[serde(default)]
    pub worker_account_id: Option<String>,
}

pub async fn apply(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ApplyRequest>,
) -> ApiResult<Json<Application>> {
    let repo = MarketplaceRepo::new(&state.db);
    let worker = body
        .worker_account_id
        .unwrap_or_else(|| local_account_id(&state));

    if !repo.check_rate_limit(&format!("apply:{worker}"), APPLICATIONS_PER_HOUR, HOUR)? {
        return Err(ApiError::Conflict(
            "You have sent a lot of applications in the last hour. Try again later.".into(),
        ));
    }

    repo.apply(&id, &worker, &body.proposal, body.quoted_minor)
        .map(Json)
        .map_err(|e| ApiError::Conflict(e.to_string()))
}

#[derive(Debug, Deserialize)]
pub struct AssignRequest {
    pub application_id: String,
}

pub async fn assign(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AssignRequest>,
) -> ApiResult<Json<Listing>> {
    MarketplaceRepo::new(&state.db)
        .assign(&id, &body.application_id)
        .map(Json)
        .map_err(|e| ApiError::Conflict(e.to_string()))
}

#[derive(Debug, Deserialize)]
pub struct MessageRequest {
    pub body: String,
    #[serde(default)]
    pub sender_account_id: Option<String>,
}

pub async fn post_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<MessageRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let repo = MarketplaceRepo::new(&state.db);
    let sender = body
        .sender_account_id
        .unwrap_or_else(|| local_account_id(&state));
    if !repo.check_rate_limit(&format!("message:{sender}"), MESSAGES_PER_HOUR, HOUR)? {
        return Err(ApiError::Conflict(
            "You have sent a lot of messages in the last hour. Try again later.".into(),
        ));
    }
    let message_id = repo
        .post_message(&id, &sender, &body.body)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(Json(serde_json::json!({ "id": message_id })))
}

#[derive(Debug, Deserialize)]
pub struct SubmitRequest {
    pub summary: String,
    #[serde(default)]
    pub deliverable_links: Vec<String>,
    #[serde(default)]
    pub evidence_notes: String,
    #[serde(default)]
    pub worker_account_id: Option<String>,
}

pub async fn submit(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SubmitRequest>,
) -> ApiResult<Json<Submission>> {
    let worker = body
        .worker_account_id
        .unwrap_or_else(|| local_account_id(&state));
    MarketplaceRepo::new(&state.db)
        .submit(
            &id,
            &worker,
            &body.summary,
            &body.deliverable_links,
            &body.evidence_notes,
        )
        .map(Json)
        .map_err(|e| ApiError::Conflict(e.to_string()))
}

#[derive(Debug, Deserialize)]
pub struct ReviewRequest {
    /// "accept", "request_revision" or "dispute".
    pub decision: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ReviewResponse {
    pub listing: Listing,
    pub ledger_entry: Option<LedgerEntry>,
    pub notice: &'static str,
}

/// Accepting settles the *simulated* ledger. Nothing else in the codebase can
/// write a ledger row that is not simulated.
pub async fn review(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ReviewRequest>,
) -> ApiResult<Json<ReviewResponse>> {
    let repo = MarketplaceRepo::new(&state.db);
    let listing = repo
        .get_listing(&id)?
        .ok_or_else(|| ApiError::not_found("That task"))?;

    let target = match body.decision.as_str() {
        "accept" => ListingState::Accepted,
        "request_revision" => ListingState::RevisionRequested,
        "dispute" => ListingState::Disputed,
        other => {
            return Err(ApiError::BadRequest(format!(
                "{other:?} is not a decision. Use accept, request_revision or dispute."
            )))
        }
    };

    let updated = repo
        .transition_listing(&id, target)
        .map_err(|e| ApiError::Conflict(e.to_string()))?;

    let mut ledger_entry = None;
    if target == ListingState::Accepted {
        let worker = repo
            .applications(&id)?
            .into_iter()
            .find(|application| {
                Some(application.id.as_str()) == listing.assigned_application_id.as_deref()
            })
            .map(|application| application.worker_account_id)
            .unwrap_or_else(|| "unknown-worker".to_string());

        ledger_entry = Some(repo.record_ledger_entry(
            &id,
            "payout",
            listing.compensation_minor + listing.expenses_minor,
            &listing.currency,
            &worker,
            "Simulated payout recorded when the creator accepted the work.",
        )?);
    }

    if let Some(note) = body.note.filter(|n| !n.trim().is_empty()) {
        repo.post_message(&id, &local_account_id(&state), &note)
            .ok();
    }

    Ok(Json(ReviewResponse {
        listing: updated,
        ledger_entry,
        notice: SIMULATION_NOTICE,
    }))
}

#[derive(Debug, Serialize)]
pub struct LedgerResponse {
    pub entries: Vec<LedgerEntry>,
    pub total_minor: i64,
    pub notice: &'static str,
}

/// The simulated ledger for one account. A payout is recorded against the
/// worker who did the work, so the worker view has to be able to ask for that
/// account rather than always the machine's own.
pub async fn ledger(
    State(state): State<AppState>,
    Query(query): Query<MyWorkQuery>,
) -> ApiResult<Json<LedgerResponse>> {
    let account = query
        .worker_account_id
        .unwrap_or_else(|| local_account_id(&state));
    let entries = MarketplaceRepo::new(&state.db).ledger(&account)?;
    Ok(Json(LedgerResponse {
        total_minor: entries.iter().map(|entry| entry.amount_minor).sum(),
        entries,
        notice: SIMULATION_NOTICE,
    }))
}

#[derive(Debug, Deserialize)]
pub struct ReportRequest {
    pub reason: String,
    #[serde(default)]
    pub detail: String,
}

pub async fn report(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ReportRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let repo = MarketplaceRepo::new(&state.db);
    let report_id = repo.report(&id, &local_account_id(&state), &body.reason, &body.detail)?;
    ActivityRepo::new(&state.db).record(
        NewActivity::user("marketplace.reported")
            .with_target("listing", &id)
            .with_outcome(Outcome::Pending),
    )?;
    Ok(Json(serde_json::json!({
        "id": report_id,
        "message": "Thank you. A person will read this report before any further action."
    })))
}

pub async fn worker_profile(
    State(state): State<AppState>,
) -> ApiResult<Json<Option<WorkerProfile>>> {
    Ok(Json(
        AccountRepo::new(&state.db).worker_profile(&local_account_id(&state))?,
    ))
}

pub async fn save_worker_profile(
    State(state): State<AppState>,
    Json(mut profile): Json<WorkerProfile>,
) -> ApiResult<Json<WorkerProfile>> {
    profile.account_id = local_account_id(&state);
    AccountRepo::new(&state.db).save_worker_profile(&profile)?;
    Ok(Json(profile))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean_listing() -> CreateListing {
        CreateListing {
            title: "Photograph a shopfront".into(),
            description: "Take five clear photographs of the shopfront.".into(),
            category: "photography".into(),
            work_mode: Some("on_site".into()),
            location_hint: Some("Leeds".into()),
            deliverables: vec!["Five JPEG photographs".into()],
            acceptance_criteria: vec!["The whole frontage is visible".into()],
            evidence_required: vec!["Timestamped photographs".into()],
            compensation_minor: 40_00,
            expenses_minor: 5_00,
            safety_class: Some("physical_on_site".into()),
            source_task_id: None,
        }
    }

    #[tokio::test]
    async fn a_prohibited_listing_is_refused_with_findings_and_never_published() {
        let state = AppState::for_tests();
        let Json(response) = create_listing(
            State(state.clone()),
            Json(CreateListing {
                description: "Follow my ex and report where they go.".into(),
                ..clean_listing()
            }),
        )
        .await
        .unwrap();

        assert!(!response.moderation.is_allowed());
        assert_eq!(response.listing.state, ListingState::Rejected);

        let error = transition_listing(
            State(state.clone()),
            Path(response.listing.id.clone()),
            Json(StateChange {
                state: "published".into(),
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::Conflict(_)));

        let Json(browse_response) = browse(State(state), Query(BrowseQuery { limit: None }))
            .await
            .unwrap();
        assert!(browse_response.listings.is_empty());
    }

    #[tokio::test]
    async fn the_full_path_reaches_a_simulated_payout() {
        let state = AppState::for_tests();
        let Json(created) = create_listing(State(state.clone()), Json(clean_listing()))
            .await
            .unwrap();
        assert!(created.moderation.is_allowed());

        let _ = transition_listing(
            State(state.clone()),
            Path(created.listing.id.clone()),
            Json(StateChange {
                state: "awaiting_creator_approval".into(),
            }),
        )
        .await
        .unwrap();
        let _ = transition_listing(
            State(state.clone()),
            Path(created.listing.id.clone()),
            Json(StateChange {
                state: "published".into(),
            }),
        )
        .await
        .unwrap();

        let Json(application) = apply(
            State(state.clone()),
            Path(created.listing.id.clone()),
            Json(ApplyRequest {
                proposal: "I live nearby and can go tomorrow.".into(),
                quoted_minor: 40_00,
                worker_account_id: Some("worker-1".into()),
            }),
        )
        .await
        .unwrap();

        let _ = assign(
            State(state.clone()),
            Path(created.listing.id.clone()),
            Json(AssignRequest {
                application_id: application.id,
            }),
        )
        .await
        .unwrap();

        let _ = submit(
            State(state.clone()),
            Path(created.listing.id.clone()),
            Json(SubmitRequest {
                summary: "Photographs attached.".into(),
                deliverable_links: vec!["file:///photos.zip".into()],
                evidence_notes: "Taken on 12 March.".into(),
                worker_account_id: Some("worker-1".into()),
            }),
        )
        .await
        .unwrap();

        let Json(review_response) = review(
            State(state.clone()),
            Path(created.listing.id.clone()),
            Json(ReviewRequest {
                decision: "accept".into(),
                note: Some("Thank you".into()),
            }),
        )
        .await
        .unwrap();

        assert_eq!(review_response.listing.state, ListingState::Accepted);
        let entry = review_response.ledger_entry.expect("a ledger entry");
        assert!(entry.simulated);
        assert_eq!(entry.amount_minor, 45_00);
        assert!(entry.note.contains("Simulated"));

        let entries = MarketplaceRepo::new(&state.db).ledger("worker-1").unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].simulated);
    }

    #[tokio::test]
    async fn a_report_promises_a_person_will_read_it() {
        let state = AppState::for_tests();
        let Json(created) = create_listing(State(state.clone()), Json(clean_listing()))
            .await
            .unwrap();
        let Json(response) = report(
            State(state),
            Path(created.listing.id),
            Json(ReportRequest {
                reason: "misleading".into(),
                detail: "The brief changed.".into(),
            }),
        )
        .await
        .unwrap();
        assert!(response["message"]
            .as_str()
            .unwrap()
            .contains("A person will read"));
    }

    #[tokio::test]
    async fn every_marketplace_response_says_payments_are_simulated() {
        let state = AppState::for_tests();
        let Json(browse_response) =
            browse(State(state.clone()), Query(BrowseQuery { limit: None }))
                .await
                .unwrap();
        assert!(browse_response.notice.contains("simulated"));

        let Json(ledger_response) = ledger(
            State(state),
            Query(MyWorkQuery {
                worker_account_id: None,
            }),
        )
        .await
        .unwrap();
        assert!(ledger_response.notice.contains("no worker is really paid"));
    }
}

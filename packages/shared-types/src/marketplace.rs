//! Human task marketplace — invitation-only development MVP.
//!
//! Payments are simulated (`DECISIONS.md` D-007). The moderation rules in this
//! module are enforced before a listing can be published and are covered by
//! tests, so a listing that names a prohibited activity cannot go live.

use serde::{Deserialize, Serialize};

use crate::error::{DomainError, DomainResult};
use crate::ids::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListingState {
    Draft,
    /// Passed moderation, awaiting the creator's own approval to publish.
    AwaitingCreatorApproval,
    Published,
    Assigned,
    Submitted,
    RevisionRequested,
    Accepted,
    Disputed,
    Cancelled,
    /// Refused by moderation; never visible to workers.
    Rejected,
}

impl ListingState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::AwaitingCreatorApproval => "awaiting_creator_approval",
            Self::Published => "published",
            Self::Assigned => "assigned",
            Self::Submitted => "submitted",
            Self::RevisionRequested => "revision_requested",
            Self::Accepted => "accepted",
            Self::Disputed => "disputed",
            Self::Cancelled => "cancelled",
            Self::Rejected => "rejected",
        }
    }

    /// Only published listings are visible to workers who are not assigned.
    pub const fn is_worker_visible(self) -> bool {
        matches!(self, Self::Published)
    }

    pub fn allows(self, to: Self) -> bool {
        use ListingState::*;
        match (self, to) {
            (a, b) if a == b => false,
            (Draft, AwaitingCreatorApproval | Rejected | Cancelled) => true,
            (AwaitingCreatorApproval, Published | Draft | Cancelled | Rejected) => true,
            (Published, Assigned | Cancelled | Rejected) => true,
            (Assigned, Submitted | Disputed | Cancelled) => true,
            (Submitted, Accepted | RevisionRequested | Disputed) => true,
            (RevisionRequested, Submitted | Disputed | Cancelled) => true,
            (Disputed, Accepted | Cancelled) => true,
            _ => false,
        }
    }

    pub fn transition(self, to: Self) -> DomainResult<Self> {
        if self.allows(to) {
            Ok(to)
        } else {
            Err(DomainError::InvalidTransition {
                entity: "listing",
                from: self.as_str().into(),
                to: to.as_str().into(),
            })
        }
    }

    pub fn parse(value: &str) -> DomainResult<Self> {
        use ListingState::*;
        for candidate in [
            Draft,
            AwaitingCreatorApproval,
            Published,
            Assigned,
            Submitted,
            RevisionRequested,
            Accepted,
            Disputed,
            Cancelled,
            Rejected,
        ] {
            if candidate.as_str() == value {
                return Ok(candidate);
            }
        }
        Err(DomainError::validation("listing_state", format!("unknown {value:?}")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkMode {
    Remote,
    OnSite,
}

/// Safety class chosen by the creator and checked by moderation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyClass {
    /// Desk work, no physical risk, no personal data.
    Standard,
    /// Involves travel to a location or handling equipment.
    PhysicalOnSite,
    /// Touches personal data; requires an explicit handling statement.
    HandlesPersonalData,
}

/// Categories that may never be listed. Checked by keyword *and* by explicit
/// category selection so a creator cannot route around it either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProhibitedCategory {
    Illegal,
    PhysicalHarm,
    Weapons,
    Deception,
    Exploitation,
    Surveillance,
    CredentialHarvesting,
    Harassment,
    PrivacyInvasion,
}

impl ProhibitedCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Illegal => "illegal",
            Self::PhysicalHarm => "physical_harm",
            Self::Weapons => "weapons",
            Self::Deception => "deception",
            Self::Exploitation => "exploitation",
            Self::Surveillance => "surveillance",
            Self::CredentialHarvesting => "credential_harvesting",
            Self::Harassment => "harassment",
            Self::PrivacyInvasion => "privacy_invasion",
        }
    }

    pub const fn explanation(self) -> &'static str {
        match self {
            Self::Illegal => "Tasks that ask for unlawful activity are not permitted.",
            Self::PhysicalHarm => "Tasks that risk harm to a person are not permitted.",
            Self::Weapons => "Weapons-related tasks are not permitted.",
            Self::Deception => "Tasks that deceive or impersonate are not permitted.",
            Self::Exploitation => "Exploitative or coercive tasks are not permitted.",
            Self::Surveillance => "Covert monitoring of people is not permitted.",
            Self::CredentialHarvesting => {
                "Tasks that collect other people's credentials are not permitted."
            }
            Self::Harassment => "Tasks that target or harass a person are not permitted.",
            Self::PrivacyInvasion => "Tasks that invade someone's privacy are not permitted.",
        }
    }
}

/// Keyword table backing moderation. Kept small, explicit and testable; it is
/// a floor, not a substitute for the human escalation path.
const PROHIBITED_TERMS: &[(ProhibitedCategory, &[&str])] = &[
    (
        ProhibitedCategory::Illegal,
        &["launder", "counterfeit", "smuggle", "burglary", "steal a", "traffick"],
    ),
    (
        ProhibitedCategory::PhysicalHarm,
        &["hurt someone", "beat up", "assault", "poison", "kill "],
    ),
    (
        ProhibitedCategory::Weapons,
        &["firearm", "gun parts", "ghost gun", "silencer", "explosive", "ammunition"],
    ),
    (
        ProhibitedCategory::Deception,
        &["fake review", "impersonate", "forge", "pretend to be a real", "astroturf"],
    ),
    (
        ProhibitedCategory::Exploitation,
        &["unpaid trial work", "under 16", "sweatshop", "debt bondage"],
    ),
    (
        ProhibitedCategory::Surveillance,
        &["follow my ex", "track a person", "covert camera", "stalk", "spy on"],
    ),
    (
        ProhibitedCategory::CredentialHarvesting,
        &["collect passwords", "phishing", "harvest logins", "credential dump", "otp code from"],
    ),
    (
        ProhibitedCategory::Harassment,
        &["harass", "dox", "flood their inbox", "intimidate"],
    ),
    (
        ProhibitedCategory::PrivacyInvasion,
        &["home address of", "private medical", "scrape personal data", "find where they live"],
    ),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModerationFinding {
    pub category: String,
    pub explanation: String,
    /// The matched phrase, so the creator can see exactly what to change.
    pub matched: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModerationVerdict {
    Allowed,
    /// Refused with at least one finding, plus the human escalation route.
    Refused {
        findings: Vec<ModerationFinding>,
        escalation: String,
    },
}

impl ModerationVerdict {
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// Screen the free-text parts of a listing. Case-insensitive substring match on
/// a normalised form so that spacing and capitalisation cannot evade it.
pub fn moderate(text: &str) -> ModerationVerdict {
    let normalised = text.to_ascii_lowercase().replace(['\n', '\t'], " ");
    let collapsed = normalised.split_whitespace().collect::<Vec<_>>().join(" ");
    let padded = format!(" {collapsed} ");

    let mut findings = Vec::new();
    for (category, terms) in PROHIBITED_TERMS {
        for term in *terms {
            if padded.contains(term) {
                findings.push(ModerationFinding {
                    category: category.as_str().to_string(),
                    explanation: category.explanation().to_string(),
                    matched: (*term).to_string(),
                });
                break;
            }
        }
    }

    if findings.is_empty() {
        ModerationVerdict::Allowed
    } else {
        ModerationVerdict::Refused {
            findings,
            escalation:
                "If you believe this is a mistake, request human review from the Marketplace \
                 screen; a person will read the listing."
                    .to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listing {
    pub id: String,
    pub creator_account_id: String,
    pub source_task_id: Option<String>,
    pub title: String,
    pub description: String,
    pub category: String,
    pub work_mode: WorkMode,
    pub location_hint: Option<String>,
    pub deliverables: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub evidence_required: Vec<String>,
    pub deadline: Option<Timestamp>,
    pub compensation_minor: i64,
    pub expenses_minor: i64,
    pub currency: String,
    pub safety_class: SafetyClass,
    pub state: ListingState,
    /// Always true: this marketplace never moves real money.
    pub simulated_payments: bool,
    pub assigned_application_id: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationState {
    Submitted,
    Withdrawn,
    Declined,
    Assigned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Application {
    pub id: String,
    pub listing_id: String,
    pub worker_account_id: String,
    pub proposal: String,
    pub quoted_minor: i64,
    pub state: ApplicationState,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerProfile {
    pub account_id: String,
    pub headline: String,
    pub skills: Vec<String>,
    pub equipment: Vec<String>,
    pub availability: String,
    pub location_radius_km: Option<u32>,
    pub portfolio_links: Vec<String>,
    pub accepts_on_site: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Submission {
    pub id: String,
    pub listing_id: String,
    pub worker_account_id: String,
    pub summary: String,
    pub deliverable_links: Vec<String>,
    pub evidence_notes: String,
    pub created_at: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_normal_listing_passes_moderation() {
        let verdict = moderate(
            "Photograph a shopfront in Leeds and send five images with a short caption.",
        );
        assert!(verdict.is_allowed(), "{verdict:?}");
    }

    #[test]
    fn prohibited_listings_are_refused_with_a_reason_and_an_escalation_route() {
        for text in [
            "Write fake reviews for my competitor",
            "Follow my ex and report where they go",
            "Collect passwords from these accounts",
            "Find the home address of this journalist",
            "Assemble firearm parts",
            "Help me launder some cash",
            "Harass this account until they delete it",
        ] {
            match moderate(text) {
                ModerationVerdict::Refused { findings, escalation } => {
                    assert!(!findings.is_empty(), "no finding for {text:?}");
                    assert!(findings[0].explanation.ends_with("not permitted."));
                    assert!(escalation.contains("human review"));
                }
                ModerationVerdict::Allowed => panic!("moderation missed: {text:?}"),
            }
        }
    }

    #[test]
    fn moderation_ignores_case_and_spacing() {
        assert!(!moderate("PHISHING   campaign\n setup").is_allowed());
        assert!(!moderate("Please\tIMPERSONATE the CEO").is_allowed());
    }

    #[test]
    fn only_published_listings_are_visible_to_workers() {
        assert!(ListingState::Published.is_worker_visible());
        for state in [
            ListingState::Draft,
            ListingState::AwaitingCreatorApproval,
            ListingState::Rejected,
            ListingState::Cancelled,
        ] {
            assert!(!state.is_worker_visible(), "{state:?} must stay hidden");
        }
    }

    #[test]
    fn a_draft_cannot_be_published_without_passing_creator_approval() {
        assert!(ListingState::Draft.transition(ListingState::Published).is_err());
        assert!(ListingState::Draft
            .transition(ListingState::AwaitingCreatorApproval)
            .is_ok());
        assert!(ListingState::AwaitingCreatorApproval
            .transition(ListingState::Published)
            .is_ok());
    }

    #[test]
    fn a_rejected_listing_is_final() {
        for target in [ListingState::Published, ListingState::Draft, ListingState::Assigned] {
            assert!(ListingState::Rejected.transition(target).is_err());
        }
    }

    #[test]
    fn listing_states_round_trip() {
        for state in [ListingState::Submitted, ListingState::RevisionRequested] {
            assert_eq!(ListingState::parse(state.as_str()).unwrap(), state);
        }
        assert!(ListingState::parse("paid").is_err());
    }
}

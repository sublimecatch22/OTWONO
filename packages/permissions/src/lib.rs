//! The permission engine.
//!
//! Three rules, in order:
//!
//! 1. If the emergency stop is engaged, nothing is allowed. Not even a
//!    capability with a standing grant.
//! 2. The most specific matching grant wins. A deny at the same specificity as
//!    an allow wins, because refusing wrongly is cheaper than permitting
//!    wrongly.
//! 3. With no matching grant, the answer is "ask the human" — never "yes".

pub mod path_policy;

use anyhow::Result;

use otwono_store::repo::permissions::PermissionRepo;
use otwono_store::repo::settings::SettingsRepo;
use otwono_store::Db;
use otwono_types::permission::{Capability, CheckOutcome, Decision, Grant, Scope};

/// What is being attempted, expressed as the scopes it touches. A grant matches
/// only if *every* scope it names is present in the request.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub capability: Option<Capability>,
    pub scopes: Vec<Scope>,
}

impl Request {
    pub fn new(capability: Capability) -> Self {
        Self {
            capability: Some(capability),
            scopes: Vec::new(),
        }
    }

    pub fn in_project(mut self, project_id: impl Into<String>) -> Self {
        self.scopes.push(Scope::Project {
            project_id: project_id.into(),
        });
        self
    }

    pub fn in_workspace(mut self, workspace_id: impl Into<String>) -> Self {
        self.scopes.push(Scope::Workspace {
            workspace_id: workspace_id.into(),
        });
        self
    }

    pub fn by_agent(mut self, agent_id: impl Into<String>) -> Self {
        self.scopes.push(Scope::Agent {
            agent_id: agent_id.into(),
        });
        self
    }

    pub fn on_path(mut self, path: impl Into<String>) -> Self {
        self.scopes.push(Scope::Path { path: path.into() });
        self
    }

    pub fn on_host(mut self, host: impl Into<String>) -> Self {
        self.scopes.push(Scope::Host { host: host.into() });
        self
    }
}

/// Does a grant's scope cover a scope in the request?
///
/// `Path` is a prefix relation — a grant on `/home/u/docs` covers
/// `/home/u/docs/a/b.txt`. Every other scope is an exact match; hosts in
/// particular do **not** match subdomains, so a grant for `example.com` never
/// covers `evil.example.com`.
fn scope_covers(grant_scope: &Scope, request_scope: &Scope) -> bool {
    match (grant_scope, request_scope) {
        (Scope::Global, _) => true,
        (Scope::Path { path: granted }, Scope::Path { path: requested }) => {
            path_policy::is_prefix_of(granted, requested)
        }
        (a, b) => a == b,
    }
}

/// A grant applies when every scope it names is covered by the request.
fn grant_applies(grant: &Grant, request: &Request) -> bool {
    if Some(grant.capability) != request.capability {
        return false;
    }
    if grant.scopes.is_empty() {
        // A grant with no scope is global in effect; treat it as such
        // explicitly rather than as a vacuous match nobody expects.
        return true;
    }
    grant.scopes.iter().all(|grant_scope| {
        matches!(grant_scope, Scope::Global)
            || request
                .scopes
                .iter()
                .any(|request_scope| scope_covers(grant_scope, request_scope))
    })
}

fn specificity(grant: &Grant) -> u8 {
    grant
        .scopes
        .iter()
        .map(|s| s.precedence())
        .max()
        .unwrap_or(0)
}

pub struct PermissionEngine<'a> {
    db: &'a Db,
}

impl<'a> PermissionEngine<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    pub fn emergency_stop(&self) -> Result<bool> {
        SettingsRepo::new(self.db).emergency_stop()
    }

    pub fn set_emergency_stop(&self, engaged: bool) -> Result<()> {
        SettingsRepo::new(self.db).set_emergency_stop(engaged)
    }

    /// Answer a request without changing anything.
    pub fn check(&self, request: &Request) -> Result<CheckOutcome> {
        if self.emergency_stop()? {
            return Ok(CheckOutcome::Stopped);
        }
        let Some(capability) = request.capability else {
            return Ok(CheckOutcome::Denied {
                reason: "no capability was named".into(),
            });
        };

        let candidates: Vec<Grant> = PermissionRepo::new(self.db)
            .active_grants()?
            .into_iter()
            .filter(|grant| grant_applies(grant, request))
            .collect();

        let Some(best) = candidates.iter().max_by(|a, b| {
            specificity(a)
                .cmp(&specificity(b))
                // At equal specificity a deny beats an allow.
                .then_with(|| deny_rank(a).cmp(&deny_rank(b)))
        }) else {
            return Ok(CheckOutcome::NeedsApproval {
                reason: format!(
                    "No permission has been given for OTWONO to {}.",
                    capability.human_request()
                ),
            });
        };

        Ok(match best.decision {
            Decision::Deny => CheckOutcome::Denied {
                reason: format!(
                    "You have refused permission for OTWONO to {}.",
                    capability.human_request()
                ),
            },
            Decision::Allow | Decision::AllowOnce => CheckOutcome::Allowed {
                grant_id: best.id.clone(),
            },
        })
    }

    /// Check and, if the winning grant was single-use, consume it. This is what
    /// callers about to perform an action should use.
    pub fn check_and_consume(&self, request: &Request) -> Result<CheckOutcome> {
        let outcome = self.check(request)?;
        if let CheckOutcome::Allowed { grant_id } = &outcome {
            let repo = PermissionRepo::new(self.db);
            if let Some(grant) = repo.get_grant(grant_id)? {
                if grant.decision == Decision::AllowOnce {
                    repo.consume_once(grant_id)?;
                }
            }
        }
        Ok(outcome)
    }

    /// Whether an agent's own policy requires a human confirmation for this
    /// capability even when a standing grant exists.
    pub fn policy_requires_confirmation(
        policy: otwono_types::agent::ApprovalPolicy,
        capability: Capability,
    ) -> bool {
        use otwono_types::agent::ApprovalPolicy::*;
        match policy {
            Always => true,
            OffDeviceOnly => capability.leaves_device(),
            Standing => false,
        }
    }

    /// Human-readable sentence for an approval dialog.
    pub fn summarise(capability: Capability, scopes: &[Scope], agent_name: Option<&str>) -> String {
        let who = agent_name.unwrap_or("OTWONO");
        let where_ = scopes
            .iter()
            .filter_map(|scope| match scope {
                Scope::Path { path } => Some(format!("in {path}")),
                Scope::Host { host } => Some(format!("at {host}")),
                Scope::Project { .. } => Some("for this project".to_string()),
                Scope::Workspace { .. } => Some("for this workspace".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(", ");

        if where_.is_empty() {
            format!("{who} wants to {}.", capability.human_request())
        } else {
            format!("{who} wants to {} {where_}.", capability.human_request())
        }
    }
}

/// Sort key that puts `Deny` last so `max_by` picks it at equal specificity.
fn deny_rank(grant: &Grant) -> u8 {
    match grant.decision {
        Decision::Deny => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otwono_store::repo::permissions::NewGrant;

    fn engine_db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn allow(db: &Db, capability: Capability, scopes: Vec<Scope>) -> Grant {
        PermissionRepo::new(db)
            .grant(NewGrant {
                capability,
                scopes,
                decision: Decision::Allow,
                spend_limit_minor: None,
                spend_category: None,
                expires_at: None,
                created_by: "user".into(),
                note: None,
            })
            .unwrap()
    }

    fn deny(db: &Db, capability: Capability, scopes: Vec<Scope>) -> Grant {
        PermissionRepo::new(db)
            .grant(NewGrant {
                capability,
                scopes,
                decision: Decision::Deny,
                spend_limit_minor: None,
                spend_category: None,
                expires_at: None,
                created_by: "user".into(),
                note: None,
            })
            .unwrap()
    }

    #[test]
    fn nothing_is_allowed_by_default() {
        let db = engine_db();
        let engine = PermissionEngine::new(&db);
        for capability in Capability::ALL {
            let outcome = engine.check(&Request::new(capability)).unwrap();
            assert!(
                matches!(outcome, CheckOutcome::NeedsApproval { .. }),
                "{capability} should default to asking, got {outcome:?}"
            );
        }
    }

    #[test]
    fn the_refusal_reads_as_a_sentence_a_person_can_act_on() {
        let db = engine_db();
        let outcome = PermissionEngine::new(&db)
            .check(&Request::new(Capability::HttpFetch))
            .unwrap();
        match outcome {
            CheckOutcome::NeedsApproval { reason } => {
                assert!(reason.starts_with("No permission"), "{reason}");
                assert!(reason.ends_with('.'), "{reason}");
                assert!(reason.contains("approved website"), "{reason}");
            }
            other => panic!("expected NeedsApproval, got {other:?}"),
        }
    }

    #[test]
    fn a_matching_grant_allows_the_action() {
        let db = engine_db();
        let grant = allow(&db, Capability::KnowledgeSearch, vec![Scope::Global]);
        let outcome = PermissionEngine::new(&db)
            .check(&Request::new(Capability::KnowledgeSearch))
            .unwrap();
        assert_eq!(outcome, CheckOutcome::Allowed { grant_id: grant.id });
    }

    #[test]
    fn a_grant_for_one_capability_does_not_cover_another() {
        let db = engine_db();
        allow(&db, Capability::KnowledgeSearch, vec![Scope::Global]);
        let outcome = PermissionEngine::new(&db)
            .check(&Request::new(Capability::FileWrite))
            .unwrap();
        assert!(matches!(outcome, CheckOutcome::NeedsApproval { .. }));
    }

    #[test]
    fn a_project_grant_does_not_leak_into_another_project() {
        let db = engine_db();
        allow(
            &db,
            Capability::FileRead,
            vec![Scope::Project {
                project_id: "prj_1".into(),
            }],
        );
        let engine = PermissionEngine::new(&db);

        assert!(matches!(
            engine
                .check(&Request::new(Capability::FileRead).in_project("prj_1"))
                .unwrap(),
            CheckOutcome::Allowed { .. }
        ));
        assert!(matches!(
            engine
                .check(&Request::new(Capability::FileRead).in_project("prj_2"))
                .unwrap(),
            CheckOutcome::NeedsApproval { .. }
        ));
    }

    #[test]
    fn a_path_grant_covers_files_beneath_it_but_not_a_sibling() {
        let db = engine_db();
        allow(
            &db,
            Capability::FileRead,
            vec![Scope::Path {
                path: "/home/u/docs".into(),
            }],
        );
        let engine = PermissionEngine::new(&db);

        assert!(matches!(
            engine
                .check(&Request::new(Capability::FileRead).on_path("/home/u/docs/a/b.txt"))
                .unwrap(),
            CheckOutcome::Allowed { .. }
        ));
        for outside in ["/home/u/secrets/x.txt", "/home/u/docsx/y.txt", "/home/u"] {
            assert!(
                matches!(
                    engine
                        .check(&Request::new(Capability::FileRead).on_path(outside))
                        .unwrap(),
                    CheckOutcome::NeedsApproval { .. }
                ),
                "{outside} must not be covered"
            );
        }
    }

    #[test]
    fn a_host_grant_does_not_cover_a_subdomain() {
        let db = engine_db();
        allow(
            &db,
            Capability::HttpFetch,
            vec![Scope::Host {
                host: "example.com".into(),
            }],
        );
        let engine = PermissionEngine::new(&db);

        assert!(matches!(
            engine
                .check(&Request::new(Capability::HttpFetch).on_host("example.com"))
                .unwrap(),
            CheckOutcome::Allowed { .. }
        ));
        assert!(
            matches!(
                engine
                    .check(&Request::new(Capability::HttpFetch).on_host("evil.example.com"))
                    .unwrap(),
                CheckOutcome::NeedsApproval { .. }
            ),
            "a subdomain is a different host"
        );
    }

    #[test]
    fn a_narrower_deny_beats_a_broader_allow() {
        let db = engine_db();
        allow(&db, Capability::FileRead, vec![Scope::Global]);
        deny(
            &db,
            Capability::FileRead,
            vec![Scope::Path {
                path: "/home/u/private".into(),
            }],
        );
        let engine = PermissionEngine::new(&db);

        assert!(matches!(
            engine
                .check(&Request::new(Capability::FileRead).on_path("/home/u/docs/a.txt"))
                .unwrap(),
            CheckOutcome::Allowed { .. }
        ));
        match engine
            .check(&Request::new(Capability::FileRead).on_path("/home/u/private/diary.txt"))
            .unwrap()
        {
            CheckOutcome::Denied { reason } => assert!(reason.contains("refused"), "{reason}"),
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn at_equal_specificity_a_deny_wins() {
        let db = engine_db();
        let scope = vec![Scope::Project {
            project_id: "prj_1".into(),
        }];
        allow(&db, Capability::HttpFetch, scope.clone());
        deny(&db, Capability::HttpFetch, scope);
        let outcome = PermissionEngine::new(&db)
            .check(&Request::new(Capability::HttpFetch).in_project("prj_1"))
            .unwrap();
        assert!(
            matches!(outcome, CheckOutcome::Denied { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn the_emergency_stop_overrides_every_grant() {
        let db = engine_db();
        allow(&db, Capability::KnowledgeSearch, vec![Scope::Global]);
        let engine = PermissionEngine::new(&db);
        assert!(matches!(
            engine
                .check(&Request::new(Capability::KnowledgeSearch))
                .unwrap(),
            CheckOutcome::Allowed { .. }
        ));

        engine.set_emergency_stop(true).unwrap();
        for capability in Capability::ALL {
            assert_eq!(
                engine.check(&Request::new(capability)).unwrap(),
                CheckOutcome::Stopped,
                "{capability} must be stopped"
            );
        }

        engine.set_emergency_stop(false).unwrap();
        assert!(matches!(
            engine
                .check(&Request::new(Capability::KnowledgeSearch))
                .unwrap(),
            CheckOutcome::Allowed { .. }
        ));
    }

    #[test]
    fn revoking_a_grant_takes_effect_on_the_next_check() {
        let db = engine_db();
        let grant = allow(&db, Capability::FileRead, vec![Scope::Global]);
        let engine = PermissionEngine::new(&db);
        assert!(matches!(
            engine.check(&Request::new(Capability::FileRead)).unwrap(),
            CheckOutcome::Allowed { .. }
        ));

        PermissionRepo::new(&db).revoke(&grant.id).unwrap();
        assert!(matches!(
            engine.check(&Request::new(Capability::FileRead)).unwrap(),
            CheckOutcome::NeedsApproval { .. }
        ));
    }

    #[test]
    fn a_single_use_grant_is_spent_after_one_action() {
        let db = engine_db();
        PermissionRepo::new(&db)
            .grant(NewGrant {
                capability: Capability::FileWrite,
                scopes: vec![Scope::Global],
                decision: Decision::AllowOnce,
                spend_limit_minor: None,
                spend_category: None,
                expires_at: None,
                created_by: "user".into(),
                note: None,
            })
            .unwrap();
        let engine = PermissionEngine::new(&db);

        assert!(matches!(
            engine
                .check_and_consume(&Request::new(Capability::FileWrite))
                .unwrap(),
            CheckOutcome::Allowed { .. }
        ));
        assert!(matches!(
            engine
                .check_and_consume(&Request::new(Capability::FileWrite))
                .unwrap(),
            CheckOutcome::NeedsApproval { .. }
        ));
    }

    #[test]
    fn checking_without_consuming_does_not_spend_a_single_use_grant() {
        let db = engine_db();
        PermissionRepo::new(&db)
            .grant(NewGrant {
                capability: Capability::FileWrite,
                scopes: vec![Scope::Global],
                decision: Decision::AllowOnce,
                spend_limit_minor: None,
                spend_category: None,
                expires_at: None,
                created_by: "user".into(),
                note: None,
            })
            .unwrap();
        let engine = PermissionEngine::new(&db);
        engine.check(&Request::new(Capability::FileWrite)).unwrap();
        assert!(matches!(
            engine.check(&Request::new(Capability::FileWrite)).unwrap(),
            CheckOutcome::Allowed { .. }
        ));
    }

    #[test]
    fn an_expired_grant_no_longer_allows_anything() {
        let db = engine_db();
        PermissionRepo::new(&db)
            .grant(NewGrant {
                capability: Capability::RelaySync,
                scopes: vec![Scope::Global],
                decision: Decision::Allow,
                spend_limit_minor: None,
                spend_category: None,
                expires_at: Some(otwono_types::now() - chrono::Duration::seconds(1)),
                created_by: "user".into(),
                note: None,
            })
            .unwrap();
        assert!(matches!(
            PermissionEngine::new(&db)
                .check(&Request::new(Capability::RelaySync))
                .unwrap(),
            CheckOutcome::NeedsApproval { .. }
        ));
    }

    #[test]
    fn a_grant_naming_two_scopes_needs_both_to_be_present() {
        let db = engine_db();
        allow(
            &db,
            Capability::FileRead,
            vec![
                Scope::Project {
                    project_id: "prj_1".into(),
                },
                Scope::Path {
                    path: "/home/u/docs".into(),
                },
            ],
        );
        let engine = PermissionEngine::new(&db);

        assert!(matches!(
            engine
                .check(
                    &Request::new(Capability::FileRead)
                        .in_project("prj_1")
                        .on_path("/home/u/docs/a.txt")
                )
                .unwrap(),
            CheckOutcome::Allowed { .. }
        ));
        assert!(
            matches!(
                engine
                    .check(&Request::new(Capability::FileRead).in_project("prj_1"))
                    .unwrap(),
                CheckOutcome::NeedsApproval { .. }
            ),
            "the path half of the grant is unmet"
        );
    }

    #[test]
    fn an_agents_policy_can_demand_confirmation_beyond_a_standing_grant() {
        use otwono_types::agent::ApprovalPolicy;
        assert!(PermissionEngine::policy_requires_confirmation(
            ApprovalPolicy::Always,
            Capability::KnowledgeSearch
        ));
        assert!(PermissionEngine::policy_requires_confirmation(
            ApprovalPolicy::OffDeviceOnly,
            Capability::HttpFetch
        ));
        assert!(!PermissionEngine::policy_requires_confirmation(
            ApprovalPolicy::OffDeviceOnly,
            Capability::KnowledgeSearch
        ));
        assert!(!PermissionEngine::policy_requires_confirmation(
            ApprovalPolicy::Standing,
            Capability::HttpFetch
        ));
    }

    #[test]
    fn approval_summaries_name_the_agent_and_the_target() {
        let summary = PermissionEngine::summarise(
            Capability::FileRead,
            &[Scope::Path {
                path: "/home/u/docs".into(),
            }],
            Some("Researcher"),
        );
        assert_eq!(
            summary,
            "Researcher wants to read a file you have authorised in /home/u/docs."
        );

        let bare = PermissionEngine::summarise(Capability::KnowledgeSearch, &[], None);
        assert_eq!(bare, "OTWONO wants to search your local knowledge index.");
    }
}

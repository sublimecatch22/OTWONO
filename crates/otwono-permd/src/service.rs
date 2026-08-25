//! The broker's control-plane surface.

use crate::action::ActionRegistry;
use crate::audit::{AuditEntry, AuditLog};
use crate::confirm::{ConfirmError, PendingStore, DEFAULT_TTL_MS};
use crate::policy::{Decision, Policy};
use crate::token::{now_unix_ms, random_id, TokenStore};
use otwono_proto::{unknown_method, CallContext, MethodDescription, RpcError, Service, ServiceDescription};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

pub const SERVICE_NAME: &str = "otwono-permd";
pub const DESCRIBE_SCHEMA_VERSION: &str = "1.0.0";

/// State shared by the two surfaces the broker presents.
///
/// There are two because ADR-0024 §3 needs them on **different sockets**: the control-plane
/// socket every daemon must reach cannot also be the socket only a person may reach, or the
/// rule about who may answer has nothing to stand on. They share this.
struct Core {
    registry: ActionRegistry,
    policy: Policy,
    tokens: Arc<TokenStore>,
    audit: AuditLog,
    /// Requests waiting for a person (ADR-0024).
    pending: PendingStore,
    /// Who may answer them (ADR-0024 §3a).
    ///
    /// Empty by default, so an unconfigured node confirms nothing. That is the same
    /// fail-closed state as before the channel existed, reached honestly rather than by
    /// having no mechanism — and it is what keeps an agent out: an agent's subject is simply
    /// never in this list.
    confirmers: Vec<String>,
}

impl Core {
    /// Write an audit record. A failure here fails the request: an action nobody can prove
    /// happened is worse than an action that did not happen.
    ///
    /// On `Core` rather than on either surface: both of them audit, and a confirmation flow
    /// that recorded the request but not who approved it would remove the only evidence
    /// that the human step happened at all (ADR-0024 §7).
    fn record(&self, entry: AuditEntry) -> Result<(), RpcError> {
        self.audit
            .append(entry)
            .map(|_| ())
            .map_err(|e| RpcError::internal(format!("cannot write the audit log, refusing to proceed: {e}")))
    }
}

/// The control-plane surface: `perm.*`. Bound on `/run/otwono/perm.sock`.
pub struct Broker {
    core: Arc<Core>,
}

/// The confirmation surface: `confirm.*`. Bound on `/run/otwono/confirm.sock`, which is
/// where its security comes from — see `ADR-0024` §3 and §4.
pub struct ConfirmService {
    core: Arc<Core>,
}

/// Claim an approved confirmation, on behalf of the subject that asked for it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimParams {
    confirmation_id: String,
}

/// Approve or deny one pending confirmation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecideParams {
    confirmation_id: String,
}

#[derive(Debug, Deserialize)]
struct RequestParams {
    action: String,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VerifyParams {
    token: String,
    action: String,
    #[serde(default)]
    resource: Option<String>,
    /// The subject the calling service observed. Supplying it binds the token to that
    /// caller; omitting it skips the check, which a service should only do if it genuinely
    /// cannot know its caller.
    #[serde(default)]
    subject: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TailParams {
    #[serde(default = "default_tail")]
    limit: usize,
}

fn default_tail() -> usize {
    50
}

impl Broker {
    pub fn new(policy: Policy, audit: AuditLog) -> Self {
        Broker {
            core: Arc::new(Core {
                registry: ActionRegistry::builtin(),
                policy,
                tokens: Arc::new(TokenStore::new()),
                audit,
                pending: PendingStore::new(),
                confirmers: Vec::new(),
            }),
        }
    }

    pub fn with_registry(self, registry: ActionRegistry) -> Self {
        // Rebuilt rather than mutated: the core is shared with the confirmation surface as
        // soon as `confirmations()` is called, and a registry that could change underneath
        // it would mean the two disagreed about what an action is.
        let core = Arc::try_unwrap(self.core)
            .unwrap_or_else(|_| panic!("with_registry must be called before confirmations()"));
        Broker {
            core: Arc::new(Core { registry, ..core }),
        }
    }

    /// Designate who may answer confirmations (ADR-0024 §3a).
    ///
    /// Subjects as the control plane spells them, e.g. `uid:1000`. Must be called before
    /// `confirmations()`, for the same reason `with_registry` must: the core is shared with
    /// the confirmation surface, and a set that could change underneath it would mean the
    /// two disagreed about who is allowed to answer.
    pub fn with_confirmers(self, confirmers: Vec<String>) -> Self {
        let core = Arc::try_unwrap(self.core)
            .unwrap_or_else(|_| panic!("with_confirmers must be called before confirmations()"));
        Broker {
            core: Arc::new(Core { confirmers, ..core }),
        }
    }

    /// The confirmation surface, for binding on its own socket.
    ///
    /// ADR-0024 §3: this must not be reachable on the control-plane socket. Handing it out
    /// as a separate `Service` is what makes that a property of the wiring rather than a
    /// rule someone has to remember.
    pub fn confirmations(&self) -> ConfirmService {
        ConfirmService {
            core: Arc::clone(&self.core),
        }
    }

    pub fn tokens(&self) -> &TokenStore {
        &self.core.tokens
    }

    pub fn registry(&self) -> &ActionRegistry {
        &self.core.registry
    }

    pub fn audit_path(&self) -> &Path {
        self.core.audit.path()
    }

    fn handle_request(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        let p: RequestParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("perm.request: {e}")))?;
        let subject = ctx.peer.subject();

        let Some(spec) = self.core.registry.get(&p.action) else {
            self.core.record(AuditEntry {
                event: "request".into(),
                subject: subject.clone(),
                action: p.action.clone(),
                resource: p.resource.clone(),
                outcome: "unknown_action".into(),
                reason: "not in the action registry".into(),
            })?;
            return Err(RpcError::invalid_params(format!("unknown action: {}", p.action)));
        };

        let evaluation = self.core.policy.evaluate(spec, &subject, p.resource.as_deref());
        let outcome = match evaluation.decision {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::Ask => "ask",
        };

        self.core.record(AuditEntry {
            event: "request".into(),
            subject: subject.clone(),
            action: p.action.clone(),
            resource: p.resource.clone(),
            outcome: outcome.into(),
            reason: match &p.reason {
                Some(r) => format!("{}; caller reason: {r}", evaluation.reason),
                None => evaluation.reason.clone(),
            },
        })?;

        match evaluation.decision {
            Decision::Deny => Err(RpcError::forbidden(format!(
                "policy denies {} for {subject}: {}",
                p.action, evaluation.reason
            ))),
            // ADR-0024: open a pending confirmation and hand back its id. Still an error
            // -- an unconfirmed action must not proceed just because nobody is available to
            // say no -- but now a recoverable one: the caller comes back with perm.claim
            // once somebody has answered, and nothing here blocks in the meantime.
            Decision::Ask => {
                let id = random_id();
                let opened = self
                    .core
                    .pending
                    .open(
                        id.clone(),
                        subject.clone(),
                        p.action.clone(),
                        p.resource.clone(),
                        p.reason.clone(),
                        now_unix_ms(),
                        DEFAULT_TTL_MS,
                    )
                    .map_err(|e| RpcError::unavailable(e.to_string()))?;
                self.core.record(AuditEntry {
                    event: "confirmation_opened".into(),
                    subject,
                    action: p.action.clone(),
                    resource: p.resource.clone(),
                    outcome: "pending".into(),
                    reason: format!("confirmation {id}, expires {}", opened.expires_unix_ms),
                })?;
                Err(RpcError::confirmation_required(format!(
                    "{} requires confirmation from the user: {}. Confirmation {id} is \
                     waiting; claim it with perm.claim once somebody has answered",
                    p.action, spec.summary
                )))
            }
            Decision::Allow => {
                let issued = self.core.tokens.issue(
                    &subject,
                    &p.action,
                    p.resource.as_deref(),
                    evaluation.ttl_seconds,
                    evaluation.one_shot,
                );
                self.core.record(AuditEntry {
                    event: "token_issued".into(),
                    subject,
                    action: p.action.clone(),
                    resource: p.resource.clone(),
                    outcome: "issued".into(),
                    reason: format!(
                        "ttl {}s, one_shot {}",
                        evaluation.ttl_seconds, evaluation.one_shot
                    ),
                })?;
                serde_json::to_value(issued).map_err(|e| RpcError::internal(e.to_string()))
            }
        }
    }

    fn handle_verify(&self, params: Value) -> Result<Value, RpcError> {
        let p: VerifyParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("perm.verify: {e}")))?;

        match self
            .core
            .tokens
            .verify(&p.token, &p.action, p.resource.as_deref(), p.subject.as_deref())
        {
            Ok(grant) => {
                self.core.record(AuditEntry {
                    event: "token_verified".into(),
                    subject: grant.subject.clone(),
                    action: grant.action.clone(),
                    resource: grant.resource.clone(),
                    outcome: "valid".into(),
                    reason: "token accepted".into(),
                })?;
                Ok(json!({
                    "subject": grant.subject,
                    "action": grant.action,
                    "resource": grant.resource,
                }))
            }
            Err(e) => {
                self.core.record(AuditEntry {
                    event: "token_rejected".into(),
                    subject: p.subject.clone().unwrap_or_else(|| "unknown".into()),
                    action: p.action.clone(),
                    resource: p.resource.clone(),
                    outcome: "invalid".into(),
                    reason: e.to_string(),
                })?;
                Err(RpcError::unauthorized(e.to_string()))
            }
        }
    }

    fn handle_audit_verify(&self) -> Result<Value, RpcError> {
        let report = AuditLog::verify(self.core.audit.path())
            .map_err(|e| RpcError::internal(format!("cannot verify the audit log: {e}")))?;
        serde_json::to_value(report).map_err(|e| RpcError::internal(e.to_string()))
    }

    /// Reading the audit log is itself guarded: it records what every subject on the
    /// machine has done.
    fn handle_audit_tail(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        let p: TailParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("perm.audit.tail: {e}")))?;
        let subject = ctx.peer.subject();
        let token = ctx
            .capability
            .as_deref()
            .ok_or_else(|| RpcError::unauthorized("perm.audit.tail requires the audit.read capability"))?;
        self.core
            .tokens
            .verify(token, "audit.read", None, Some(&subject))
            .map_err(|e| RpcError::unauthorized(e.to_string()))?;

        let text = std::fs::read_to_string(self.core.audit.path()).unwrap_or_default();
        let lines: Vec<Value> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let start = lines.len().saturating_sub(p.limit);
        Ok(json!({ "records": &lines[start..] }))
    }
}

/// Turn a confirmation refusal into the right kind of RPC error.
///
/// The distinction matters to a caller deciding what to do next: `StillPending` is "come
/// back", everything else is "this is over".
fn confirm_rpc(e: ConfirmError) -> RpcError {
    match e {
        ConfirmError::StillPending => RpcError::confirmation_required(e.to_string()),
        ConfirmError::TooMany => RpcError::unavailable(e.to_string()),
        ConfirmError::Unknown => RpcError::invalid_params(e.to_string()),
        _ => RpcError::forbidden(e.to_string()),
    }
}

impl Broker {
    /// Consume an approved confirmation and issue the token it authorised.
    ///
    /// The subject is taken from the connection's peer credentials, never from the
    /// parameters: a caller that could name its own subject could claim somebody else's
    /// approval, which is the whole thing this flow exists to prevent.
    fn handle_claim(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        let p: ClaimParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("perm.claim: {e}")))?;
        let subject = ctx.peer.subject();
        let now = now_unix_ms();

        let claimed = match self.core.pending.claim(&p.confirmation_id, &subject, now) {
            Ok(c) => c,
            Err(e) => {
                // Recorded even when it fails. A claim against a denied or expired
                // confirmation is exactly the event an audit reader wants to find.
                self.core.record(AuditEntry {
                    event: "confirmation_claim".into(),
                    subject,
                    action: "perm.claim".into(),
                    resource: Some(p.confirmation_id.clone()),
                    outcome: "refused".into(),
                    reason: e.to_string(),
                })?;
                return Err(confirm_rpc(e));
            }
        };

        // Re-evaluate rather than trusting the pending record. Policy may have changed
        // while a person was deciding, and a confirmation authorises a person's consent --
        // it does not override a rule that now says deny.
        let spec = self
            .core
            .registry
            .get(&claimed.action)
            .ok_or_else(|| RpcError::invalid_params(format!("unknown action {}", claimed.action)))?;
        let evaluation = self
            .core
            .policy
            .evaluate(spec, &claimed.subject, claimed.resource.as_deref());
        if evaluation.decision == Decision::Deny {
            self.core.record(AuditEntry {
                event: "confirmation_claim".into(),
                subject: claimed.subject.clone(),
                action: claimed.action.clone(),
                resource: claimed.resource.clone(),
                outcome: "denied".into(),
                reason: format!("policy changed while pending: {}", evaluation.reason),
            })?;
            return Err(RpcError::forbidden(format!(
                "{} was confirmed, but policy now denies it: {}",
                claimed.action, evaluation.reason
            )));
        }

        let issued = self.core.tokens.issue(
            &claimed.subject,
            &claimed.action,
            claimed.resource.as_deref(),
            evaluation.ttl_seconds,
            // A confirmed action is one-shot regardless of what the rule says. The person
            // agreed to one thing happening once; a reusable token would let it happen
            // again without them.
            true,
        );
        self.core.record(AuditEntry {
            event: "token_issued".into(),
            subject: claimed.subject.clone(),
            action: claimed.action.clone(),
            resource: claimed.resource.clone(),
            outcome: "issued".into(),
            reason: format!(
                "confirmation {} approved by {}; ttl {}s, one_shot true",
                claimed.id,
                claimed.decided_by.as_deref().unwrap_or("unknown"),
                evaluation.ttl_seconds
            ),
        })?;
        serde_json::to_value(issued).map_err(|e| RpcError::internal(e.to_string()))
    }
}

impl ConfirmService {
    /// Everything waiting for a person.
    fn handle_confirm_list(&self) -> Result<Value, RpcError> {
        let now = now_unix_ms();
        let pending: Vec<Value> = self
            .core
            .pending
            .list(now)
            .into_iter()
            .map(|p| {
                // The registered summary and blast radius travel with it: "irreversible" is
                // the word that changes an answer, and a confirmer reading only an action
                // name is deciding on less than the registry knows.
                let (summary, blast) = match self.core.registry.get(&p.action) {
                    Some(s) => (s.summary.clone(), format!("{:?}", s.blast_radius)),
                    None => (String::new(), String::new()),
                };
                json!({
                    "confirmation_id": p.id,
                    "subject": p.subject,
                    "action": p.action,
                    "summary": summary,
                    "blast_radius": blast,
                    "resource": p.resource,
                    // Presented as what it is: the asker's own words, not an explanation.
                    "caller_claims": p.reason,
                    "age_ms": p.age_ms(now),
                    "expires_unix_ms": p.expires_unix_ms,
                })
            })
            .collect();
        Ok(json!({
            "pending": pending,
            "note": "caller_claims is a string chosen by whatever asked for the permission. \
                     It is an assertion, not a fact, and may come from an agent acting on \
                     content it did not write",
        }))
    }

    fn handle_confirm_decide(
        &self,
        ctx: &CallContext,
        params: Value,
        approve: bool,
    ) -> Result<Value, RpcError> {
        let p: DecideParams =
            serde_json::from_value(params).map_err(|e| RpcError::invalid_params(format!("confirm: {e}")))?;
        let by = ctx.peer.subject();
        let now = now_unix_ms();

        match self
            .core
            .pending
            .decide(&p.confirmation_id, &by, &self.core.confirmers, approve, now)
        {
            Ok(decided) => {
                self.core.record(AuditEntry {
                    event: "confirmation_decided".into(),
                    subject: decided.subject.clone(),
                    action: decided.action.clone(),
                    resource: decided.resource.clone(),
                    outcome: if approve { "approved" } else { "denied" }.into(),
                    reason: format!("confirmation {} decided by {by}", decided.id),
                })?;
                Ok(json!({
                    "confirmation_id": decided.id,
                    "state": decided.state,
                    "decided_by": by,
                }))
            }
            Err(e) => {
                // A refused answer is the one an audit reader most wants to see: it means
                // something not designated as a confirmer tried to approve an action.
                self.core.record(AuditEntry {
                    event: "confirmation_decided".into(),
                    subject: by.clone(),
                    action: "confirm.decide".into(),
                    resource: Some(p.confirmation_id.clone()),
                    outcome: "refused".into(),
                    reason: e.to_string(),
                })?;
                Err(confirm_rpc(e))
            }
        }
    }
}

impl Service for ConfirmService {
    fn describe(&self) -> ServiceDescription {
        ServiceDescription {
            schema_version: DESCRIBE_SCHEMA_VERSION.to_string(),
            service: "otwono-permd-confirm".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            methods: vec![
                MethodDescription::open("confirm.list", "What is waiting for a person to approve or deny"),
                MethodDescription::open(
                    "confirm.approve",
                    "Approve one pending confirmation. Refused to the subject that asked",
                ),
                MethodDescription::open("confirm.deny", "Deny one pending confirmation"),
            ],
        }
    }

    fn call(&self, ctx: &CallContext, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "confirm.list" => self.handle_confirm_list(),
            "confirm.approve" => self.handle_confirm_decide(ctx, params, true),
            "confirm.deny" => self.handle_confirm_decide(ctx, params, false),
            // perm.* is deliberately absent. This socket exists to be reachable by a person
            // and by nothing else; serving the request path here would hand whatever can
            // reach it the ability to ask as well as to answer.
            other => Err(unknown_method(other)),
        }
    }
}

impl Service for Broker {
    fn describe(&self) -> ServiceDescription {
        ServiceDescription {
            schema_version: DESCRIBE_SCHEMA_VERSION.to_string(),
            service: SERVICE_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            methods: vec![
                MethodDescription::open("perm.actions", "List the registered typed actions"),
                MethodDescription::open(
                    "perm.claim",
                    "Collect the token an approved confirmation authorised (ADR-0024)",
                ),
                MethodDescription::open(
                    "perm.request",
                    "Ask for a capability token; the policy decision is the control, so this method is open",
                ),
                MethodDescription::open(
                    "perm.verify",
                    "Verify a capability token on behalf of another service",
                ),
                MethodDescription::open("perm.audit.verify", "Check the audit log's hash chain end to end"),
                MethodDescription::guarded("perm.audit.tail", "Read recent audit records", "audit.read"),
            ],
        }
    }

    fn call(&self, ctx: &CallContext, method: &str, params: Value) -> Result<Value, RpcError> {
        // Opportunistic cleanup; cheap and keeps the token map bounded on a long-lived node.
        self.core.tokens.purge_expired_at(now_unix_ms());

        match method {
            "perm.actions" => serde_json::to_value(json!({ "actions": self.core.registry.all() }))
                .map_err(|e| RpcError::internal(e.to_string())),
            "perm.request" => self.handle_request(ctx, params),
            // ADR-0024. perm.claim is for the subject that asked; the confirm.* pair is for
            // whoever answers. They are separated here for clarity, and separated *in
            // reachability* by the socket each is served on -- see otwono-permd's main,
            // which binds the confirmation socket with different permissions.
            "perm.claim" => self.handle_claim(ctx, params),
            "perm.verify" => self.handle_verify(params),
            "perm.audit.verify" => self.handle_audit_verify(),
            "perm.audit.tail" => self.handle_audit_tail(ctx, params),
            other => Err(unknown_method(other)),
        }
    }
}

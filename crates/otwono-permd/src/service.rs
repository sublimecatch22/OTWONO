//! The broker's control-plane surface.

use crate::action::ActionRegistry;
use crate::audit::{AuditEntry, AuditLog};
use crate::policy::{Decision, Policy};
use crate::token::{now_unix_ms, TokenStore};
use otwono_proto::{unknown_method, CallContext, MethodDescription, RpcError, Service, ServiceDescription};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

pub const SERVICE_NAME: &str = "otwono-permd";
pub const DESCRIBE_SCHEMA_VERSION: &str = "1.0.0";

pub struct Broker {
    registry: ActionRegistry,
    policy: Policy,
    tokens: Arc<TokenStore>,
    audit: AuditLog,
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
            registry: ActionRegistry::builtin(),
            policy,
            tokens: Arc::new(TokenStore::new()),
            audit,
        }
    }

    pub fn with_registry(mut self, registry: ActionRegistry) -> Self {
        self.registry = registry;
        self
    }

    pub fn tokens(&self) -> &TokenStore {
        &self.tokens
    }

    pub fn registry(&self) -> &ActionRegistry {
        &self.registry
    }

    pub fn audit_path(&self) -> &Path {
        self.audit.path()
    }

    /// Write an audit record. A failure here fails the request: an action nobody can prove
    /// happened is worse than an action that did not happen.
    fn record(&self, entry: AuditEntry) -> Result<(), RpcError> {
        self.audit
            .append(entry)
            .map(|_| ())
            .map_err(|e| RpcError::internal(format!("cannot write the audit log, refusing to proceed: {e}")))
    }

    fn handle_request(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        let p: RequestParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("perm.request: {e}")))?;
        let subject = ctx.peer.subject();

        let Some(spec) = self.registry.get(&p.action) else {
            self.record(AuditEntry {
                event: "request".into(),
                subject: subject.clone(),
                action: p.action.clone(),
                resource: p.resource.clone(),
                outcome: "unknown_action".into(),
                reason: "not in the action registry".into(),
            })?;
            return Err(RpcError::invalid_params(format!("unknown action: {}", p.action)));
        };

        let evaluation = self.policy.evaluate(spec, &subject, p.resource.as_deref());
        let outcome = match evaluation.decision {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::Ask => "ask",
        };

        self.record(AuditEntry {
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
            // No confirmation channel exists yet (Phase 7). Returning an error is the
            // fail-closed answer: an unconfirmed action must not proceed just because
            // nobody is available to say no.
            Decision::Ask => Err(RpcError::confirmation_required(format!(
                "{} requires confirmation from the user: {}",
                p.action, spec.summary
            ))),
            Decision::Allow => {
                let issued = self.tokens.issue(
                    &subject,
                    &p.action,
                    p.resource.as_deref(),
                    evaluation.ttl_seconds,
                    evaluation.one_shot,
                );
                self.record(AuditEntry {
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
            .tokens
            .verify(&p.token, &p.action, p.resource.as_deref(), p.subject.as_deref())
        {
            Ok(grant) => {
                self.record(AuditEntry {
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
                self.record(AuditEntry {
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
        let report = AuditLog::verify(self.audit.path())
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
        self.tokens
            .verify(token, "audit.read", None, Some(&subject))
            .map_err(|e| RpcError::unauthorized(e.to_string()))?;

        let text = std::fs::read_to_string(self.audit.path()).unwrap_or_default();
        let lines: Vec<Value> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let start = lines.len().saturating_sub(p.limit);
        Ok(json!({ "records": &lines[start..] }))
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
        self.tokens.purge_expired_at(now_unix_ms());

        match method {
            "perm.actions" => serde_json::to_value(json!({ "actions": self.registry.all() }))
                .map_err(|e| RpcError::internal(e.to_string())),
            "perm.request" => self.handle_request(ctx, params),
            "perm.verify" => self.handle_verify(params),
            "perm.audit.verify" => self.handle_audit_verify(),
            "perm.audit.tail" => self.handle_audit_tail(ctx, params),
            other => Err(unknown_method(other)),
        }
    }
}

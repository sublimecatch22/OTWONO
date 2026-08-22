//! Declarative policy: who may perform which action on which resource.
//!
//! Policy is human-readable and diffable TOML in `/etc/otwono/policy.d/`. It is loaded in
//! filename order and the **first matching rule wins**, so a site can drop in a
//! `10-local.toml` ahead of the shipped defaults without editing them.
//!
//! The default is `deny`. A policy set that matches nothing refuses everything, which is
//! the only safe way for a security component to fail.

use crate::action::{ActionRegistry, ActionSpec};
use glob::{MatchOptions, Pattern};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
    /// A human must confirm. The broker issues no token until they do.
    Ask,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    /// Action id, or a `*` glob over action ids.
    pub action: String,
    /// Subjects this rule covers, e.g. `uid:0`. Absent means any subject.
    #[serde(default)]
    pub subjects: Vec<String>,
    /// Resource glob. Absent means the rule ignores the resource.
    #[serde(default)]
    pub resource: Option<String>,
    pub decision: Decision,
    /// Token lifetime when this rule allows. Defaults to `DEFAULT_TTL_SECONDS`.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    /// Issue a token good for exactly one call.
    #[serde(default)]
    pub one_shot: Option<bool>,
}

pub const DEFAULT_TTL_SECONDS: u64 = 300;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicyFile {
    #[serde(default)]
    pub rule: Vec<Rule>,
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    rules: Vec<Rule>,
}

/// The outcome of evaluating a request, with the reason attached.
///
/// The reason is not decoration: an operator has to be able to answer "why was this
/// allowed?" from the audit log alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    pub decision: Decision,
    pub reason: String,
    pub ttl_seconds: u64,
    pub one_shot: bool,
}

impl Policy {
    pub fn new(rules: Vec<Rule>) -> Self {
        Policy { rules }
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Load every `*.toml` under `dir`, in filename order. A missing directory yields an
    /// empty policy, which denies everything.
    pub fn load_dir(dir: &Path) -> Result<Self, PolicyError> {
        let mut files: Vec<_> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "toml"))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Policy::default()),
            Err(e) => return Err(PolicyError::Io(format!("{}: {e}", dir.display()))),
        };
        files.sort();

        let mut rules = Vec::new();
        for f in files {
            let text =
                std::fs::read_to_string(&f).map_err(|e| PolicyError::Io(format!("{}: {e}", f.display())))?;
            let parsed: PolicyFile =
                toml::from_str(&text).map_err(|e| PolicyError::Parse(format!("{}: {e}", f.display())))?;
            rules.extend(parsed.rule);
        }
        Ok(Policy::new(rules))
    }

    /// Reject rules naming actions this build does not know about.
    ///
    /// Without this, a typo in a policy file silently becomes a rule that never matches —
    /// and the operator believes they granted something they did not.
    pub fn validate(&self, registry: &ActionRegistry) -> Result<(), PolicyError> {
        for r in &self.rules {
            if r.action.contains('*') {
                continue;
            }
            if registry.get(&r.action).is_none() {
                return Err(PolicyError::UnknownAction(r.action.clone()));
            }
        }
        Ok(())
    }

    /// Evaluate a request. First matching rule wins; no match means deny.
    pub fn evaluate(&self, spec: &ActionSpec, subject: &str, resource: Option<&str>) -> Evaluation {
        for (i, rule) in self.rules.iter().enumerate() {
            if !glob_matches(&rule.action, &spec.id) {
                continue;
            }
            if !rule.subjects.is_empty() && !rule.subjects.iter().any(|s| s == subject) {
                continue;
            }
            if let Some(pattern) = &rule.resource {
                match resource {
                    Some(r) if glob_matches(pattern, r) => {}
                    _ => continue,
                }
            }

            let decision = if spec.always_confirm && rule.decision == Decision::Allow {
                // Policy may not clear an intrinsic confirmation requirement.
                Decision::Ask
            } else {
                rule.decision
            };

            let reason = if decision != rule.decision {
                format!(
                    "rule {i} says {:?}, but action {} always requires confirmation",
                    rule.decision, spec.id
                )
            } else {
                format!(
                    "rule {i} ({} on {})",
                    rule.action,
                    rule.resource.as_deref().unwrap_or("*")
                )
            };

            return Evaluation {
                decision,
                reason,
                ttl_seconds: rule.ttl_seconds.unwrap_or(DEFAULT_TTL_SECONDS),
                one_shot: rule.one_shot.unwrap_or(matches!(
                    spec.blast_radius,
                    crate::action::BlastRadius::Irreversible | crate::action::BlastRadius::Egress
                )),
            };
        }

        Evaluation {
            decision: Decision::Deny,
            reason: "no rule matched; default is deny".to_string(),
            ttl_seconds: 0,
            one_shot: true,
        }
    }
}

/// Glob match where `*` does not cross a `/` but `**` does. Matches the semantics an
/// operator expects when writing `/home/*/Documents` versus `/home/**`.
fn glob_matches(pattern: &str, value: &str) -> bool {
    match Pattern::new(pattern) {
        Ok(p) => p.matches_with(
            value,
            MatchOptions {
                case_sensitive: true,
                require_literal_separator: true,
                require_literal_leading_dot: false,
            },
        ),
        // A malformed pattern must never match. Failing open here would be a hole.
        Err(_) => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyError {
    Io(String),
    Parse(String),
    UnknownAction(String),
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::Io(e) => write!(f, "cannot read policy: {e}"),
            PolicyError::Parse(e) => write!(f, "malformed policy: {e}"),
            PolicyError::UnknownAction(a) => {
                write!(f, "policy names an action this build does not define: {a}")
            }
        }
    }
}

impl std::error::Error for PolicyError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> ActionRegistry {
        ActionRegistry::builtin()
    }

    fn spec(id: &str) -> ActionSpec {
        registry().get(id).unwrap().clone()
    }

    #[test]
    fn an_empty_policy_denies_everything() {
        let p = Policy::default();
        let e = p.evaluate(&spec("hw.read"), "uid:0", None);
        assert_eq!(e.decision, Decision::Deny);
        assert!(e.reason.contains("default is deny"));
    }

    #[test]
    fn a_matching_allow_rule_grants() {
        let p = Policy::new(vec![Rule {
            action: "hw.read".into(),
            subjects: vec!["uid:1000".into()],
            resource: None,
            decision: Decision::Allow,
            ttl_seconds: Some(60),
            one_shot: None,
        }]);
        let e = p.evaluate(&spec("hw.read"), "uid:1000", None);
        assert_eq!(e.decision, Decision::Allow);
        assert_eq!(e.ttl_seconds, 60);
    }

    #[test]
    fn a_rule_for_another_subject_does_not_match() {
        let p = Policy::new(vec![Rule {
            action: "hw.read".into(),
            subjects: vec!["uid:1000".into()],
            resource: None,
            decision: Decision::Allow,
            ttl_seconds: None,
            one_shot: None,
        }]);
        assert_eq!(
            p.evaluate(&spec("hw.read"), "uid:1001", None).decision,
            Decision::Deny
        );
    }

    #[test]
    fn policy_cannot_clear_an_intrinsic_confirmation_requirement() {
        // The load-bearing test for the whole model: an operator (or a compromised policy
        // file) writing `allow` on a destructive action gets `ask`, not `allow`.
        let p = Policy::new(vec![Rule {
            action: "fs.delete".into(),
            subjects: vec![],
            resource: None,
            decision: Decision::Allow,
            ttl_seconds: None,
            one_shot: None,
        }]);
        let e = p.evaluate(&spec("fs.delete"), "uid:0", Some("/home/u/x"));
        assert_eq!(e.decision, Decision::Ask);
        assert!(e.reason.contains("always requires confirmation"), "{}", e.reason);
    }

    #[test]
    fn a_deny_rule_stays_deny_even_for_a_confirm_action() {
        let p = Policy::new(vec![Rule {
            action: "net.egress".into(),
            subjects: vec![],
            resource: None,
            decision: Decision::Deny,
            ttl_seconds: None,
            one_shot: None,
        }]);
        assert_eq!(
            p.evaluate(&spec("net.egress"), "uid:0", None).decision,
            Decision::Deny
        );
    }

    #[test]
    fn first_matching_rule_wins() {
        let p = Policy::new(vec![
            Rule {
                action: "fs.read".into(),
                subjects: vec![],
                resource: Some("/etc/**".into()),
                decision: Decision::Deny,
                ttl_seconds: None,
                one_shot: None,
            },
            Rule {
                action: "fs.read".into(),
                subjects: vec![],
                resource: None,
                decision: Decision::Allow,
                ttl_seconds: None,
                one_shot: None,
            },
        ]);
        assert_eq!(
            p.evaluate(&spec("fs.read"), "uid:0", Some("/etc/shadow"))
                .decision,
            Decision::Deny
        );
        assert_eq!(
            p.evaluate(&spec("fs.read"), "uid:0", Some("/tmp/x")).decision,
            Decision::Allow
        );
    }

    #[test]
    fn single_star_does_not_cross_a_path_separator() {
        assert!(glob_matches("/home/*", "/home/alice"));
        assert!(!glob_matches("/home/*", "/home/alice/secrets"));
        assert!(glob_matches("/home/**", "/home/alice/secrets"));
    }

    #[test]
    fn a_malformed_glob_matches_nothing_rather_than_everything() {
        assert!(!glob_matches("[", "anything"));
    }

    #[test]
    fn a_resource_rule_does_not_match_a_request_without_a_resource() {
        let p = Policy::new(vec![Rule {
            action: "fs.read".into(),
            subjects: vec![],
            resource: Some("/tmp/**".into()),
            decision: Decision::Allow,
            ttl_seconds: None,
            one_shot: None,
        }]);
        assert_eq!(
            p.evaluate(&spec("fs.read"), "uid:0", None).decision,
            Decision::Deny
        );
    }

    #[test]
    fn egress_and_irreversible_actions_default_to_one_shot_tokens() {
        let p = Policy::new(vec![Rule {
            action: "*".into(),
            subjects: vec![],
            resource: None,
            decision: Decision::Deny,
            ttl_seconds: None,
            one_shot: None,
        }]);
        assert!(p.evaluate(&spec("net.egress"), "uid:0", None).one_shot);
        assert!(!p.evaluate(&spec("hw.read"), "uid:0", None).one_shot);
    }

    #[test]
    fn validation_rejects_a_typo_instead_of_silently_never_matching() {
        let p = Policy::new(vec![Rule {
            action: "hw.raed".into(),
            subjects: vec![],
            resource: None,
            decision: Decision::Allow,
            ttl_seconds: None,
            one_shot: None,
        }]);
        assert_eq!(
            p.validate(&registry()),
            Err(PolicyError::UnknownAction("hw.raed".into()))
        );
    }

    #[test]
    fn parses_a_realistic_policy_file() {
        let text = r#"
[[rule]]
action = "hw.read"
subjects = ["uid:0", "uid:1000"]
decision = "allow"
ttl_seconds = 120

[[rule]]
action = "fs.read"
resource = "/var/lib/otwono/**"
decision = "allow"
"#;
        let f: PolicyFile = toml::from_str(text).unwrap();
        assert_eq!(f.rule.len(), 2);
        assert_eq!(f.rule[0].decision, Decision::Allow);
        assert_eq!(f.rule[0].subjects.len(), 2);
        assert_eq!(f.rule[1].resource.as_deref(), Some("/var/lib/otwono/**"));
    }

    #[test]
    fn a_missing_policy_directory_denies_rather_than_erroring() {
        let p = Policy::load_dir(Path::new("/nonexistent/policy.d")).unwrap();
        assert!(p.rules().is_empty());
        assert_eq!(
            p.evaluate(&spec("hw.read"), "uid:0", None).decision,
            Decision::Deny
        );
    }
}

//! The typed action registry.
//!
//! Every privileged operation in OTWONO is a declared action. There is no generic
//! "do arbitrary thing" call, because a permission model whose action set is open-ended
//! cannot be reasoned about (docs/security/SECURITY-MODEL.md Section 2).
//!
//! An action's *intrinsic* properties live here, not in policy. Policy decides who may do
//! a thing; the registry decides what kind of thing it is. That split is what stops a
//! permissive policy file from quietly turning an irreversible action into a silent one.

use serde::{Deserialize, Serialize};

/// How much damage a successful call can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlastRadius {
    /// Reads something. No state changes.
    Read,
    /// Changes state that is easy to undo.
    Reversible,
    /// Changes state that cannot be undone from inside the system.
    Irreversible,
    /// Sends data off this node. Irreversible in the strongest sense — see
    /// docs/security/DATA-VISIBILITY.md: published data cannot be recalled.
    Egress,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionSpec {
    pub id: String,
    pub summary: String,
    pub blast_radius: BlastRadius,
    /// True when a human must confirm regardless of what policy says.
    ///
    /// Policy can never clear this. The list in SECURITY-MODEL.md Section 2 is not advisory:
    /// deleting user data, promoting a visibility label, sending data off-node, installing
    /// software and changing policy all require a person, whatever the rules file claims
    /// and whatever the agent concluded.
    pub always_confirm: bool,
}

impl ActionSpec {
    fn new(id: &str, summary: &str, blast_radius: BlastRadius, always_confirm: bool) -> Self {
        ActionSpec {
            id: id.to_string(),
            summary: summary.to_string(),
            blast_radius,
            always_confirm,
        }
    }
}

/// The set of actions this build knows about.
///
/// Phase 2 registers the handful the control plane itself needs. Each new subsystem adds
/// its actions here, and adding one is a security change that gets reviewed as such.
#[derive(Debug, Clone)]
pub struct ActionRegistry {
    actions: Vec<ActionSpec>,
}

impl Default for ActionRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}

impl ActionRegistry {
    pub fn builtin() -> Self {
        ActionRegistry {
            actions: vec![
                ActionSpec::new(
                    "hw.read",
                    "Read the hardware capability profile",
                    BlastRadius::Read,
                    false,
                ),
                ActionSpec::new(
                    "audit.read",
                    "Read the permission broker's audit log",
                    BlastRadius::Read,
                    false,
                ),
                ActionSpec::new("fs.read", "Read a file or directory", BlastRadius::Read, false),
                ActionSpec::new(
                    "id.sign",
                    "Sign a payload with this node's identity key",
                    BlastRadius::Reversible,
                    false,
                ),
                ActionSpec::new(
                    "id.sign_session",
                    "Sign one Noise handshake hash so otwono-netd can authenticate a peer",
                    BlastRadius::Reversible,
                    false,
                ),
                ActionSpec::new(
                    "id.bind_agreement",
                    "Vouch for an X25519 agreement key held by another daemon",
                    BlastRadius::Reversible,
                    false,
                ),
                ActionSpec::new(
                    "net.read",
                    "List the peers this node has met",
                    BlastRadius::Read,
                    false,
                ),
                ActionSpec::new(
                    "net.connect",
                    "Open an authenticated connection to a peer",
                    BlastRadius::Reversible,
                    false,
                ),
                ActionSpec::new(
                    "id.rotate",
                    "Replace this node's identity key, changing its NodeID",
                    BlastRadius::Irreversible,
                    true,
                ),
                ActionSpec::new(
                    "fs.write",
                    "Create or modify a file",
                    BlastRadius::Reversible,
                    false,
                ),
                ActionSpec::new("fs.delete", "Delete user data", BlastRadius::Irreversible, true),
                ActionSpec::new(
                    "label.promote",
                    "Make stored data more widely visible",
                    BlastRadius::Egress,
                    true,
                ),
                ActionSpec::new("net.egress", "Send data off this node", BlastRadius::Egress, true),
                ActionSpec::new(
                    "pkg.install",
                    "Install or remove software",
                    BlastRadius::Irreversible,
                    true,
                ),
                ActionSpec::new(
                    "policy.write",
                    "Change the permission policy itself",
                    BlastRadius::Irreversible,
                    true,
                ),
            ],
        }
    }

    pub fn get(&self, id: &str) -> Option<&ActionSpec> {
        self.actions.iter().find(|a| a.id == id)
    }

    pub fn all(&self) -> &[ActionSpec] {
        &self.actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_capabilities_never_demand_confirmation() {
        // These fire on every peer connection. If either could be set always_confirm, the
        // mesh would deadlock on a confirmation channel that does not exist yet.
        let r = ActionRegistry::builtin();
        for id in ["id.sign_session", "id.bind_agreement"] {
            let spec = r.get(id).unwrap_or_else(|| panic!("{id} must be registered"));
            assert!(!spec.always_confirm, "{id} runs unattended");
            assert_eq!(spec.blast_radius, BlastRadius::Reversible);
        }
    }

    #[test]
    fn signing_a_session_is_a_narrower_action_than_signing_anything() {
        // Separate actions so policy can grant the mesh what it needs without granting a
        // general signing oracle.
        let r = ActionRegistry::builtin();
        assert!(r.get("id.sign").is_some());
        assert!(r.get("id.sign_session").is_some());
        assert_ne!(
            r.get("id.sign").unwrap().summary,
            r.get("id.sign_session").unwrap().summary
        );
    }

    #[test]
    fn unknown_actions_are_not_invented() {
        let r = ActionRegistry::builtin();
        assert!(r.get("hw.read").is_some());
        assert!(r.get("definitely.not.an.action").is_none());
    }

    #[test]
    fn destructive_and_egress_actions_always_require_confirmation() {
        let r = ActionRegistry::builtin();
        for id in [
            "fs.delete",
            "label.promote",
            "net.egress",
            "pkg.install",
            "policy.write",
        ] {
            let spec = r.get(id).unwrap_or_else(|| panic!("{id} must be registered"));
            assert!(spec.always_confirm, "{id} must always require confirmation");
        }
    }

    #[test]
    fn read_actions_do_not_demand_confirmation() {
        let r = ActionRegistry::builtin();
        for id in ["hw.read", "audit.read", "fs.read"] {
            assert!(
                !r.get(id).unwrap().always_confirm,
                "{id} should not need confirmation"
            );
        }
    }

    #[test]
    fn signing_is_guarded_but_does_not_stop_for_confirmation() {
        // Signing is privileged — it makes the node's key vouch for something — but it is
        // a normal brokered call. Rotation is not: it changes the node's name and orphans
        // every peer relationship, so a person has to say yes.
        let r = ActionRegistry::builtin();
        assert!(!r.get("id.sign").unwrap().always_confirm);
        assert!(r.get("id.rotate").unwrap().always_confirm);
        assert_eq!(
            r.get("id.rotate").unwrap().blast_radius,
            BlastRadius::Irreversible
        );
    }

    #[test]
    fn blast_radius_is_ordered_so_policy_can_compare_it() {
        assert!(BlastRadius::Read < BlastRadius::Reversible);
        assert!(BlastRadius::Reversible < BlastRadius::Irreversible);
        assert!(BlastRadius::Irreversible < BlastRadius::Egress);
    }

    #[test]
    fn every_registered_action_has_a_summary() {
        for a in ActionRegistry::builtin().all() {
            assert!(
                !a.summary.is_empty(),
                "{} needs a summary for the confirmation prompt",
                a.id
            );
        }
    }
}

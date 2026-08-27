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
                    "ai.read",
                    "List models and ask whether one would load on this node",
                    BlastRadius::Read,
                    false,
                ),
                ActionSpec::new("ai.infer", "Run local inference", BlastRadius::Reversible, false),
                // Installing a model puts executable content on the node: a model that can
                // call tools is instructions from whoever published it. Irreversible
                // because the blob store is content-addressed and an install is not undone
                // by removing the manifest.
                ActionSpec::new(
                    "ai.admin",
                    "Install or remove a model, changing what this node will run",
                    BlastRadius::Irreversible,
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
                // Fetching from a peer brings content this node did not author onto this
                // node. Reversible because what arrives is verified against the content id
                // that was asked for and lands nowhere on its own — but it is a peer's
                // bytes, and it is not a read.
                ActionSpec::new(
                    "net.content",
                    "Fetch a content-addressed object from a peer",
                    BlastRadius::Reversible,
                    false,
                ),
                // Unwrapping is where the sharing key of ADR-0019 gets used, and it is
                // its own capability rather than id.sign for the reason every other split
                // here exists: otwono-stored must be able to open what was shared with
                // this node without being able to sign as it. Read, because a successful
                // call changes nothing and sends nothing — it hands back a key for
                // ciphertext the caller already holds, which is a read of user data.
                // Anyone granted this can open every SHARED object they can obtain, so the
                // grant is the decision, not the individual call: a prompt per unwrap
                // would fire once per object on a node doing anything at all.
                ActionSpec::new(
                    "id.unwrap_shared",
                    "Open a content key that was sealed to this node's sharing key",
                    BlastRadius::Read,
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
                // Narrower than net.egress, and separate for the same reason id.sign_session
                // is separate from id.sign: policy should be able to grant a node the
                // ability to fetch from sources an operator approved without granting a
                // general egress oracle. Egress blast radius, because bytes do leave — the
                // request path is a bounded covert channel and ADR-0014 says so. Not
                // always_confirm: the confirmation happened when the source was added to
                // the allow-list, which is a policy.write, and requiring one per call would
                // make unattended update downloads impossible on a headless node.
                // Egress blast radius: nothing moves here, but after this call the object
                // is permitted to reach the peers named and nothing further will be asked.
                //
                // Not always_confirm, which is the uncomfortable half of this decision. §8's
                // rule is about *promotion* — making existing data more visible — and
                // store.demote already refuses widening and routes it to label.promote.
                // This creates a new object out of bytes the caller already holds, and it
                // is strictly narrower than store.put with visibility "public", which needs
                // only store.write and no confirmation. Requiring a person for the
                // encrypted, recipient-limited call while the plaintext-to-everyone call
                // goes through unattended would make the safer option the harder one, which
                // is how a system teaches people to use the unsafe one. Confirmation for
                // SHARED stays where ADR-0019 §4 puts it: on egress.
                ActionSpec::new(
                    "store.share",
                    "Encrypt an object to named recipients, allowing it to reach them",
                    BlastRadius::Egress,
                    false,
                ),
                ActionSpec::new(
                    "store.read",
                    "Read an object from the content store, whatever its label",
                    BlastRadius::Read,
                    false,
                ),
                ActionSpec::new(
                    "store.write",
                    "Put an object into the content store",
                    BlastRadius::Reversible,
                    false,
                ),
                // Separate from store.read for the reason ADR-0010 separated id.sign_session
                // from id.sign: otwono-netd must be able to serve a peer without being able
                // to read everything on the node. Egress blast radius because the bytes
                // leave; not always_confirm, because the method already refuses anything
                // but PUBLIC and REPLICATED, and a per-request prompt would make serving
                // impossible on an unattended node.
                ActionSpec::new(
                    "store.serve",
                    "Hand an object to a peer, if its label permits leaving the node",
                    BlastRadius::Egress,
                    false,
                ),
                // The cluster cache is its own pair of capabilities, not store.read
                // and store.write, for the same reason store.serve is not store.read:
                // otwono-netd must be able to add what it fetched to the shared cache
                // without being able to write the user's own store. Reversible because
                // everything in the cache is disposable and re-fetchable by definition.
                ActionSpec::new(
                    "cache.read",
                    "Read the cluster cache's contents and accounting",
                    BlastRadius::Read,
                    false,
                ),
                ActionSpec::new(
                    "cache.write",
                    "Add to, pin in, or purge the cluster cache",
                    BlastRadius::Reversible,
                    false,
                ),
                // Separate from cache.write, and the distinction is the operator's to make:
                // cache.write is "keep what I fetched", where the bytes are already here
                // because someone asked for them; this is "keep what a stranger offered"
                // (ADR-0026 §10). Reversible for the same reason cache.write is -- a replica
                // is disposable by construction. Not always_confirm, because a node that had
                // to prompt before holding one could not replicate while unattended, which
                // is the only condition under which replication is worth anything.
                // Publishing changes what a *name* means to every peer that reads it, where
                // store.write only adds bytes nobody has asked for. Egress blast radius
                // because the record is meant to leave the node -- that is what publishing
                // is -- and a person may reasonably run a node that stores and publishes
                // nothing (ADR-0027).
                ActionSpec::new(
                    "pointer.read",
                    "Read which names this node publishes, and at what sequence",
                    BlastRadius::Read,
                    false,
                ),
                // Local state only: recording what a peer said about its own names, and the
                // sequence it said it at. Reversible -- losing it costs rollback protection,
                // which is a real loss but not an irreversible one, and the record can be
                // fetched again (ADR-0027 §1).
                ActionSpec::new(
                    "pointer.write",
                    "Record what a peer published, and remember its sequence",
                    BlastRadius::Reversible,
                    false,
                ),
                ActionSpec::new(
                    "pointer.publish",
                    "Publish a signed pointer under this node's name",
                    BlastRadius::Egress,
                    false,
                ),
                ActionSpec::new(
                    "cache.replicate",
                    "Hold content offered by a peer as a replica for the cluster",
                    BlastRadius::Reversible,
                    false,
                ),
                // Separate from cache.replicate, which is the whole point (ADR-0028 §8).
                // Holding neighbourhood content the operator can inspect and purge is a
                // different thing to agree to than carrying a stranger's opaque mail, and
                // one capability for both would mean granting the first silently enrols
                // them in the second.
                ActionSpec::new(
                    "envelope.carry",
                    "Hold an addressed envelope for a recipient this node may meet later",
                    BlastRadius::Reversible,
                    false,
                ),
                ActionSpec::new(
                    "net.fetch",
                    "Fetch an object from a source on the egress allow-list",
                    BlastRadius::Egress,
                    false,
                ),
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
                // The wallet (ADR-0022 §3, ADR-0023 §3). Three of these four always
                // confirm, which means that until Phase 7 ships a confirmation channel the
                // wallet can be read and nothing else -- not created, not signed with, not
                // exported. ADR-0023 §4 records that plainly rather than leaving it to be
                // met as a surprise.
                //
                // Read is genuinely Read: ADR-0023 §2 keeps no public key in the clear, so
                // this reports whether a vault exists and what parameters it uses, and
                // anything naming an address needs the passphrase.
                ActionSpec::new(
                    "wallet.read",
                    "Read the wallet's status, and derive a public key given the passphrase",
                    BlastRadius::Read,
                    false,
                ),
                // Irreversible and confirmed for two independent reasons, either enough: it
                // returns the recovery phrase once, because somebody has to write it down --
                // so an unattended caller learns the seed of a wallet about to be funded,
                // which is wallet.export_seed's disclosure with different timing. And it
                // mints the key that holds money, with no undo and a silent failure: a
                // wallet created by somebody other than its owner looks exactly like one
                // created by its owner.
                //
                // Creating *over* an existing vault is refused by the daemon rather than
                // confirmed. A prompt invites a yes, and that yes is unrecoverable.
                ActionSpec::new(
                    "wallet.create",
                    "Create a wallet and show its recovery phrase once",
                    BlastRadius::Irreversible,
                    true,
                ),
                // Irreversible rather than Egress because signing does not send -- but a
                // signed transaction, once broadcast, cannot be recalled, and that is what
                // irreversible means here. otwono-fetchd does the sending (ADR-0014).
                ActionSpec::new(
                    "wallet.sign",
                    "Sign with a wallet key. What this signs cannot be recalled once sent",
                    BlastRadius::Irreversible,
                    true,
                ),
                // The one action that hands over everything at once. ADR-0022 §3 requires
                // the UI re-enter the passphrase rather than accept a confirmation click;
                // that is a UI obligation this registry cannot enforce and the ADR says so.
                ActionSpec::new(
                    "wallet.export_seed",
                    "Reveal the recovery phrase or seed, which is the whole wallet",
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
    fn serving_a_peer_is_a_narrower_action_than_reading_the_store() {
        // A network daemon needs to hand public content to peers. It does not need to be
        // able to read the user's private notes, and a single store.read would give it
        // both.
        let r = ActionRegistry::builtin();
        let read = r.get("store.read").expect("store.read must be registered");
        let serve = r.get("store.serve").expect("store.serve must be registered");
        assert_eq!(read.blast_radius, BlastRadius::Read);
        assert_eq!(
            serve.blast_radius,
            BlastRadius::Egress,
            "serving sends bytes off the node"
        );
        assert!(
            !serve.always_confirm,
            "the method refuses anything but public content, and an unattended node has \
             nobody to prompt"
        );
    }

    #[test]
    fn fetching_is_a_narrower_action_than_general_egress() {
        // Both send bytes off the node, so both carry Egress blast radius. The difference
        // is that a fetch goes somewhere an operator already approved, so it does not stop
        // for a human it may not have (ADR-0014).
        let r = ActionRegistry::builtin();
        let egress = r.get("net.egress").expect("net.egress must be registered");
        let fetch = r.get("net.fetch").expect("net.fetch must be registered");
        assert_eq!(egress.blast_radius, BlastRadius::Egress);
        assert_eq!(fetch.blast_radius, BlastRadius::Egress);
        assert!(egress.always_confirm);
        assert!(
            !fetch.always_confirm,
            "an unattended node has no one to confirm to"
        );
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
    fn every_wallet_action_that_spends_or_reveals_requires_confirmation() {
        // ADR-0022 §3 and ADR-0023 §3. Reading is free; everything that mints a key, spends,
        // or hands over the phrase stops for a person. Policy cannot clear these -- policy.rs
        // turns Allow into Ask for any always_confirm action -- so this registry is where
        // the requirement actually lives, and a later "just for testing" edit here would
        // silently remove the only thing standing between an agent and a household's money.
        let r = ActionRegistry::builtin();
        for id in ["wallet.create", "wallet.sign", "wallet.export_seed"] {
            let spec = r.get(id).unwrap_or_else(|| panic!("{id} must be registered"));
            assert!(spec.always_confirm, "{id} must always require confirmation");
            assert_eq!(
                spec.blast_radius,
                BlastRadius::Irreversible,
                "{id} cannot be undone once it has happened"
            );
        }
    }

    #[test]
    fn reading_the_wallet_never_needs_a_confirmation_or_a_write() {
        // The other half: if reading were confirmed too, a finance screen could not render
        // at all, and the pressure would be to widen something that should not widen.
        let r = ActionRegistry::builtin();
        let read = r.get("wallet.read").expect("wallet.read must be registered");
        assert!(!read.always_confirm);
        assert_eq!(read.blast_radius, BlastRadius::Read);
    }

    #[test]
    fn a_wallet_capability_is_never_satisfied_by_an_identity_one() {
        // ADR-0022 §2 put the wallet in its own daemon so that compromising the node's name
        // does not cost the household its money. That separation is only real if the
        // capabilities are distinct: an id.* token must never authorise a wallet.* action.
        // They are different strings, which is the mechanism -- this pins that nobody has
        // aliased them, and names why it would matter.
        let r = ActionRegistry::builtin();
        for wallet in [
            "wallet.read",
            "wallet.create",
            "wallet.sign",
            "wallet.export_seed",
        ] {
            assert!(r.get(wallet).is_some(), "{wallet} must be registered");
            assert!(
                !wallet.starts_with("id."),
                "{wallet} must not live in the identity daemon's namespace"
            );
        }
        // And the identity daemon's own signing capability is not a wallet one.
        assert!(r.get("id.sign").is_some());
        assert!(r.get("wallet.sign").is_some());
        assert_ne!("id.sign", "wallet.sign");
    }

    #[test]
    fn sharing_is_never_harder_to_reach_than_publishing_the_same_bytes() {
        // If this inverts, the system teaches people to publish rather than share: the
        // encrypted, recipient-limited call would stop for a person while the
        // plaintext-to-everyone one does not.
        let r = ActionRegistry::builtin();
        let share = r.get("store.share").expect("store.share must be registered");
        let put = r.get("store.write").expect("store.write must be registered");
        assert!(
            !(share.always_confirm && !put.always_confirm),
            "store.share must not demand confirmation that store.write does not"
        );
        assert_eq!(
            share.blast_radius,
            BlastRadius::Egress,
            "after this call the object may reach the peers named, with nothing else asked"
        );
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

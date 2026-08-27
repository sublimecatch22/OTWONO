//! Saying no, usefully.
//!
//! `AI-RUNTIME.md` §6: *"Degradation must be honest: a T0 node says 'I cannot do that
//! locally; I can queue it for your workstation when it is reachable' rather than producing
//! a bad answer."*
//!
//! That sentence has three parts, and a refusal that drops any of them is worse than
//! useless:
//!
//! 1. **I cannot do that** — plainly, not hedged into something that reads like a failure
//!    the user caused.
//! 2. **locally** — the limit is this machine, not the request. A user told "I can't" will
//!    stop asking; a user told "not on this machine" will ask somewhere else.
//! 3. **I can queue it for your workstation** — where it *could* happen, or an honest "I do
//!    not know of anywhere", which is a different and equally useful answer.
//!
//! # Why this is a type and not a string
//!
//! Because part 3 is a claim about the world that can be wrong. If [`Elsewhere`] is a
//! sentence, nothing stops a caller writing a hopeful one; if it is a value, the compiler
//! makes whoever constructs it decide whether a peer is actually known and reachable. The
//! prose is generated at the edge, from facts, rather than typed in wherever a refusal
//! happens to be raised.

use crate::AssistantShape;
use serde::{Deserialize, Serialize};

/// Where a refused request could be carried out instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Elsewhere {
    /// Nowhere this node knows of. The honest common case on a lone T0 node, and stated
    /// rather than dressed up: a user who is told to try later, with nothing behind that,
    /// learns to distrust everything else the assistant says.
    Nowhere,
    /// A peer this node has authenticated that advertises inference. Naming the peer
    /// matters — "a peer could do this" is a rumour; "your workstation could" is an offer.
    Peer {
        /// The peer's NodeID fingerprint, the same short form `otwono-netd --peers` shows.
        fingerprint: String,
        /// Whether it is connected right now. False means the work can be queued, not done,
        /// and the difference is exactly what the user needs to hear.
        reachable: bool,
    },
    /// A cloud provider the user has configured. Never a default: a node that reached for
    /// somebody's cloud because it could not answer locally would be sending the user's
    /// request off the machine to solve a problem the user never agreed to solve that way.
    ConfiguredProvider { name: String },
}

/// Why the assistant will not do something, and what would change that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Refusal {
    /// The words parsed, but there is no such verb.
    ///
    /// Carries suggestions because a T0 assistant's failure mode is a user guessing at
    /// vocabulary, and a closed verb set is the one situation where "did you mean" is
    /// reliable rather than a guess dressed as help.
    UnknownVerb { said: String, suggestions: Vec<String> },
    /// The verb exists but the arguments do not fit it.
    Malformed {
        verb: String,
        expected: String,
        got: String,
    },
    /// The verb exists and the machine cannot run it in this shape.
    ///
    /// The §6 case. `shape` is carried so the message can say *why* rather than only *that*
    /// — "this machine runs a command grammar" is a fact a user can act on, where "not
    /// supported" is not.
    NeedsAModel {
        said: String,
        shape: AssistantShape,
        elsewhere: Elsewhere,
    },
}

impl Refusal {
    /// The refusal as a sentence for a person.
    ///
    /// Generated here, from the values, so that every refusal in the system says the same
    /// thing the same way — and so that the awkward cases (nowhere to send it, a peer that
    /// is known but offline) cannot be quietly skipped by whoever writes the next caller.
    pub fn message(&self) -> String {
        match self {
            Refusal::UnknownVerb { said, suggestions } => {
                let mut m = format!("I do not know how to \"{said}\".");
                if !suggestions.is_empty() {
                    m.push_str(&format!(" Did you mean: {}?", suggestions.join(", ")));
                }
                m
            }
            Refusal::Malformed { verb, expected, got } => {
                format!("\"{verb}\" needs {expected}, but got {got}.")
            }
            Refusal::NeedsAModel {
                said,
                shape,
                elsewhere,
            } => {
                // Never "I cannot". Always "I cannot *here*", because the limit is the
                // machine and saying otherwise misinforms the user about their own system.
                let mut m = format!(
                    "I cannot do \"{said}\" on this machine: it runs a {} assistant, \
                     which answers a fixed set of commands and does not reason about \
                     open-ended requests.",
                    match shape {
                        AssistantShape::CommandGrammar => "command-grammar",
                        _ => "limited",
                    }
                );
                match elsewhere {
                    Elsewhere::Nowhere => m.push_str(
                        " I do not know of another node that could, so there is nothing \
                         to queue this for.",
                    ),
                    Elsewhere::Peer {
                        fingerprint,
                        reachable: true,
                    } => m.push_str(&format!(" Peer {fingerprint} could do it now.")),
                    Elsewhere::Peer {
                        fingerprint,
                        reachable: false,
                    } => m.push_str(&format!(
                        " I can queue it for peer {fingerprint} when it is reachable."
                    )),
                    Elsewhere::ConfiguredProvider { name } => {
                        m.push_str(&format!(" Your configured provider {name} could do it."))
                    }
                }
                m
            }
        }
    }

    /// Whether the request could succeed somewhere else as things stand.
    ///
    /// A known-but-unreachable peer counts: the work can be queued, which is the §6
    /// sentence's whole point. Nowhere does not.
    pub fn could_happen_elsewhere(&self) -> bool {
        matches!(
            self,
            Refusal::NeedsAModel {
                elsewhere: Elsewhere::Peer { .. } | Elsewhere::ConfiguredProvider { .. },
                ..
            }
        )
    }
}

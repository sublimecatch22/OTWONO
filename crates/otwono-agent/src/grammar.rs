//! The verb table, and the parse from words to an [`Intent`].
//!
//! # The table is the design
//!
//! Every verb maps to a control-plane method that already exists and is already authorized
//! by `otwono-permd`. The grammar invents no operations. That is what makes it safe to give
//! a T0 node an assistant at all: the assistant's reach is exactly the set of things the
//! user could already do by typing a `*ctl` command, and its privilege is exactly the
//! caller's, because it does not execute anything (see [`crate::intent`]).
//!
//! Adding a verb is therefore never a security decision — the method it names was already
//! reachable and already gated. It is a vocabulary decision.
//!
//! # Why not fuzzy matching
//!
//! Because a deterministic grammar that guesses is a language model with none of the
//! benefits. The parse either recognises the verb or refuses; the only concession is
//! [`Refusal::UnknownVerb`]'s suggestion list, which is edit-distance over a **closed set**
//! of a dozen words. That is a different thing from inferring intent: it never produces an
//! `Intent`, only a better error.

use crate::intent::{Argument, Intent};
use crate::refusal::{Elsewhere, Refusal};
use crate::AssistantShape;
use std::collections::BTreeMap;

/// One verb: what to say, what it means, and what it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verb {
    pub word: &'static str,
    pub method: &'static str,
    pub capability: Option<&'static str>,
    /// Argument names, in the order they are given positionally.
    pub parameters: &'static [(&'static str, ParamKind)],
    pub mutates: bool,
    /// One line, for `otwono do help`.
    pub summary: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    Path,
    ContentId,
    Visibility,
    /// A word from a fixed set, checked at parse time.
    OneOf(&'static [&'static str]),
    /// Optional trailing parameter. Absent is legal; present must fit the inner kind.
    Optional(&'static ParamKind),
}

const VISIBILITIES: &[&str] = &["private", "shared", "public", "replicated"];

const VIS: ParamKind = ParamKind::Visibility;

/// The verbs a T0 node understands.
///
/// Small on purpose. Each one is a thing people actually ask an OS to do, phrased the way
/// they ask it, and each maps to a method that already exists. The bar for adding one is
/// that it is a real request in ordinary words — not that a method exists to expose.
pub const VERBS: &[Verb] = &[
    Verb {
        word: "save",
        method: "store.put",
        capability: Some("store.write"),
        parameters: &[
            ("file", ParamKind::Path),
            ("visibility", ParamKind::Optional(&VIS)),
        ],
        mutates: true,
        summary: "save a file into your store (private unless you say otherwise)",
    },
    Verb {
        word: "fetch",
        method: "store.get",
        capability: Some("store.read"),
        parameters: &[("content_id", ParamKind::ContentId)],
        mutates: false,
        summary: "read something back out of your store by its id",
    },
    Verb {
        word: "describe",
        method: "store.stat",
        capability: Some("store.read"),
        parameters: &[("content_id", ParamKind::ContentId)],
        mutates: false,
        summary: "say what an object is, without reading its contents",
    },
    Verb {
        word: "hide",
        method: "store.demote",
        capability: Some("store.write"),
        parameters: &[("content_id", ParamKind::ContentId), ("visibility", VIS)],
        mutates: true,
        summary: "make an object less visible than it is now",
    },
    Verb {
        word: "tier",
        method: "hw.tier",
        capability: Some("hw.read"),
        parameters: &[],
        mutates: false,
        summary: "say what class of machine this is",
    },
    Verb {
        word: "hardware",
        method: "hw.profile",
        capability: Some("hw.read"),
        parameters: &[],
        mutates: false,
        summary: "the full capability profile for this machine",
    },
    Verb {
        word: "peers",
        method: "net.peers",
        capability: Some("net.read"),
        parameters: &[],
        mutates: false,
        summary: "which other nodes this one has authenticated",
    },
    Verb {
        word: "cache",
        method: "cache.status",
        capability: Some("cache.read"),
        parameters: &[],
        mutates: false,
        summary: "what this node is holding for its neighbours, and how much room is left",
    },
];

/// The verb table this machine offers.
///
/// Takes the shape rather than reading it, so the decision stays in the capability engine
/// where CLAUDE.md §2.6 puts it. A grammar that classified its own machine would be the
/// second place that opinion lives.
#[derive(Debug, Clone, Copy)]
pub struct Grammar {
    shape: AssistantShape,
}

impl Grammar {
    pub fn for_shape(shape: AssistantShape) -> Grammar {
        Grammar { shape }
    }

    pub fn shape(&self) -> AssistantShape {
        self.shape
    }

    /// Every verb, for `help`.
    pub fn verbs(&self) -> &'static [Verb] {
        VERBS
    }
}

/// Parse a request into an intent, or refuse it.
///
/// `elsewhere` is passed in rather than discovered: whether a peer could help is a fact
/// about the mesh at this moment, which this crate has no business reaching out to find.
/// The caller knows, and passing it keeps the parse a pure function — exhaustively testable
/// with no network, which is the only way a grammar's determinism can actually be checked.
pub fn parse(grammar: &Grammar, words: &[&str], elsewhere: &Elsewhere) -> Result<Intent, Refusal> {
    let Some((head, rest)) = words.split_first() else {
        return Err(Refusal::UnknownVerb {
            said: String::new(),
            suggestions: VERBS.iter().take(3).map(|v| v.word.to_string()).collect(),
        });
    };
    let head_lower = head.to_lowercase();

    let Some(verb) = VERBS.iter().find(|v| v.word == head_lower) else {
        // A shape with a model would send this to the model instead of refusing. Saying so
        // here, rather than at the call site, is what keeps the T0 message accurate as
        // higher shapes are built: this arm is the one that has to change.
        if grammar.shape.uses_a_model() {
            return Err(Refusal::NeedsAModel {
                said: words.join(" "),
                shape: grammar.shape,
                elsewhere: elsewhere.clone(),
            });
        }
        return Err(Refusal::UnknownVerb {
            said: head.to_string(),
            suggestions: suggestions_for(&head_lower),
        });
    };

    let required = verb
        .parameters
        .iter()
        .filter(|(_, k)| !matches!(k, ParamKind::Optional(_)))
        .count();
    if rest.len() < required || rest.len() > verb.parameters.len() {
        return Err(Refusal::Malformed {
            verb: verb.word.to_string(),
            expected: describe_parameters(verb),
            got: if rest.is_empty() {
                "nothing".to_string()
            } else {
                format!("{} argument(s)", rest.len())
            },
        });
    }

    let mut arguments = BTreeMap::new();
    for ((name, kind), given) in verb.parameters.iter().zip(rest.iter()) {
        let argument = check(kind, given).ok_or_else(|| Refusal::Malformed {
            verb: verb.word.to_string(),
            expected: format!("{name} to be {}", kind_name(kind)),
            got: format!("\"{given}\""),
        })?;
        arguments.insert((*name).to_string(), argument);
    }

    Ok(Intent {
        verb: verb.word.to_string(),
        method: verb.method.to_string(),
        capability: verb.capability.map(str::to_string),
        arguments,
        mutates: verb.mutates,
    })
}

fn check(kind: &ParamKind, given: &str) -> Option<Argument> {
    match kind {
        ParamKind::Optional(inner) => check(inner, given),
        ParamKind::Path => (!given.is_empty()).then(|| Argument::Path(given.to_string())),
        ParamKind::ContentId => is_content_id(given).then(|| Argument::ContentId(given.to_string())),
        ParamKind::Visibility => {
            // Lowercased, because a label is compared byte for byte everywhere else and two
            // spellings would be two labels. Rejecting an unknown one rather than defaulting
            // is deliberate: CLAUDE.md §8 makes a missing label PRIVATE, and quietly turning
            // a typo into PRIVATE would hide the typo on the one call where it matters most.
            let lower = given.to_lowercase();
            VISIBILITIES
                .contains(&lower.as_str())
                .then_some(Argument::Visibility(lower))
        }
        ParamKind::OneOf(allowed) => {
            let lower = given.to_lowercase();
            allowed.contains(&lower.as_str()).then_some(Argument::Word(lower))
        }
    }
}

fn is_content_id(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn kind_name(kind: &ParamKind) -> String {
    match kind {
        ParamKind::Optional(inner) => format!("an optional {}", kind_name(inner)),
        ParamKind::Path => "a file path".into(),
        ParamKind::ContentId => "a 64-character lowercase hex content id".into(),
        ParamKind::Visibility => format!("one of {}", VISIBILITIES.join(", ")),
        ParamKind::OneOf(allowed) => format!("one of {}", allowed.join(", ")),
    }
}

fn describe_parameters(verb: &Verb) -> String {
    if verb.parameters.is_empty() {
        return "no arguments".into();
    }
    let names: Vec<String> = verb
        .parameters
        .iter()
        .map(|(n, k)| match k {
            ParamKind::Optional(_) => format!("[{n}]"),
            _ => format!("<{n}>"),
        })
        .collect();
    names.join(" ")
}

/// Verbs close enough to what was typed to be worth offering.
///
/// Edit distance over a closed set of a dozen words, capped at three suggestions and at a
/// distance of two. The cap is the point: a list of every verb is not a suggestion, it is
/// the help text, and offering it as though it were a guess teaches the user that the
/// assistant does not know either.
fn suggestions_for(said: &str) -> Vec<String> {
    let mut scored: Vec<(usize, &'static str)> = VERBS
        .iter()
        .map(|v| (edit_distance(said, v.word), v.word))
        .filter(|(d, _)| *d <= 2)
        .collect();
    scored.sort_by_key(|(d, w)| (*d, *w));
    scored.into_iter().take(3).map(|(_, w)| w.to_string()).collect()
}

/// Levenshtein distance, two rows rather than a full matrix.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

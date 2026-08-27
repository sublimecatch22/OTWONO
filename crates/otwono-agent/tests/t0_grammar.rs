//! The T0 assistant, exercised as a whole.
//!
//! `AI-RUNTIME.md` §6 makes two promises about a T0 node, and both are testable without a
//! model, a socket or a daemon — which is the point of the shape:
//!
//! 1. a fixed set of commands works, deterministically;
//! 2. everything else is refused *honestly* — this machine, not this request, and with
//!    somewhere else named when there is one.

use otwono_agent::{parse, AssistantShape, Elsewhere, Grammar, Refusal};

fn t0() -> Grammar {
    Grammar::for_shape(AssistantShape::CommandGrammar)
}

fn nowhere() -> Elsewhere {
    Elsewhere::Nowhere
}

#[test]
fn a_t0_node_understands_its_verbs_and_maps_them_to_real_methods() {
    let g = t0();
    let intent = parse(&g, &["save", "/home/user/notes.md"], &nowhere()).expect("a known verb");
    assert_eq!(intent.method, "store.put");
    assert_eq!(intent.capability.as_deref(), Some("store.write"));
    assert!(intent.mutates);
    assert_eq!(intent.arguments["file"].as_str(), "/home/user/notes.md");
}

/// Every verb names a method that some daemon actually serves.
///
/// The grammar's safety argument is that it invents no operations — each verb is a method
/// that was already reachable and already gated. A typo here would silently produce intents
/// nothing can dispatch, and the assistant would fail at the last step for a reason no user
/// could act on.
#[test]
fn every_verb_names_a_method_that_exists_and_a_capability_that_is_real() {
    let known_methods = [
        "store.put",
        "store.get",
        "store.stat",
        "store.demote",
        "hw.tier",
        "hw.profile",
        "net.peers",
        "cache.status",
    ];
    let known_capabilities = ["store.write", "store.read", "hw.read", "net.read", "cache.read"];
    for verb in t0().verbs() {
        assert!(
            known_methods.contains(&verb.method),
            "verb \"{}\" names method {}, which no daemon serves",
            verb.word,
            verb.method
        );
        if let Some(cap) = verb.capability {
            assert!(
                known_capabilities.contains(&cap),
                "verb \"{}\" needs capability {cap}, which the broker does not register",
                verb.word
            );
        }
        assert!(!verb.summary.is_empty(), "\"{}\" has no help text", verb.word);
    }
}

/// A mutating verb must be marked as one.
///
/// Carried on the intent so a caller can confirm before acting without consulting a second
/// table — and a second table is exactly how a "read-only" verb eventually writes.
#[test]
fn the_verbs_that_change_things_say_so() {
    for verb in t0().verbs() {
        let writes = verb.method.ends_with(".put") || verb.method.ends_with(".demote");
        assert_eq!(
            verb.mutates, writes,
            "\"{}\" ({}) is marked mutates={}",
            verb.word, verb.method, verb.mutates
        );
    }
}

#[test]
fn an_unknown_verb_is_refused_with_suggestions_from_the_closed_set() {
    let err = parse(&t0(), &["sav", "/tmp/x"], &nowhere()).expect_err("no such verb");
    match &err {
        Refusal::UnknownVerb { said, suggestions } => {
            assert_eq!(said, "sav");
            assert!(suggestions.contains(&"save".to_string()), "{suggestions:?}");
            assert!(suggestions.len() <= 3, "a list of everything is not a suggestion");
        }
        other => panic!("{other:?}"),
    }
    assert!(err.message().contains("Did you mean"), "{}", err.message());
}

/// Nonsense gets no suggestions rather than the nearest word by force.
#[test]
fn a_word_close_to_nothing_is_offered_nothing() {
    let err = parse(&t0(), &["photosynthesise"], &nowhere()).expect_err("no such verb");
    match err {
        Refusal::UnknownVerb { suggestions, .. } => assert!(
            suggestions.is_empty(),
            "offered {suggestions:?} for a word like nothing in the table"
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_wrong_arguments_are_refused_with_what_was_wanted() {
    // Too few.
    let err = parse(&t0(), &["save"], &nowhere()).expect_err("save needs a file");
    assert!(err.message().contains("<file>"), "{}", err.message());

    // Right count, wrong shape: a content id that is not one.
    let err = parse(&t0(), &["fetch", "not-a-digest"], &nowhere()).expect_err("bad id");
    assert!(err.message().contains("hex content id"), "{}", err.message());

    // Too many.
    let id = "a".repeat(64);
    assert!(
        parse(&t0(), &["tier", &id], &nowhere()).is_err(),
        "tier takes nothing"
    );
}

/// An unknown visibility is refused, never quietly defaulted.
///
/// CLAUDE.md §8 makes a missing label PRIVATE. Turning a *typo* into PRIVATE would hide the
/// typo on the one call where the user was thinking hardest about exposure — and would mean
/// "public" and "publci" behave differently with no message.
#[test]
fn a_misspelled_visibility_is_refused_rather_than_defaulted() {
    let err = parse(&t0(), &["save", "/tmp/x", "publci"], &nowhere()).expect_err("bad label");
    assert!(err.message().contains("private"), "{}", err.message());
    let ok = parse(&t0(), &["save", "/tmp/x", "PUBLIC"], &nowhere()).expect("case is forgiven");
    assert_eq!(ok.arguments["visibility"].as_str(), "public");
}

#[test]
fn an_optional_argument_may_be_left_out() {
    let intent = parse(&t0(), &["save", "/tmp/x"], &nowhere()).expect("visibility is optional");
    assert!(!intent.arguments.contains_key("visibility"));
}

/// The §6 sentence, checked as a sentence.
#[test]
fn a_refusal_names_the_machine_not_the_request() {
    let refusal = Refusal::NeedsAModel {
        said: "summarise my week".into(),
        shape: AssistantShape::CommandGrammar,
        elsewhere: Elsewhere::Nowhere,
    };
    let m = refusal.message();
    assert!(m.contains("on this machine"), "{m}");
    assert!(m.contains("command-grammar"), "{m}");
    assert!(m.contains("nothing to queue"), "{m}");
    assert!(!refusal.could_happen_elsewhere());
}

/// "I can queue it for your workstation when it is reachable" — the actual §6 example.
#[test]
fn an_unreachable_peer_is_offered_as_something_to_queue_for() {
    let refusal = Refusal::NeedsAModel {
        said: "summarise my week".into(),
        shape: AssistantShape::CommandGrammar,
        elsewhere: Elsewhere::Peer {
            fingerprint: "otw1:twe8-ekyb-schm-64rb".into(),
            reachable: false,
        },
    };
    let m = refusal.message();
    assert!(m.contains("queue it for peer otw1:twe8-ekyb-schm-64rb"), "{m}");
    assert!(m.contains("when it is reachable"), "{m}");
    assert!(
        refusal.could_happen_elsewhere(),
        "a known peer that is merely offline is still somewhere to queue for"
    );

    // Reachable now is a different sentence: an offer, not a queue.
    let now = Refusal::NeedsAModel {
        said: "summarise my week".into(),
        shape: AssistantShape::CommandGrammar,
        elsewhere: Elsewhere::Peer {
            fingerprint: "otw1:twe8-ekyb-schm-64rb".into(),
            reachable: true,
        },
    };
    assert!(now.message().contains("could do it now"), "{}", now.message());
}

/// The grammar holds no privilege, and the intent it produces is inert.
///
/// This is the property that lets a T0 node have an assistant at all: parsing something is
/// not being allowed to do it. The intent names the capability the *caller* must hold; it
/// does not carry one (CLAUDE.md §2.5).
#[test]
fn parsing_is_not_permission() {
    let intent = parse(&t0(), &["hide", &"b".repeat(64), "private"], &nowhere()).expect("valid");
    assert_eq!(intent.capability.as_deref(), Some("store.write"));
    // Serialising it must not produce a token, a signature, or anything bearer-shaped.
    let json = serde_json::to_string(&intent).unwrap();
    for leak in ["token", "capability_token", "signature", "secret"] {
        assert!(!json.contains(leak), "an intent carried a {leak}: {json}");
    }
}

/// The shape comes from the capability engine, and the grammar does not second-guess it.
#[test]
fn a_shape_with_a_model_sends_unknown_requests_to_it_rather_than_refusing() {
    // The same words that a T0 node refuses outright are, on a T1 node, the case a model
    // exists to handle. Asserting it here means the T0 message stays accurate as the higher
    // shapes get built: this is the arm that has to change.
    let t1 = Grammar::for_shape(AssistantShape::SingleStepToolCalling);
    let err = parse(&t1, &["summarise", "my", "week"], &nowhere()).expect_err("not a verb");
    assert!(
        matches!(err, Refusal::NeedsAModel { .. }),
        "a shape with a model should not answer with UnknownVerb: {err:?}"
    );
    assert!(AssistantShape::CommandGrammar < AssistantShape::SingleStepToolCalling);
    assert!(!AssistantShape::CommandGrammar.uses_a_model());
}

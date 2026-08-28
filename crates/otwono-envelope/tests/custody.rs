//! What a carrier may and may not do with somebody else's envelope (ADR-0028).
//!
//! The interesting cases are all about a carrier that behaves *generously*. A relay that
//! refuses things is visible and self-limiting; a relay that holds a message longer than it
//! was meant to live, or hands it to the wrong node, is the failure nobody notices.

use otwono_envelope::{Carry, CarryPolicy, Declined, Envelope};
use otwono_identity::NodeIdentity;

fn node(seed: u8) -> NodeIdentity {
    NodeIdentity::from_seeds(&[seed; 32], &[seed; 32], 1)
}

fn cid(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn policy() -> CarryPolicy {
    CarryPolicy {
        room_bytes: 1 << 20,
        max_size_bytes: 64 << 10,
        max_hold_ms: 24 * 60 * 60 * 1000,
    }
}

const NOW: u64 = 1_700_000_000_000;

fn offered(expires_at_ms: u64) -> Envelope {
    Envelope::new(&cid(0x0a), node(1).node_id(), 4096, expires_at_ms)
}

/// A carrier may bring an expiry closer. It may never push one further out.
///
/// The central rule of ADR-0028 §3, and the one worth pinning hardest: a replica's TTL
/// restarts when it is offered again, because replication is about durability. If an
/// envelope inherited that behaviour, a message would live as long as any relay kept
/// meeting a peer that re-offered it — a message network quietly becoming a permanent
/// archive of everyone's undelivered mail.
#[test]
fn a_carrier_can_shorten_an_expiry_and_never_extend_one() {
    let p = policy();

    // A sender asking for a year gets the carrier's ceiling instead.
    let greedy = offered(NOW + 365 * 24 * 60 * 60 * 1000);
    assert_eq!(
        p.decide(&greedy, NOW),
        Carry::Accept {
            until_ms: NOW + p.max_hold_ms
        },
        "a carrier committed to holding an envelope for longer than its own ceiling"
    );

    // A sender asking for an hour gets an hour, not the ceiling. The rule is a minimum of
    // the two, not a replacement -- a carrier that rounded every envelope up to its ceiling
    // would be extending most of them.
    let modest = offered(NOW + 60 * 60 * 1000);
    assert_eq!(
        p.decide(&modest, NOW),
        Carry::Accept {
            until_ms: NOW + 60 * 60 * 1000
        },
        "a carrier extended an envelope that asked for less than its ceiling"
    );

    // Exactly at the ceiling is the boundary, and it is the sender's number that stands.
    let exact = offered(NOW + p.max_hold_ms);
    assert_eq!(
        p.decide(&exact, NOW),
        Carry::Accept {
            until_ms: NOW + p.max_hold_ms
        }
    );
}

/// Re-offering the same envelope later does not buy it more time.
///
/// The other half of the same rule, from the direction it would actually fail: the carrier
/// is stateless per decision, so what stops a refresh is that the sender's absolute
/// `expires_at_ms` is the ceiling of every decision, not that anyone remembers the first one.
#[test]
fn offering_the_same_envelope_again_does_not_refresh_it() {
    let p = policy();
    let deadline = NOW + 2 * 60 * 60 * 1000;
    let envelope = offered(deadline);

    assert_eq!(p.decide(&envelope, NOW), Carry::Accept { until_ms: deadline });
    // An hour later, offered again by a peer that still has it.
    assert_eq!(
        p.decide(&envelope, NOW + 60 * 60 * 1000),
        Carry::Accept { until_ms: deadline },
        "re-offering moved the deadline"
    );
    // And once it is past, it is past, however often it is offered.
    assert_eq!(
        p.decide(&envelope, deadline),
        Carry::Decline(Declined::Expired),
        "an expired envelope was taken at the moment it expired"
    );
    assert_eq!(
        p.decide(&envelope, deadline + 1),
        Carry::Decline(Declined::Expired)
    );
}

/// Every field comes from a stranger, so every field is checked.
#[test]
fn a_descriptor_from_a_stranger_is_not_trusted_until_it_parses() {
    let p = policy();
    let good = offered(NOW + 1000);
    assert!(good.validate().is_ok());

    let mut bad_id = good.clone();
    bad_id.envelope_id = "../../etc/otwono/policy.d/10-default.toml".into();
    assert!(matches!(
        p.decide(&bad_id, NOW),
        Carry::Decline(Declined::Malformed(_))
    ));

    // Uppercase hex is not the shape a content id has here, and accepting it would mean two
    // strings naming one object.
    let mut shouty = good.clone();
    shouty.envelope_id = shouty.envelope_id.to_uppercase();
    assert!(matches!(
        p.decide(&shouty, NOW),
        Carry::Decline(Declined::Malformed(_))
    ));

    let mut bad_recipient = good.clone();
    bad_recipient.recipient = "otw1-not-a-node".into();
    assert!(matches!(
        p.decide(&bad_recipient, NOW),
        Carry::Decline(Declined::Malformed(_))
    ));

    let mut empty = good.clone();
    empty.size_bytes = 0;
    assert!(matches!(
        p.decide(&empty, NOW),
        Carry::Decline(Declined::Malformed(_))
    ));
}

/// A carrier's own limits are its own, and it says which one it hit.
#[test]
fn a_carrier_refuses_on_its_own_terms_and_names_which() {
    let p = CarryPolicy {
        room_bytes: 8192,
        max_size_bytes: 4096,
        max_hold_ms: 1000,
    };

    let big = Envelope::new(&cid(0x0b), node(1).node_id(), 5000, NOW + 500);
    assert_eq!(
        p.decide(&big, NOW),
        Carry::Decline(Declined::TooLarge {
            size_bytes: 5000,
            ceiling_bytes: 4096
        }),
        "the per-envelope ceiling must bite before the budget does"
    );

    // Within the per-envelope ceiling, but the machine is full.
    let full = CarryPolicy { room_bytes: 100, ..p };
    let ordinary = Envelope::new(&cid(0x0c), node(1).node_id(), 4000, NOW + 500);
    assert_eq!(
        full.decide(&ordinary, NOW),
        Carry::Decline(Declined::NoRoom {
            size_bytes: 4000,
            room_bytes: 100
        })
    );
}

/// An envelope is handed to the node it names, compared as an identity and not as a string.
#[test]
fn an_envelope_is_only_for_the_node_it_names() {
    let alice = node(1);
    let bob = node(2);
    let envelope = Envelope::new(&cid(0x0d), alice.node_id(), 128, NOW + 1000);

    assert!(envelope.is_for(alice.node_id()));
    assert!(
        !envelope.is_for(bob.node_id()),
        "an envelope was offered to the wrong node"
    );

    // An unparseable recipient is for nobody, rather than for everybody.
    let mut broken = envelope.clone();
    broken.recipient = "nonsense".into();
    assert!(!broken.is_for(alice.node_id()));
    assert!(!broken.is_for(bob.node_id()));
}

/// The descriptor says nothing about who sent it (ADR-0028 §4).
///
/// Asserted on the serialized form, because that is what actually crosses a link. A sender
/// field added later "just for debugging" would hand every relay a social graph, and it
/// would arrive without anyone deciding to make that trade.
#[test]
fn nothing_a_relay_receives_names_the_sender() {
    let envelope = offered(NOW + 1000);
    let json = serde_json::to_string(&envelope).unwrap();
    for leak in ["sender", "from", "author", "origin", "reply_to"] {
        assert!(
            !json.contains(leak),
            "the relay-visible descriptor carries a {leak:?} field: {json}"
        );
    }
    // What it does carry, so this test fails if the shape is gutted rather than passing
    // vacuously on an empty record.
    for expected in ["envelope_id", "recipient", "size_bytes", "expires_at_ms"] {
        assert!(json.contains(expected), "the descriptor lost {expected:?}");
    }
}

/// A carrier's committed deadline is its own, and the sender's expiry is a ceiling on it.
///
/// ADR-0028 §10. The mesh has no NTP guarantee, so a sender's wall-clock instant compared
/// against a carrier's clock later is two numbers disagreeing for invisible reasons. What the
/// carrier sweeps on is what it committed to when it took custody.
#[test]
fn a_carrier_sweeps_on_the_deadline_it_committed_to() {
    use otwono_envelope::Custody;
    let p = policy();
    let envelope = offered(NOW + 3 * 60 * 60 * 1000);

    let Carry::Accept { until_ms } = p.decide(&envelope, NOW) else {
        panic!("a well-formed envelope inside budget must be accepted");
    };
    let held = Custody::taken(&envelope, NOW, until_ms);
    assert_eq!(
        held.until_ms,
        NOW + 3 * 60 * 60 * 1000,
        "the sender asked for less than the ceiling"
    );
    assert!(!held.is_due(NOW));
    assert!(!held.is_due(held.until_ms - 1));
    assert!(held.is_due(held.until_ms), "the committed deadline did not fire");
}

/// Constructing custody directly cannot lengthen an envelope's life.
///
/// The min-rule lives in `CarryPolicy::decide`, and this is the second door: a caller that
/// computed its own deadline, or a record read back off a disk somebody edited, still cannot
/// push the deadline past what the sender signed for.
#[test]
fn custody_cannot_outlive_what_the_sender_asked_for() {
    use otwono_envelope::Custody;
    let envelope = offered(NOW + 1000);
    let greedy = Custody::taken(&envelope, NOW, NOW + 999_999_999);
    assert_eq!(
        greedy.until_ms,
        NOW + 1000,
        "custody was constructed with a deadline past the sender's expiry"
    );
}

/// A carrier whose clock runs fast still drops it in bounded time.
///
/// The failure §10 exists to bound: with the deadline measured from the carrier's own custody
/// moment, a wrong clock shifts *when* the envelope is dropped, not *whether* it ever is.
#[test]
fn a_skewed_clock_still_drops_the_envelope_eventually() {
    use otwono_envelope::Custody;
    let p = policy();
    // A sender asking for a year; this carrier holds for a day at most.
    let envelope = offered(NOW + 365 * 24 * 60 * 60 * 1000);

    // A carrier whose clock reads a week ahead of the sender's.
    let skewed_now = NOW + 7 * 24 * 60 * 60 * 1000;
    let Carry::Accept { until_ms } = p.decide(&envelope, skewed_now) else {
        panic!("an envelope well inside its expiry must be accepted despite the skew");
    };
    let held = Custody::taken(&envelope, skewed_now, until_ms);
    assert_eq!(
        held.until_ms,
        skewed_now + p.max_hold_ms,
        "the deadline must come from this carrier's custody moment, not from clock arithmetic"
    );
    assert!(held.is_due(skewed_now + p.max_hold_ms));
}

/// A refusal names both a code and the numbers behind it.
///
/// Both matter, and they are not the same string. A carrier's reply names the code so a
/// caller can branch on it; a log carries the long form so an operator can tell an oversized
/// envelope from a full disk without reproducing the run that produced it.
#[test]
fn a_refusal_names_both_a_code_and_the_numbers_behind_it() {
    let cases = [
        (Declined::Expired, "expired"),
        (Declined::TooLarge { size_bytes: 9, ceiling_bytes: 4 }, "too_large"),
        (Declined::NoRoom { size_bytes: 9, room_bytes: 4 }, "no_room"),
        (Declined::Malformed("bad id".into()), "malformed"),
    ];
    for (declined, code) in cases {
        assert_eq!(declined.code(), code);
        assert!(
            declined.to_string().starts_with(code),
            "{declined} does not begin with its own code"
        );
    }
    assert_eq!(
        Declined::NoRoom { size_bytes: 9, room_bytes: 4 }.to_string(),
        "no_room: 9 bytes, 4 free",
        "the numbers are the reason this exists"
    );
}

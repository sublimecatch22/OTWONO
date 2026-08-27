//! The rules a signed mutable pointer lives by (ADR-0027).
//!
//! These are worth exercising exhaustively because the interesting failures all involve a
//! **genuinely valid signature**. A forgery is easy to catch and a forgery test proves
//! little; what this crate exists to stop is a correctly signed record that is old, or is
//! for a different name, or is signed by the wrong key — every one of which passes a naive
//! signature check.

use otwono_identity::NodeIdentity;
use otwono_pointer::{Accepted, Pointer, PointerError, PointerKey, SequenceLog};

fn identity(seed: u8) -> NodeIdentity {
    NodeIdentity::from_seeds(&[seed; 32], &[seed; 32], 1)
}

fn key_of(id: &NodeIdentity) -> [u8; 32] {
    id.public_key_bytes()
}

/// Sign a pointer the way `otwono-idd` would: `id.sign` over the pointer payload, which
/// prepends the application domain itself.
fn sign(pointer: &mut Pointer, id: &NodeIdentity) {
    let payload = pointer.payload_for_id_sign().expect("encodes");
    let signature = id.sign(&otwono_identity::domain_separated(&payload));
    pointer.signature = data_encoding::BASE64.encode(&signature.to_bytes());
}

fn published(id: &NodeIdentity, name: &str, sequence: u64, content: Option<&str>) -> Pointer {
    let mut p = Pointer::new(
        id.node_id(),
        "wiki",
        name,
        sequence,
        content.map(str::to_string),
        1_700_000_000_000 + sequence,
    );
    sign(&mut p, id);
    p
}

fn cid(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

#[test]
fn a_signed_pointer_verifies_and_a_tampered_one_does_not() {
    let alice = identity(1);
    let p = published(&alice, "Home", 1, Some(&cid(0xaa)));
    p.verify(&key_of(&alice)).expect("a genuine pointer");

    // Every field is covered: changing any of them must break the signature.
    for mutate in [
        (|p: &mut Pointer| p.sequence = 2) as fn(&mut Pointer),
        |p: &mut Pointer| p.content_id = Some(cid(0xbb)),
        |p: &mut Pointer| p.name = "Somewhere-Else".into(),
        |p: &mut Pointer| p.service = "profile".into(),
        |p: &mut Pointer| p.published_at_ms = 0,
        |p: &mut Pointer| p.content_id = None,
    ] {
        let mut tampered = p.clone();
        mutate(&mut tampered);
        assert_eq!(
            tampered.verify(&key_of(&alice)),
            Err(PointerError::BadSignature),
            "a field changed without breaking the signature"
        );
    }
}

/// Re-serializing the record through another tool must not change what verifies.
#[test]
fn key_order_does_not_change_what_the_signature_covers() {
    let alice = identity(2);
    let p = published(&alice, "Home", 1, Some(&cid(0x11)));
    let round_tripped: Pointer = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
    round_tripped.verify(&key_of(&alice)).expect("still verifies");
}

/// **The attack this crate exists for.** An old record is genuine and still verifies.
#[test]
fn an_older_record_verifies_perfectly_and_is_still_refused() {
    let alice = identity(3);
    let key = key_of(&alice);
    let asked = PointerKey {
        node_id: alice.node_id().to_text(),
        service: "wiki".into(),
        name: "Home".into(),
    };

    let old = published(&alice, "Home", 4, Some(&cid(0x44)));
    let new = published(&alice, "Home", 7, Some(&cid(0x77)));

    // Both are real. This is the point: the defence cannot be the signature.
    old.verify(&key).expect("the old one is genuinely signed");
    new.verify(&key).expect("so is the new one");

    let mut log = SequenceLog::new();
    assert_eq!(log.accept(&new, &key, &asked).unwrap(), Accepted::FirstSeen);
    assert_eq!(log.highest_seen(&asked), Some(7));

    assert_eq!(
        log.accept(&old, &key, &asked),
        Err(PointerError::Rollback { seen: 7, offered: 4 }),
        "a replayed older record was accepted"
    );
    // And the log is unmoved, so a second attempt fails identically rather than sliding.
    assert_eq!(log.highest_seen(&asked), Some(7));
}

/// The same sequence with different content is refused too.
///
/// Equal counts as a rollback: re-serving a sequence with changed bytes would otherwise be a
/// way to change what a name means without advancing it, which is the same attack wearing a
/// different number.
#[test]
fn the_same_sequence_cannot_be_reused_for_different_content() {
    let alice = identity(4);
    let key = key_of(&alice);
    let asked = PointerKey {
        node_id: alice.node_id().to_text(),
        service: "wiki".into(),
        name: "Home".into(),
    };
    let mut log = SequenceLog::new();

    let first = published(&alice, "Home", 5, Some(&cid(0x01)));
    log.accept(&first, &key, &asked).expect("first is taken");

    let restated = published(&alice, "Home", 5, Some(&cid(0x02)));
    restated.verify(&key).expect("genuinely signed by the owner");
    assert!(matches!(
        log.accept(&restated, &key, &asked),
        Err(PointerError::Rollback { seen: 5, offered: 5 })
    ));
}

/// A first read has no protection, and the API says which case it was.
///
/// Folding this into "accepted" would hide the one moment a reader is defenceless. A caller
/// that wants to warn a person, or pin a key, can only do it if it is told.
#[test]
fn a_first_read_is_marked_as_trust_on_first_use() {
    let alice = identity(5);
    let key = key_of(&alice);
    let asked = PointerKey {
        node_id: alice.node_id().to_text(),
        service: "wiki".into(),
        name: "Home".into(),
    };
    let mut log = SequenceLog::new();
    assert_eq!(log.highest_seen(&asked), None);

    // Even a wildly high sequence is taken on first sight — there is nothing to compare to.
    let whatever = published(&alice, "Home", 9_000, Some(&cid(0x99)));
    assert_eq!(log.accept(&whatever, &key, &asked).unwrap(), Accepted::FirstSeen);

    // From here on it is protected.
    let older = published(&alice, "Home", 8_999, Some(&cid(0x88)));
    assert!(matches!(
        log.accept(&older, &key, &asked),
        Err(PointerError::Rollback { .. })
    ));
}

/// Somebody else's signature over your name is refused, however valid it is.
#[test]
fn a_record_signed_by_the_wrong_key_is_refused() {
    let alice = identity(6);
    let mallory = identity(7);

    // Mallory writes a pointer claiming Alice's NodeID and signs it with her own key.
    let mut forged = Pointer::new(alice.node_id(), "wiki", "Home", 1, Some(cid(0xff)), 1);
    sign(&mut forged, &mallory);

    // It verifies against Mallory's key in the arithmetic sense...
    assert_eq!(forged.verify(&key_of(&mallory)), Err(PointerError::WrongKey));
    // ...and against Alice's it fails on the signature. Neither path accepts it.
    assert_eq!(forged.verify(&key_of(&alice)), Err(PointerError::BadSignature));
}

/// A valid record for one name may not be served under another.
#[test]
fn a_pointer_cannot_be_lifted_into_a_different_name_or_service() {
    let alice = identity(8);
    let key = key_of(&alice);
    let mut log = SequenceLog::new();

    let home = published(&alice, "Home", 1, Some(&cid(0x0a)));
    home.verify(&key).expect("genuine");

    // Asked for a different page.
    let asked_other_page = PointerKey {
        node_id: alice.node_id().to_text(),
        service: "wiki".into(),
        name: "Recipes".into(),
    };
    assert!(matches!(
        log.accept(&home, &key, &asked_other_page),
        Err(PointerError::WrongPointer { .. })
    ));

    // Asked for the same name in a different service.
    let asked_other_service = PointerKey {
        node_id: alice.node_id().to_text(),
        service: "profile".into(),
        name: "Home".into(),
    };
    assert!(matches!(
        log.accept(&home, &key, &asked_other_service),
        Err(PointerError::WrongPointer { .. })
    ));
    assert!(log.is_empty(), "a refused record must not be remembered");
}

/// A tombstone is an ordinary update that happens to point at nothing.
#[test]
fn deletion_is_a_signed_record_and_cannot_be_rolled_back() {
    let alice = identity(9);
    let key = key_of(&alice);
    let asked = PointerKey {
        node_id: alice.node_id().to_text(),
        service: "wiki".into(),
        name: "Home".into(),
    };
    let mut log = SequenceLog::new();

    let live = published(&alice, "Home", 1, Some(&cid(0x0b)));
    log.accept(&live, &key, &asked).expect("published");
    assert!(!live.is_tombstone());

    let gone = published(&alice, "Home", 2, None);
    gone.verify(&key)
        .expect("a tombstone is signed like anything else");
    assert!(gone.is_tombstone());
    assert!(matches!(
        log.accept(&gone, &key, &asked).unwrap(),
        Accepted::Advanced { from: 1, to: 2 }
    ));

    // Serving the old live version again after deletion is exactly the rollback case.
    assert!(matches!(
        log.accept(&live, &key, &asked),
        Err(PointerError::Rollback { seen: 2, offered: 1 })
    ));
}

/// A future timestamp does not beat a higher sequence.
///
/// ADR-0027 §2. `published_at_ms` is signed and shown to people, and it orders nothing —
/// otherwise the winner would be whoever's clock is furthest ahead, which on a mesh with no
/// NTP guarantee is not a rare case.
#[test]
fn a_future_timestamp_does_not_win_against_a_higher_sequence() {
    let alice = identity(10);
    let key = key_of(&alice);
    let asked = PointerKey {
        node_id: alice.node_id().to_text(),
        service: "wiki".into(),
        name: "Home".into(),
    };
    let mut log = SequenceLog::new();

    let real = published(&alice, "Home", 10, Some(&cid(0x10)));
    log.accept(&real, &key, &asked).expect("current");

    // Sequence 9, but dated a century from now, and correctly signed.
    let mut backdated = Pointer::new(
        alice.node_id(),
        "wiki",
        "Home",
        9,
        Some(cid(0x09)),
        4_000_000_000_000,
    );
    sign(&mut backdated, &alice);
    backdated.verify(&key).expect("genuinely signed");
    assert!(
        backdated.published_at_ms > real.published_at_ms,
        "the test needs the fake to look newer by clock"
    );

    assert!(
        matches!(
            log.accept(&backdated, &key, &asked),
            Err(PointerError::Rollback { .. })
        ),
        "a later timestamp beat a higher sequence"
    );
}

/// Structural rules hold whoever signed the record.
#[test]
fn a_malformed_record_is_refused_before_anything_else() {
    let alice = identity(11);
    let key = key_of(&alice);

    for (why, mutate) in [
        (
            "sequence zero",
            (|p: &mut Pointer| p.sequence = 0) as fn(&mut Pointer),
        ),
        ("empty name", |p: &mut Pointer| p.name = String::new()),
        ("uppercase service", |p: &mut Pointer| p.service = "Wiki".into()),
        ("short content id", |p: &mut Pointer| {
            p.content_id = Some("abc".into())
        }),
        ("uppercase content id", |p: &mut Pointer| {
            p.content_id = Some("A".repeat(64))
        }),
        ("wrong schema", |p: &mut Pointer| {
            p.schema_version = "9.9.9".into()
        }),
    ] {
        let mut p = Pointer::new(alice.node_id(), "wiki", "Home", 1, Some(cid(0x01)), 1);
        mutate(&mut p);
        // Signed *after* the mutation, so the signature is genuine and only the shape is
        // wrong. Otherwise this would be testing the signature check a second time.
        sign(&mut p, &alice);
        assert!(
            matches!(p.verify(&key), Err(PointerError::Malformed(_))),
            "{why} was accepted"
        );
    }
}

/// The log tracks pointers independently.
#[test]
fn one_pointer_advancing_does_not_move_another() {
    let alice = identity(12);
    let key = key_of(&alice);
    let mut log = SequenceLog::new();

    let home = PointerKey {
        node_id: alice.node_id().to_text(),
        service: "wiki".into(),
        name: "Home".into(),
    };
    let recipes = PointerKey {
        node_id: alice.node_id().to_text(),
        service: "wiki".into(),
        name: "Recipes".into(),
    };

    log.accept(&published(&alice, "Home", 50, Some(&cid(0x50))), &key, &home)
        .expect("home");
    // Recipes at a much lower sequence is a first sight, not a rollback.
    assert_eq!(
        log.accept(&published(&alice, "Recipes", 1, Some(&cid(0x01))), &key, &recipes)
            .unwrap(),
        Accepted::FirstSeen
    );
    assert_eq!(log.highest_seen(&home), Some(50));
    assert_eq!(log.highest_seen(&recipes), Some(1));
    assert_eq!(log.len(), 2);
}

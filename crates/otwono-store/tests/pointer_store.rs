//! The pointer store's own rules (ADR-0027).
//!
//! `otwono-pointer` owns verification and the rollback comparison; this owns durability and
//! the on-disk layout. The properties worth testing here are the ones a filesystem can get
//! wrong: a name that is a path, two keys that collide, and a sequence log that has to
//! survive a restart to be worth anything.

use otwono_identity::NodeIdentity;
use otwono_pointer::{Accepted, Pointer, PointerKey};
use otwono_store::{PointerStore, PointerStoreError};

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("otw-ptr-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn identity() -> NodeIdentity {
    NodeIdentity::from_seeds(&[3; 32], &[3; 32], 1)
}

fn signed(id: &NodeIdentity, service: &str, name: &str, sequence: u64) -> Pointer {
    let mut p = Pointer::new(
        id.node_id(),
        service,
        name,
        sequence,
        Some("ab".repeat(32)),
        1_700_000_000_000,
    );
    let payload = p.payload_for_id_sign().unwrap();
    p.signature =
        data_encoding::BASE64.encode(&id.sign(&otwono_identity::domain_separated(&payload)).to_bytes());
    p
}

/// A name that is a path must not become one.
///
/// 512 bytes of anything is a valid pointer name, and this one arrives from a stranger. If
/// the name reached the filesystem, publishing it would overwrite the node's own policy.
#[test]
fn a_name_that_looks_like_a_path_stays_inside_the_store() {
    let dir = tmp("traversal");
    let store = PointerStore::at(&dir).unwrap();
    let id = identity();

    let hostile = "../../../../etc/otwono/policy.d/10-default.toml";
    store
        .publish(&signed(&id, "wiki", hostile, 1))
        .expect("a hostile name is stored, not refused");

    // Nothing was written outside the store, and the record is readable back by its name.
    let escaped = dir.join("../../../../etc/otwono/policy.d/10-default.toml");
    assert!(!escaped.exists(), "a pointer name escaped the store directory");
    let back = store.mine("wiki", hostile).unwrap().expect("readable back");
    assert_eq!(back.name, hostile);

    // Every file that exists is a flat hashed name inside `mine`.
    for entry in std::fs::read_dir(dir.join("mine")).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(
            name.ends_with(".json") && name.len() == 69 && !name.contains('/'),
            "unexpected file in the store: {name}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two keys that concatenate the same way must not share a file.
#[test]
fn keys_that_would_collide_under_naive_joining_do_not() {
    let dir = tmp("collide");
    let store = PointerStore::at(&dir).unwrap();
    let id = identity();

    // ("wiki", "a/b") and ("wiki-a", "b") are different pointers whose parts run together
    // under a careless join.
    store.publish(&signed(&id, "wiki", "a/b", 1)).unwrap();
    store.publish(&signed(&id, "wiki-a", "b", 1)).unwrap();

    assert_eq!(store.mine("wiki", "a/b").unwrap().unwrap().name, "a/b");
    assert_eq!(store.mine("wiki-a", "b").unwrap().unwrap().name, "b");
    assert_eq!(store.published().unwrap().len(), 2, "one overwrote the other");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The store assigns the next sequence, so a caller cannot regress by losing track.
#[test]
fn the_store_knows_what_the_next_sequence_is() {
    let dir = tmp("next");
    let store = PointerStore::at(&dir).unwrap();
    let id = identity();

    assert_eq!(store.next_sequence("wiki", "Home").unwrap(), 1);
    store.publish(&signed(&id, "wiki", "Home", 1)).unwrap();
    assert_eq!(store.next_sequence("wiki", "Home").unwrap(), 2);
    // A different name is independent.
    assert_eq!(store.next_sequence("wiki", "Other").unwrap(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Publishing backwards is refused, because it is unrecoverable.
///
/// A pointer that regresses is unreadable to every peer that already saw the higher number,
/// permanently — they will refuse it as a rollback and be right to. So the owner is stopped
/// here rather than discovering it from peers who have gone quiet.
#[test]
fn a_node_cannot_publish_backwards_over_its_own_pointer() {
    let dir = tmp("regress");
    let store = PointerStore::at(&dir).unwrap();
    let id = identity();

    store.publish(&signed(&id, "wiki", "Home", 5)).unwrap();
    for going_backwards in [4, 5] {
        assert!(
            matches!(
                store.publish(&signed(&id, "wiki", "Home", going_backwards)),
                Err(PointerStoreError::WouldRegress { published: 5, .. })
            ),
            "sequence {going_backwards} was published over 5"
        );
    }
    store
        .publish(&signed(&id, "wiki", "Home", 6))
        .expect("forwards is fine");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The rollback log survives a restart, or it is worth nothing.
///
/// This is the property that makes the defence real rather than per-process. A node that
/// forgot every sequence on reboot would drop back to first-use trust every time it
/// restarted, which is most of the protection gone for an attacker who can wait.
#[test]
fn what_a_peer_published_is_still_remembered_after_a_restart() {
    let dir = tmp("restart");
    let alice = identity();
    let key = alice.public_key_bytes();
    let asked = PointerKey {
        node_id: alice.node_id().to_text(),
        service: "wiki".into(),
        name: "Home".into(),
    };

    {
        let store = PointerStore::at(&dir).unwrap();
        assert_eq!(
            store
                .accept_from_peer(&signed(&alice, "wiki", "Home", 9), &key, &asked)
                .unwrap(),
            Accepted::FirstSeen
        );
        assert_eq!(store.highest_seen(&asked), Some(9));
    }

    // A new store over the same directory: a fresh process, same disk.
    let reopened = PointerStore::at(&dir).unwrap();
    assert_eq!(
        reopened.highest_seen(&asked),
        Some(9),
        "the sequence log did not survive a restart"
    );
    assert!(
        reopened
            .accept_from_peer(&signed(&alice, "wiki", "Home", 8), &key, &asked)
            .is_err(),
        "a rollback was accepted after a restart"
    );
    assert_eq!(reopened.from_peer(&asked).unwrap().unwrap().sequence, 9);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A record from a peer and one of our own with the same name do not collide.
#[test]
fn our_own_pointer_and_a_peers_with_the_same_name_are_separate() {
    let dir = tmp("separate");
    let store = PointerStore::at(&dir).unwrap();
    let mine = NodeIdentity::from_seeds(&[1; 32], &[1; 32], 1);
    let theirs = NodeIdentity::from_seeds(&[2; 32], &[2; 32], 1);

    store.publish(&signed(&mine, "wiki", "Home", 1)).unwrap();
    let asked = PointerKey {
        node_id: theirs.node_id().to_text(),
        service: "wiki".into(),
        name: "Home".into(),
    };
    store
        .accept_from_peer(
            &signed(&theirs, "wiki", "Home", 1),
            &theirs.public_key_bytes(),
            &asked,
        )
        .unwrap();

    assert_eq!(
        store.mine("wiki", "Home").unwrap().unwrap().node_id,
        mine.node_id().to_text()
    );
    assert_eq!(
        store.from_peer(&asked).unwrap().unwrap().node_id,
        theirs.node_id().to_text()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A damaged record does not stop the node describing what else it publishes.
#[test]
fn one_unreadable_record_does_not_break_the_listing() {
    let dir = tmp("damaged");
    let store = PointerStore::at(&dir).unwrap();
    let id = identity();
    store.publish(&signed(&id, "wiki", "Good", 1)).unwrap();
    store.publish(&signed(&id, "wiki", "Bad", 1)).unwrap();

    // Corrupt one of them on disk.
    let victim = std::fs::read_dir(dir.join("mine"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    std::fs::write(victim.path(), b"{ not json").unwrap();

    let listed = store.published().unwrap();
    assert_eq!(listed.len(), 1, "a damaged record took the listing with it");
    let _ = std::fs::remove_dir_all(&dir);
}

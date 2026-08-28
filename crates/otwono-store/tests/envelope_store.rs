//! What a carrier's custody store must get right (ADR-0028).
//!
//! `otwono-envelope` owns the custody *rules*; this owns durability, scoping and the on-disk
//! layout. The properties worth testing here are the ones a filesystem or a careless filter
//! can get wrong: a record that does not survive a restart, a sweep that keeps what it should
//! drop, and a scoped listing that leaks somebody else's mail.

use otwono_envelope::{CarryPolicy, Declined, Envelope};
use otwono_identity::NodeIdentity;
use otwono_store::EnvelopeStore;

const NOW: u64 = 1_700_000_000_000;

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("otw-env-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

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

fn for_node(seed: u8, id_byte: u8, expires: u64) -> Envelope {
    Envelope::new(&cid(id_byte), node(seed).node_id(), 4096, expires)
}

/// Custody survives a restart, or the carrier silently loses somebody's mail.
///
/// The property the whole store exists for. A carrier that forgot its custody on reboot would
/// drop every envelope it held, and — because ADR-0028 §5 has no acknowledgement — neither the
/// sender nor the recipient would ever learn it happened.
#[test]
fn custody_survives_a_restart() {
    let dir = tmp("restart");
    let envelope = for_node(1, 0x0a, NOW + 60 * 60 * 1000);

    {
        let store = EnvelopeStore::at(&dir).unwrap();
        store
            .take(&envelope, &policy(), NOW)
            .unwrap()
            .expect("a well-formed envelope inside budget is taken");
        assert_eq!(store.held(NOW).unwrap().len(), 1);
    }

    // A new store over the same directory: a fresh process, same disk.
    let reopened = EnvelopeStore::at(&dir).unwrap();
    let held = reopened.held(NOW).unwrap();
    assert_eq!(held.len(), 1, "custody did not survive a restart");
    assert_eq!(held[0].envelope.envelope_id, envelope.envelope_id);
    assert_eq!(held[0].until_ms, NOW + 60 * 60 * 1000);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The scoped listing shows one recipient's mail and nobody else's (ADR-0028 §9).
///
/// The leak this store exists to prevent. A collection path that returned the whole bag would
/// let any peer enumerate every recipient the carrier serves, which is strictly more than §4
/// says a relay learns.
#[test]
fn a_recipient_sees_only_its_own_mail() {
    let dir = tmp("scoped");
    let store = EnvelopeStore::at(&dir).unwrap();
    let alice = node(1);
    let bob = node(2);

    store
        .take(&for_node(1, 0x0a, NOW + 10_000), &policy(), NOW)
        .unwrap()
        .unwrap();
    store
        .take(&for_node(1, 0x0b, NOW + 10_000), &policy(), NOW)
        .unwrap()
        .unwrap();
    store
        .take(&for_node(2, 0x0c, NOW + 10_000), &policy(), NOW)
        .unwrap()
        .unwrap();

    assert_eq!(store.held(NOW).unwrap().len(), 3, "the carrier holds three");
    let hers = store.held_for(alice.node_id(), NOW).unwrap();
    assert_eq!(hers.len(), 2);
    assert!(hers.iter().all(|c| c.envelope.is_for(alice.node_id())));
    assert_eq!(store.held_for(bob.node_id(), NOW).unwrap().len(), 1);

    // A node the carrier holds nothing for gets an empty list, not an error — the same
    // answer a node that carries nothing gives, so asking cannot reveal whether it carries.
    assert!(store.held_for(node(9).node_id(), NOW).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A lapsed envelope is swept, and swept means gone from the disk.
///
/// Not merely filtered out of the listing: an envelope past its deadline still occupying the
/// budget would make a carrier's capacity decay with nothing to show for it.
#[test]
fn a_lapsed_envelope_is_deleted_rather_than_hidden() {
    let dir = tmp("sweep");
    let store = EnvelopeStore::at(&dir).unwrap();
    store
        .take(&for_node(1, 0x0a, NOW + 1000), &policy(), NOW)
        .unwrap()
        .unwrap();
    store
        .take(&for_node(1, 0x0b, NOW + 90_000), &policy(), NOW)
        .unwrap()
        .unwrap();

    let files = || std::fs::read_dir(dir.join("held")).unwrap().count();
    assert_eq!(files(), 2);
    assert_eq!(store.held(NOW).unwrap().len(), 2);
    assert_eq!(store.bytes_held(NOW).unwrap(), 8192);

    // Past the first one's deadline.
    assert_eq!(store.held(NOW + 1000).unwrap().len(), 1);
    assert_eq!(
        files(),
        1,
        "a lapsed record was hidden from the listing but left on disk"
    );
    assert_eq!(store.bytes_held(NOW + 1000).unwrap(), 4096);
    assert!(store.get(&cid(0x0a), NOW + 1000).unwrap().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Releasing custody is idempotent, because delivery races a sweep.
#[test]
fn releasing_custody_twice_is_not_an_error() {
    let dir = tmp("release");
    let store = EnvelopeStore::at(&dir).unwrap();
    let envelope = for_node(1, 0x0a, NOW + 10_000);
    store.take(&envelope, &policy(), NOW).unwrap().unwrap();

    store.release(&envelope.envelope_id).expect("first release");
    store
        .release(&envelope.envelope_id)
        .expect("a second release must not fail");
    store
        .release(&cid(0xee))
        .expect("releasing what was never held must not fail");
    assert!(store.held(NOW).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A carrier at its budget refuses cleanly and says which limit bit.
#[test]
fn a_full_carrier_refuses_rather_than_crashing() {
    let dir = tmp("full");
    let store = EnvelopeStore::at(&dir).unwrap();
    let tight = CarryPolicy {
        room_bytes: 100,
        max_size_bytes: 64 << 10,
        max_hold_ms: 1000,
    };
    let refusal = store
        .take(&for_node(1, 0x0a, NOW + 500), &tight, NOW)
        .unwrap()
        .expect_err("4096 bytes must not fit in 100");
    assert!(matches!(refusal, Declined::NoRoom { .. }), "{refusal:?}");
    assert!(
        store.held(NOW).unwrap().is_empty(),
        "a refused envelope was stored anyway"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A damaged record does not take the listing with it, and does not linger.
#[test]
fn one_unreadable_record_does_not_break_the_listing() {
    let dir = tmp("damaged");
    let store = EnvelopeStore::at(&dir).unwrap();
    store
        .take(&for_node(1, 0x0a, NOW + 10_000), &policy(), NOW)
        .unwrap()
        .unwrap();
    store
        .take(&for_node(1, 0x0b, NOW + 10_000), &policy(), NOW)
        .unwrap()
        .unwrap();

    let victim = std::fs::read_dir(dir.join("held"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    std::fs::write(victim.path(), b"{ not json").unwrap();

    assert_eq!(
        store.held(NOW).unwrap().len(),
        1,
        "a damaged record took the listing with it"
    );
    assert_eq!(
        std::fs::read_dir(dir.join("held")).unwrap().count(),
        1,
        "a record that cannot be read cannot be delivered, so it must not keep its budget"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// No temporary files are left behind by the durable write.
#[test]
fn the_durable_write_leaves_no_litter() {
    let dir = tmp("litter");
    let store = EnvelopeStore::at(&dir).unwrap();
    for b in 0u8..8 {
        store
            .take(&for_node(1, b, NOW + 10_000), &policy(), NOW)
            .unwrap()
            .unwrap();
    }
    for entry in std::fs::read_dir(dir.join("held")).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(name.ends_with(".json"), "leftover temporary file: {name}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

//! What a wiki page's chain must refuse (ADR-0032).

use otwono_identity::NodeIdentity;
use otwono_wiki::{walk, Revision, WalkEnd, WikiError};
use std::collections::HashMap;

fn cid(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn signed(who: &NodeIdentity, page: &str, body: u8, parent: Option<String>, at: u64) -> Revision {
    let mut r = Revision::new(who.node_id(), page, cid(body), parent, at);
    r.signature = data_encoding::BASE64.encode(&who.sign(&r.signing_bytes().unwrap()).to_bytes());
    r
}

/// A store, and the id each revision is filed under.
///
/// Real content ids are the hash of the encoded revision; here they are assigned, because
/// what these tests are about is the chain's rules and not the hashing, which `otwono-store`
/// owns and tests.
#[derive(Default)]
struct Shelf(HashMap<String, Revision>);

impl Shelf {
    fn put(&mut self, id: u8, r: Revision) -> String {
        let id = cid(id);
        self.0.insert(id.clone(), r);
        id
    }
}

impl otwono_wiki::Revisions for Shelf {
    fn get(&self, content_id: &str) -> Option<Revision> {
        self.0.get(content_id).cloned()
    }
}

fn keys_of(who: &NodeIdentity) -> impl Fn(&str) -> Option<[u8; 32]> + '_ {
    move |author| (author == who.node_id().to_text()).then(|| who.verifying_key().to_bytes())
}

#[test]
fn a_page_reads_back_as_its_history_newest_first() {
    let me = NodeIdentity::generate().unwrap();
    let mut shelf = Shelf::default();
    let first = shelf.put(0x01, signed(&me, "Getting-Started", 0xaa, None, 1_000));
    let second = shelf.put(
        0x02,
        signed(&me, "Getting-Started", 0xbb, Some(first.clone()), 2_000),
    );
    let head = shelf.put(
        0x03,
        signed(&me, "Getting-Started", 0xcc, Some(second.clone()), 3_000),
    );

    let h = walk(&shelf, &head, "Getting-Started", keys_of(&me), 64).expect("a good chain");
    assert_eq!(
        h.end,
        WalkEnd::Complete,
        "the walk did not reach the first revision"
    );
    assert_eq!(
        h.steps.iter().map(|s| s.content_id.as_str()).collect::<Vec<_>>(),
        vec![head.as_str(), second.as_str(), first.as_str()],
        "history must come back head first"
    );
    assert!(h.steps.last().unwrap().revision.is_first());
}

#[test]
fn an_ancestor_nobody_signed_is_refused_even_though_the_head_verifies() {
    // The reason every revision is signed and not only the head. A peer serves the chain, and
    // the pointer's signature covers the head alone — so an unsigned ancestor would be shown
    // as the author's earlier words on nothing but that peer's say-so.
    let me = NodeIdentity::generate().unwrap();
    let mut shelf = Shelf::default();
    let mut forged = Revision::new(me.node_id(), "Getting-Started", cid(0xaa), None, 1_000);
    forged.signature = String::new();
    let first = shelf.put(0x01, forged);
    let head = shelf.put(0x02, signed(&me, "Getting-Started", 0xbb, Some(first), 2_000));

    let err = walk(&shelf, &head, "Getting-Started", keys_of(&me), 64)
        .expect_err("an unsigned ancestor must be refused");
    assert_eq!(err, WikiError::BadSignature, "{err}");
}

#[test]
fn an_ancestor_signed_by_somebody_else_claiming_the_authors_name_is_refused() {
    // The identity half. A NodeID is a hash of a public key, so an attacker signs with their
    // own key and presents it; only the binding between key and claimed NodeID catches that.
    let me = NodeIdentity::generate().unwrap();
    let them = NodeIdentity::generate().unwrap();

    // Their revision, but wearing my NodeID.
    let mut impostor = Revision::new(me.node_id(), "Getting-Started", cid(0xaa), None, 1_000);
    impostor.signature =
        data_encoding::BASE64.encode(&them.sign(&impostor.signing_bytes().unwrap()).to_bytes());

    let mut shelf = Shelf::default();
    let first = shelf.put(0x01, impostor);
    let head = shelf.put(0x02, signed(&me, "Getting-Started", 0xbb, Some(first), 2_000));

    let err = walk(&shelf, &head, "Getting-Started", keys_of(&me), 64)
        .expect_err("a revision signed by the wrong key must be refused");
    assert_eq!(err, WikiError::BadSignature, "{err}");
}

#[test]
fn a_revision_of_another_page_cannot_be_spliced_into_this_ones_history() {
    // Why `page` is inside the signed record. Both of these are genuinely signed by the same
    // author; the second is simply not part of the page being read, and without the field a
    // peer could pad any history with the author's writing from anywhere.
    let me = NodeIdentity::generate().unwrap();
    let mut shelf = Shelf::default();
    let elsewhere = shelf.put(0x01, signed(&me, "Some-Other-Page", 0xaa, None, 1_000));
    let head = shelf.put(0x02, signed(&me, "Getting-Started", 0xbb, Some(elsewhere), 2_000));

    let err = walk(&shelf, &head, "Getting-Started", keys_of(&me), 64)
        .expect_err("a revision of another page must be refused");
    assert!(
        matches!(err, WikiError::WrongPage { ref found, .. } if found == "Some-Other-Page"),
        "{err}"
    );
}

#[test]
fn a_history_that_loops_is_an_error_and_not_a_hang() {
    // `parent` is not trusted to be older, because nothing in the record can prove that. A
    // reader that assumed it would follow a loop for as long as a peer kept answering.
    let me = NodeIdentity::generate().unwrap();
    let mut shelf = Shelf::default();
    let a = cid(0x01);
    let b = cid(0x02);
    shelf.0.insert(
        a.clone(),
        signed(&me, "Getting-Started", 0xaa, Some(b.clone()), 1_000),
    );
    shelf.0.insert(
        b.clone(),
        signed(&me, "Getting-Started", 0xbb, Some(a.clone()), 2_000),
    );

    let err = walk(&shelf, &a, "Getting-Started", keys_of(&me), 64).expect_err("a loop must be refused");
    assert!(matches!(err, WikiError::Cycle(_)), "{err}");
}

#[test]
fn a_head_whose_ancestors_are_not_here_reads_as_truncated_rather_than_broken() {
    // The ordinary case for a reader that has just fetched somebody's page: it has the head
    // and none of the history. That is not a fault, and a walk that returned a bare list
    // could not tell it apart from a page with one revision.
    let me = NodeIdentity::generate().unwrap();
    let mut shelf = Shelf::default();
    let absent = cid(0xff);
    let head = shelf.put(
        0x02,
        signed(&me, "Getting-Started", 0xbb, Some(absent.clone()), 2_000),
    );

    let h = walk(&shelf, &head, "Getting-Started", keys_of(&me), 64).expect("not an error");
    assert_eq!(h.steps.len(), 1);
    assert_eq!(h.end, WalkEnd::Truncated { missing: absent });
}

#[test]
fn a_walk_stops_at_the_callers_limit() {
    // How long a page's history is is the serving peer's choice, so a reader needs a bound.
    let me = NodeIdentity::generate().unwrap();
    let mut shelf = Shelf::default();
    let mut parent = None;
    let mut head = String::new();
    for i in 1..=10u8 {
        head = shelf.put(
            i,
            signed(&me, "Getting-Started", i, parent.clone(), i as u64 * 100),
        );
        parent = Some(head.clone());
    }

    let h = walk(&shelf, &head, "Getting-Started", keys_of(&me), 4).expect("a good chain");
    assert_eq!(h.steps.len(), 4);
    assert_eq!(h.end, WalkEnd::Limited, "a bounded walk must say it was bounded");
}

#[test]
fn an_author_this_reader_has_no_key_for_is_refused_rather_than_skipped() {
    // "Verify it later" is "never". A page copied from elsewhere keeps its original author,
    // so a chain can legitimately change hands — and the reader must then have that author's
    // key, not wave the revision through.
    let me = NodeIdentity::generate().unwrap();
    let them = NodeIdentity::generate().unwrap();
    let mut shelf = Shelf::default();
    let theirs = shelf.put(0x01, signed(&them, "Getting-Started", 0xaa, None, 1_000));
    let head = shelf.put(0x02, signed(&me, "Getting-Started", 0xbb, Some(theirs), 2_000));

    let err = walk(&shelf, &head, "Getting-Started", keys_of(&me), 64)
        .expect_err("an author with no key must be refused");
    assert_eq!(err, WikiError::WrongKey, "{err}");
}

#[test]
fn a_signature_over_a_pointer_cannot_be_replayed_as_a_revision() {
    // Domain separation. Both records are canonical JSON over similar-looking fields, which
    // is exactly when a shared domain lets a verifier accept the wrong kind of thing.
    let me = NodeIdentity::generate().unwrap();
    let r = Revision::new(me.node_id(), "Getting-Started", cid(0xaa), None, 1_000);
    let bytes = r.signing_bytes().unwrap();
    assert!(
        bytes
            .windows(otwono_wiki::WIKI_REVISION_DOMAIN.len())
            .any(|w| w == otwono_wiki::WIKI_REVISION_DOMAIN),
        "the revision domain must be in what gets signed"
    );
    assert!(
        !bytes.windows(18).any(|w| w == b"otwono-pointer-v1:"),
        "a revision must not be signed under the pointer's domain"
    );
}

#[test]
fn a_page_name_that_would_break_a_listing_is_refused() {
    // Refused in the record rather than escaped at each display, because there will be more
    // than one display. A slash would make one page look like a path under another.
    let me = NodeIdentity::generate().unwrap();
    for bad in ["", "with/slash", "line\nbreak", "bell\u{7}"] {
        let r = Revision::new(me.node_id(), bad, cid(0xaa), None, 1_000);
        assert!(
            r.check_shape().is_err(),
            "page name {bad:?} was accepted and should not be"
        );
    }
    let long = "n".repeat(otwono_wiki::MAX_PAGE_NAME_BYTES + 1);
    assert!(Revision::new(me.node_id(), long, cid(0xaa), None, 1_000)
        .check_shape()
        .is_err());
    assert!(
        Revision::new(me.node_id(), "Getting-Started", cid(0xaa), None, 1_000)
            .check_shape()
            .is_ok()
    );
}

#[test]
fn the_two_signing_payloads_differ_by_exactly_the_application_domain() {
    // `id.sign` prepends the application domain; an in-process signer does not. A caller
    // that reached for the wrong one would make a signature nothing can verify, on a record
    // that looks perfectly well formed — so the relationship between them is pinned here
    // rather than left to two functions that happen to agree today.
    let me = NodeIdentity::generate().unwrap();
    let r = signed(&me, "Getting-Started", 0xaa, None, 1_000);
    let full = r.signing_bytes().unwrap();
    let for_daemon = r.payload_for_id_sign().unwrap();
    assert_eq!(
        full,
        [otwono_identity::APPLICATION_DOMAIN, &for_daemon[..]].concat(),
        "signing_bytes must be the application domain followed by the id.sign payload"
    );
}

#[test]
fn a_revision_signed_through_the_daemons_payload_verifies() {
    // The round trip the CLI actually performs: sign `payload_for_id_sign` with the
    // application domain prepended, exactly as otwono-idd does, and check `verify` accepts
    // it. Asserting the two payloads line up is not the same as asserting a signature made
    // that way is accepted.
    let me = NodeIdentity::generate().unwrap();
    let mut r = Revision::new(me.node_id(), "Getting-Started", cid(0xaa), None, 1_000);
    let as_the_daemon_would = [
        otwono_identity::APPLICATION_DOMAIN,
        &r.payload_for_id_sign().unwrap()[..],
    ]
    .concat();
    r.signature = data_encoding::BASE64.encode(&me.sign(&as_the_daemon_would).to_bytes());
    r.verify(&me.verifying_key().to_bytes()).expect("must verify");
}

//! Wiki pages as signed chains of revisions (ADR-0032).
//!
//! The first service composed from the three primitives rather than a fourth one. A page's
//! current state is a **signed mutable pointer** (ADR-0027) under the service namespace
//! `wiki`; what it names is a [`Revision`], and each revision names its parent, so a page is
//! a chain and the pointer names its head. The text itself is an ordinary
//! **content-addressed block** (ADR-0016) named by the revision.
//!
//! # One writer, by construction
//!
//! `onm://<nodeid>/wiki/<path>` puts a page in one node's namespace, and a pointer has
//! exactly one writer (ADR-0027 §2). So a page has exactly one author and there is no
//! concurrent-write conflict to resolve: node B's copy of node A's page is a *different*
//! page under a different NodeID. `DISTRIBUTED-SERVICES.md` §2's "last-writer-wins with
//! explicit merge" describes a shared document, which needs either a designated owner
//! merging proposals or a CRDT — ADR-0032 §7 leaves both open and neither is here.
//!
//! # Why every revision is signed and not just the head
//!
//! The pointer's signature vouches for the head. History is walked by fetching parents, and
//! a peer serving the chain could otherwise substitute an ancestor: the reader would verify
//! the head, follow an unsigned link, and show a fabricated earlier version as the author's
//! words. Content addressing stops the bytes being *altered* — a substituted parent has a
//! different id — but an id is only as good as whatever vouches for it, and nothing does
//! once you step off the head.
//!
//! # What this crate is not
//!
//! No I/O, no network, no storage. It is the record and its rules, so that both can be
//! tested without a daemon (`DISTRIBUTED-SERVICES.md` §4.5). Publishing and reading are
//! `store.put` plus `pointer.publish`, and `content.pointer` plus the fetch path — all of
//! which exist already.

#![forbid(unsafe_code)]

use otwono_identity::{canonical_json, NodeId, APPLICATION_DOMAIN};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: &str = "1.0.0";

/// Domain separation for a revision's signature.
///
/// Distinct from the pointer's and every other record's, so a signature made over one kind
/// of OTWONO record can never be replayed as another. Both records are canonical JSON over
/// similar-looking fields, which is exactly the situation where a shared domain would let a
/// verifier accept the wrong thing.
pub const WIKI_REVISION_DOMAIN: &[u8] = b"otwono-wiki-revision-v1:";

/// How long a page name may be, in bytes.
///
/// Shorter than a pointer's 512 because this one is also a path segment in
/// `onm://<nodeid>/wiki/<path>` and is shown in a list. The limit exists so that a name
/// cannot be used to make a listing unreadable.
pub const MAX_PAGE_NAME_BYTES: usize = 128;

/// One revision of one page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    pub schema_version: String,
    /// The page this belongs to, matching the pointer's name.
    ///
    /// Inside the signed record rather than inferred from where the revision was found. A
    /// revision lifted from one page and served under another would otherwise still verify,
    /// which is the mistake ADR-0027 §6 refuses for a pointer's service namespace.
    pub page: String,
    /// Content id of the text.
    pub body: String,
    /// Content id of the previous revision; absent on the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Who wrote it, in full text form.
    pub author: String,
    /// The author's wall clock. Shown to people; **never** used for ordering.
    pub written_at_ms: u64,
    /// Base64 Ed25519. Empty until signed.
    #[serde(default)]
    pub signature: String,
}

impl Revision {
    /// A new, unsigned revision.
    pub fn new(
        author: &NodeId,
        page: impl Into<String>,
        body: impl Into<String>,
        parent: Option<String>,
        written_at_ms: u64,
    ) -> Revision {
        Revision {
            schema_version: SCHEMA_VERSION.to_string(),
            page: page.into(),
            body: body.into(),
            parent,
            author: author.to_text(),
            written_at_ms,
            signature: String::new(),
        }
    }

    /// Whether this is the first revision of its page.
    pub fn is_first(&self) -> bool {
        self.parent.is_none()
    }

    /// The bytes a signature covers: `APPLICATION_DOMAIN || WIKI_REVISION_DOMAIN || canonical`.
    ///
    /// The application domain is included because that is what `id.sign` prepends before
    /// signing, so a verifier that omitted it would reject every genuine signature. Built
    /// here rather than by callers, so the signing path and the verifying path cannot
    /// disagree — the same reason ADR-0027 gives for the pointer.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, WikiError> {
        let mut value = serde_json::to_value(self).map_err(|e| WikiError::Encoding(e.to_string()))?;
        if let Some(obj) = value.as_object_mut() {
            // A signature cannot cover itself.
            obj.remove("signature");
        }
        let mut message = APPLICATION_DOMAIN.to_vec();
        message.extend_from_slice(WIKI_REVISION_DOMAIN);
        message.extend_from_slice(&canonical_json(&value));
        Ok(message)
    }

    /// The bytes to hand `id.sign`, which prepends the application domain itself.
    ///
    /// The same message as [`Self::signing_bytes`] minus that prefix. Two functions rather
    /// than one because the daemon adds it and an in-process signer does not, and a caller
    /// that used the wrong one would produce a signature nothing could verify — the failure
    /// would be a bad signature on a record that looks perfectly well formed.
    pub fn payload_for_id_sign(&self) -> Result<Vec<u8>, WikiError> {
        let mut value = serde_json::to_value(self).map_err(|e| WikiError::Encoding(e.to_string()))?;
        if let Some(obj) = value.as_object_mut() {
            obj.remove("signature");
        }
        let mut message = WIKI_REVISION_DOMAIN.to_vec();
        message.extend_from_slice(&canonical_json(&value));
        Ok(message)
    }

    /// Check the signature, and that the key offered is the author's.
    ///
    /// The second half is not optional and not the caller's job, for the reason ADR-0027
    /// gives: a NodeID is a hash of the public key, so the key cannot be recovered from it
    /// and must be supplied alongside — and a verifier that only checked the signature would
    /// accept any record from anyone, since an attacker signs with their own key and supplies
    /// it. The binding between key and claimed NodeID is the whole of the identity check.
    pub fn verify(&self, public_key: &[u8; 32]) -> Result<(), WikiError> {
        let claimed =
            NodeId::parse(&self.author).map_err(|e| WikiError::Malformed(format!("author: {e}")))?;
        if !claimed.matches_public_key(public_key) {
            return Err(WikiError::WrongKey);
        }
        self.check_shape()?;
        let signature = data_encoding::BASE64
            .decode(self.signature.as_bytes())
            .map_err(|e| WikiError::Malformed(format!("signature is not base64: {e}")))?;
        otwono_identity::verify_signature(public_key, &self.signing_bytes()?, &signature)
            .map_err(|_| WikiError::BadSignature)
    }

    /// Structural rules that hold regardless of who signed it.
    pub fn check_shape(&self) -> Result<(), WikiError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(WikiError::Malformed(format!(
                "schema_version {} is not {SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        if self.page.is_empty() || self.page.len() > MAX_PAGE_NAME_BYTES {
            return Err(WikiError::Malformed(format!(
                "page name must be 1..={MAX_PAGE_NAME_BYTES} bytes"
            )));
        }
        // A slash would make one page's name look like a path under another, and a control
        // character would let a name disappear or rewrite a line in any listing that shows
        // it. Refused in the record rather than escaped at each display, because there will
        // be more than one display.
        if self.page.contains('/') || self.page.chars().any(|c| c.is_control()) {
            return Err(WikiError::Malformed(
                "page name may not contain a slash or a control character".into(),
            ));
        }
        is_content_id(&self.body).map_err(|e| WikiError::Malformed(format!("body: {e}")))?;
        if let Some(parent) = &self.parent {
            is_content_id(parent).map_err(|e| WikiError::Malformed(format!("parent: {e}")))?;
        }
        Ok(())
    }
}

fn is_content_id(id: &str) -> Result<(), String> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(format!("{id:?} is not a 64-character lowercase hex digest"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WikiError {
    Malformed(String),
    Encoding(String),
    WrongKey,
    BadSignature,
    /// The chain revisited an id, so it is not a history.
    Cycle(String),
    /// A revision in the chain names a different page than the one being read.
    WrongPage {
        wanted: String,
        found: String,
    },
}

impl std::fmt::Display for WikiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WikiError::Malformed(why) => write!(f, "malformed revision: {why}"),
            WikiError::Encoding(why) => write!(f, "could not encode revision: {why}"),
            WikiError::WrongKey => write!(f, "the key offered is not the author's"),
            WikiError::BadSignature => write!(f, "the signature does not verify"),
            WikiError::Cycle(id) => write!(f, "the page's history revisits {id}, so it is not a history"),
            WikiError::WrongPage { wanted, found } => {
                write!(f, "a revision of {found:?} was served as part of {wanted:?}")
            }
        }
    }
}

impl std::error::Error for WikiError {}

/// Where a history walk gets revisions from.
///
/// A trait rather than a store handle, so the rules can be tested against a map and the same
/// code runs against a real fetch path (`DISTRIBUTED-SERVICES.md` §4.5). Returning `None` for
/// an id that is not here is not an error: a reader may hold a head whose ancestors it has
/// never fetched, and that is a *truncated* history rather than a broken one.
pub trait Revisions {
    fn get(&self, content_id: &str) -> Option<Revision>;
}

impl<F> Revisions for F
where
    F: Fn(&str) -> Option<Revision>,
{
    fn get(&self, content_id: &str) -> Option<Revision> {
        self(content_id)
    }
}

/// One step of a page's history, as a reader sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The content id this revision was fetched under.
    pub content_id: String,
    pub revision: Revision,
}

/// How a walk ended, which is as much of the answer as the steps are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkEnd {
    /// A revision with no parent: the page's first, and the whole history is here.
    Complete,
    /// A parent this reader does not have. Normal — a head can be read without its ancestors
    /// ever having been fetched — and the reason a walk reports how it ended rather than
    /// returning a bare list that cannot tell "all of it" from "as much as I have".
    Truncated { missing: String },
    /// The walk stopped at the caller's limit; there may be more.
    Limited,
}

/// The result of walking a page's history from its head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    /// Head first, oldest last, as far as the walk got.
    pub steps: Vec<Step>,
    pub end: WalkEnd,
}

/// Walk a page's history from `head`, verifying every step.
///
/// `author_key` resolves a NodeID to the public key to check a revision against. It takes the
/// claimed author because a page copied from elsewhere keeps its original author (ADR-0032),
/// so a chain can legitimately change hands partway down and a single key would reject it.
/// Returning `None` refuses the revision rather than skipping the check — an unknown author
/// is exactly the case where "verify it later" becomes "never".
///
/// Every revision is checked for signature *and* for naming the page being read. Verifying
/// only the head is what this exists to avoid: an ancestor is served by the same peer, is not
/// covered by the pointer's signature, and would otherwise be displayed as the author's words
/// on nothing but that peer's say-so.
///
/// `limit` bounds the walk. A page's history is served by a peer, so it is that peer's choice
/// how long it is, and a reader with no bound would follow it for as long as one kept
/// answering.
pub fn walk<R: Revisions, K>(
    revisions: &R,
    head: &str,
    page: &str,
    author_key: K,
    limit: usize,
) -> Result<History, WikiError>
where
    K: Fn(&str) -> Option<[u8; 32]>,
{
    let mut steps: Vec<Step> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut next = Some(head.to_string());

    while let Some(id) = next {
        if steps.len() >= limit {
            return Ok(History {
                steps,
                end: WalkEnd::Limited,
            });
        }
        // Checked before the fetch, not after: a cycle that was fetched first would already
        // have cost the round trip this is meant to bound.
        if seen.contains(&id) {
            return Err(WikiError::Cycle(id));
        }
        let Some(revision) = revisions.get(&id) else {
            return Ok(History {
                steps,
                end: WalkEnd::Truncated { missing: id },
            });
        };
        if revision.page != page {
            return Err(WikiError::WrongPage {
                wanted: page.to_string(),
                found: revision.page,
            });
        }
        let Some(key) = author_key(&revision.author) else {
            return Err(WikiError::WrongKey);
        };
        revision.verify(&key)?;

        seen.push(id.clone());
        next = revision.parent.clone();
        steps.push(Step {
            content_id: id,
            revision,
        });
    }

    Ok(History {
        steps,
        end: WalkEnd::Complete,
    })
}

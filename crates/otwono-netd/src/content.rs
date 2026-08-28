//! Serving content to peers, and fetching it from them (ADR-0017).
//!
//! # The label is checked twice, and the two checks are not the same code
//!
//! `otwono-stored` decides what may leave this node: it owns the store and it owns the
//! boundary. This module then checks the label again, in a different process, before a byte
//! reaches a link — which is what `DATA-VISIBILITY.md` §4 asks for.
//!
//! [`may_leave_a_node`] deliberately does **not** call
//! `otwono_store::Visibility::may_leave_the_node_unattended`, even though this crate now
//! depends on `otwono-store` for content addressing. A duplicated check that shares its
//! implementation duplicates nothing: one bug would pass both. So this one is written from
//! the other direction — an allow-list of the two strings that may appear, refusing
//! everything else including anything it does not recognise.
//!
//! # What this daemon can reach
//!
//! `store.serve` and nothing else. `otwono-netd` holds no `store.read` capability, so there
//! is no call it can make that returns a private object, however confused it becomes.
//!
//! # Roles are fixed for the life of a channel
//!
//! The node that dialled asks; the node that accepted answers. One request, one reply, in
//! order. That rules out a node fetching over a channel a peer opened to it — such a node
//! dials its own — and it is what keeps this a loop rather than a state machine.

use crate::HANDSHAKE_TIMEOUT;
use otwono_identity::NodeId;
use otwono_net::content::{
    self, ChunkEntry, ChunkPart, ManifestPage, ProtocolError, Request, Response, SharedEnvelope,
    SharedIndexEntry, SharedIndexPage,
};
use otwono_net::{LinkAdapter, LinkProperties, SecureChannel};
use otwono_proto::{code, Client};
use otwono_store::{ChunkRef, ContentId};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The labels this daemon will put on a link. Anything else — `private`, `shared`, a typo,
/// a label from a future version — is refused.
pub const SERVABLE_LABELS: [&str; 2] = ["public", "replicated"];

/// Largest object one `net.fetch` will assemble.
///
/// The same ceiling as `store.put`'s inline cap, and for the same reason: the reply carries
/// the object base64-encoded on one control-plane line, so an object this daemon cannot hand
/// back is one it should not have spent a peer's bandwidth fetching. That ceiling comes from
/// the transport (`otwono_proto::MAX_LINE_BYTES`), not from anything about content — see the
/// note on `otwono_stored::MAX_INLINE_BYTES`. A streaming interface is what lifts both.
pub const MAX_FETCH_BYTES: u64 = otwono_stored::MAX_INLINE_BYTES as u64;

/// Ceiling on an object's chunk count, so a peer cannot make this daemon allocate a list
/// before it has sent anything.
pub const MAX_FETCH_CHUNKS: usize = 65536;

/// Hard cap on round trips for one object, whatever the peer claims. Belt to the
/// no-progress braces: a peer that dribbles one byte per reply is not making progress in
/// any sense worth honouring.
pub const MAX_ROUND_TRIPS: usize = 100_000;

/// The independent half of the duplicated label check.
///
/// Written as an allow-list on purpose. `None`, an unknown string, and a label that has not
/// been invented yet all refuse — the same fail-closed default the store uses for an
/// unparseable label, arrived at by different code.
pub fn may_leave_a_node(label: Option<&str>) -> bool {
    matches!(label, Some(l) if SERVABLE_LABELS.contains(&l))
}

/// The same question for one named peer, which is the only way `shared` is ever true
/// (ADR-0019 §4).
///
/// `sharing` is the envelope the store attached to its reply. This does not take the store's
/// word for the decision: it checks that the sealed key in that envelope is addressed to the
/// peer *this daemon* authenticated through the Noise handshake. That is what keeps the two
/// label checks independent for `shared` as they already are for everything else — a store
/// that started attaching somebody else's copy would be caught here rather than putting it
/// on a link.
pub fn may_go_to_peer(label: Option<&str>, sharing: Option<&SharedEnvelope>, peer: &NodeId) -> bool {
    match (label, sharing) {
        // An envelope on an object that is not shared is a reply describing itself two ways.
        (Some(l), None) if SERVABLE_LABELS.contains(&l) => true,
        (Some("shared"), Some(envelope)) => envelope.sealed_key.recipient == peer.to_text(),
        _ => false,
    }
}

/// Answers peers' content requests out of the local store.
pub struct ContentResponder {
    store_socket: PathBuf,
    perm_socket: PathBuf,
    /// Cached `store.serve` capability, re-requested when the broker expires it.
    token: Mutex<Option<String>>,
}

impl std::fmt::Debug for ContentResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentResponder")
            .field("store", &self.store_socket)
            .finish_non_exhaustive()
    }
}

/// What one peer has been given during one session.
///
/// Only shared objects are tracked, and only that a manifest for them went out. It exists so
/// this daemon's check on a *chunk* of a shared object can be its own rather than an echo of
/// the store's: the store holds the recipient list, so a chunk reply cannot carry an
/// independent proof of authorization without repeating the whole envelope on every chunk.
/// What this daemon can say by itself is "I have already given this peer, in this session,
/// the manifest and sealed key for this object" — and if it has not, the chunk does not go
/// out, whatever the store answered.
///
/// Per session, not per peer, and never persisted: a session is the unit the Noise handshake
/// authenticated, and remembering across sessions would mean trusting an old decision made
/// against a key that may since have been rotated.
#[derive(Debug, Default)]
pub struct Session {
    shared_released: std::collections::HashSet<String>,
    /// What this node is willing to have copied, taken once and paged from thereafter
    /// (ADR-0026 §7). Same once-per-session discipline as `shared_index`, for the same
    /// reason: producing it scans every object record.
    replicable_index: Option<Vec<otwono_net::content::ReplicableEntry>>,
    /// What this node has sealed to this peer, taken once and paged from thereafter
    /// (ADR-0020). `None` until the peer first asks.
    ///
    /// Once per session, because producing it means scanning every object record, and a peer
    /// that could force a fresh scan per request would have a cheap way to make an
    /// SD-card-backed node miserable. A peer wanting a fresher answer opens a new session,
    /// and a new session costs a Noise handshake — the right price, with no rate limiter, no
    /// timer and no configuration.
    ///
    /// The cost, stated where it will be read: an object shared *during* a session is not
    /// visible to that session.
    shared_index: Option<Vec<SharedIndexEntry>>,
    /// What this node is carrying and may pass on, taken once per session (ADR-0028 §9).
    carried_index: Option<Vec<otwono_net::content::CarriedEntry>>,
    /// What this node is carrying *for this peer*, taken once per session.
    ///
    /// A separate snapshot from `carried_index` and not a filter over it, because the two
    /// answer different questions and sharing one cache would mean a bug in the scoping
    /// could serve the wrong one. The cost is the same as every other index here: an
    /// envelope taken into custody *during* a session is not visible to that session.
    addressed_index: Option<Vec<otwono_net::content::CarriedEntry>>,
    /// Envelope ids this node holds custody of, for the *serving* decision (ADR-0028 §11).
    ///
    /// Separate from the two indexes above because it answers a different question — not
    /// "what may I offer you" but "may I hand you this object at all" — and because it is
    /// consulted on the manifest path, where the other two never are.
    carried_custody: Option<std::collections::HashSet<String>>,
}

impl Session {
    fn remember(&mut self, content_id: &str) {
        self.shared_released.insert(content_id.to_string());
    }

    fn was_released(&self, content_id: &str) -> bool {
        self.shared_released.contains(content_id)
    }
}

impl ContentResponder {
    pub fn new(store_socket: impl AsRef<Path>, perm_socket: impl AsRef<Path>) -> Self {
        ContentResponder {
            store_socket: store_socket.as_ref().to_path_buf(),
            perm_socket: perm_socket.as_ref().to_path_buf(),
            token: Mutex::new(None),
        }
    }

    /// Answer one request. Never fails: every failure is the same refusal.
    ///
    /// That includes the store being down, the capability being denied, and the request
    /// being malformed. A peer that could tell those apart would learn about this node's
    /// configuration, and a peer that could tell "refused" from "absent" would learn what
    /// this node holds.
    pub fn answer(&self, peer: &NodeId, request: &Request, session: &mut Session) -> Response {
        // Two shapes of refusal, because there are two shapes of question. One about a named
        // object is refused with `not_available` naming it. One asking *which* objects is
        // refused with an empty page — the same answer a peer with nothing sealed to it
        // gets, so asking cannot tell "I will not tell you" from "there is nothing"
        // (ADR-0020).
        let refuse = || match request {
            Request::Manifest { content_id, .. } | Request::Chunk { content_id, .. } => {
                Response::not_available(content_id)
            }
            Request::SharedWithMe { .. } => Response::SharedWithYou(SharedIndexPage { entries: Vec::new() }),
            // An empty page of the *matching* kind. Refusing a replication question with a
            // sharing answer would be a protocol error dressed as a refusal.
            Request::Replicable { .. } => {
                Response::Replicable(otwono_net::content::ReplicablePage { entries: Vec::new() })
            }
            // `not_available` naming the pointer, because unlike the index questions this one
            // is about a *named* thing. An empty page would be the wrong shape, and inventing
            // a "no such pointer" reply distinct from a refusal would tell a stranger which
            // names this node has -- the same leak `not_available` exists to close.
            Request::Pointer { .. } => Response::not_available(""),
            // Both carriage questions refuse with an empty page of the matching kind. A
            // carrier that refuses and one that holds nothing must be indistinguishable, or
            // asking becomes a way to find out whether a node carries at all (ADR-0028 §9).
            Request::Relayable { .. } | Request::AddressedToMe { .. } => {
                Response::Carried(otwono_net::content::CarriedPage { entries: Vec::new() })
            }
        };
        if request.validate().is_err() {
            return refuse();
        }
        if let Request::SharedWithMe { after, max_entries } = request {
            return self.shared_with(peer, after.as_deref(), *max_entries, session);
        }
        if let Request::Replicable { after, max_entries } = request {
            return self.replicable(peer, after.as_deref(), *max_entries, session);
        }
        if let Request::Pointer { service, name } = request {
            return self.pointer(service, name);
        }
        if let Request::Relayable { after, max_entries } = request {
            return self.carried(after.as_deref(), *max_entries, session, None);
        }
        if let Request::AddressedToMe { after, max_entries } = request {
            // The recipient is the NodeID the handshake authenticated, never a field the
            // asker supplied. There is nothing to spoof because there is nothing to send.
            return self.carried(after.as_deref(), *max_entries, session, Some(peer));
        }
        match self.ask_store(peer, request) {
            Some(reply) => self
                .translate(peer, request, &reply, session)
                .unwrap_or_else(refuse),
            None => refuse(),
        }
    }

    /// Answer "what have you sealed to me?" out of this session's snapshot (ADR-0020).
    fn shared_with(
        &self,
        peer: &NodeId,
        after: Option<&str>,
        max_entries: u32,
        session: &mut Session,
    ) -> Response {
        let empty = || Response::SharedWithYou(SharedIndexPage { entries: Vec::new() });
        if session.shared_index.is_none() {
            // Taken for the peer this daemon authenticated, never for one a request named:
            // a peer that could ask on somebody else's behalf would be enumerating the
            // recipient graph.
            let Some(entries) = self.ask_shared_index(peer) else {
                // The store is unreachable or refused. Reported as "nothing", like every
                // other refusal on this path -- a peer that could tell this apart from an
                // empty answer would learn about this node's configuration.
                return empty();
            };
            session.shared_index = Some(entries);
        }
        let all = session.shared_index.as_ref().expect("set just above");
        let start = match after {
            // Strictly after, so a caller paging with the last id it saw does not see it
            // twice. An unknown cursor yields nothing rather than starting over, which is
            // the honest answer to "continue after something that is not in this list".
            Some(after) => match all.iter().position(|e| e.content_id == after) {
                Some(i) => i + 1,
                None => return empty(),
            },
            None => 0,
        };
        let take = (max_entries as usize).min(content::MAX_SHARED_ENTRIES_PER_REQUEST as usize);
        Response::SharedWithYou(SharedIndexPage {
            entries: all.iter().skip(start).take(take).cloned().collect(),
        })
    }

    /// Ask the store what it has sealed to this peer. `None` if it could not be asked.
    /// Serve what this node is willing to have copied (ADR-0026 §7).
    ///
    /// Deliberately not scoped to the asking peer, unlike `shared_with`: `REPLICATED` means
    /// copying is permitted, so every peer gets the same answer and there is no filter to
    /// get wrong.
    /// One of this node's own pointers, if it publishes that name (ADR-0027).
    ///
    /// Answers only for **this** node. The request carries no owner because there is nothing
    /// to carry: a peer serving somebody else's record would be handing over something the
    /// asker cannot verify without a third party's key, and would make this node a cache for
    /// pointers, which ADR-0027 left open precisely because a cached pointer is a rollback
    /// risk wearing a friendly face.
    ///
    /// A name this node does not publish is refused with the same `not_available` an
    /// unpublishable one gets. Distinguishing them would let a stranger enumerate which
    /// names exist by the shape of the refusal.
    fn pointer(&self, service: &str, name: &str) -> Response {
        let refused = || Response::not_available("");
        // `call_store`, not a hand-rolled call. store.serve is BlastRadius::Egress and its
        // tokens are therefore one-shot by default, so a cached one is usually already
        // spent — which is exactly what `call_store`'s retry exists for, and exactly what
        // the first version of this function got wrong. It asked once with the cached token
        // and refused on failure, so every pointer request over a link was answered "I do
        // not publish that name" by a node that did.
        let Some(reply) = self.call_store("pointer.mine", json!({ "service": service, "name": name })) else {
            return refused();
        };
        match reply.get("record") {
            Some(record) if !record.is_null() => Response::Pointer(otwono_net::content::PointerReply {
                record: record.clone(),
            }),
            _ => refused(),
        }
    }

    fn replicable(
        &self,
        _peer: &NodeId,
        after: Option<&str>,
        max_entries: u32,
        session: &mut Session,
    ) -> Response {
        let empty = || Response::Replicable(otwono_net::content::ReplicablePage { entries: Vec::new() });
        if session.replicable_index.is_none() {
            let Some(entries) = self.ask_replicable_index() else {
                return empty();
            };
            session.replicable_index = Some(entries);
        }
        let all = session.replicable_index.as_ref().expect("set just above");
        let start = match after {
            Some(after) => match all.iter().position(|e| e.content_id == after) {
                Some(i) => i + 1,
                None => return empty(),
            },
            None => 0,
        };
        let entries: Vec<_> = all
            .iter()
            .skip(start)
            .take(max_entries as usize)
            .cloned()
            .collect();
        Response::Replicable(otwono_net::content::ReplicablePage { entries })
    }

    /// Answer a carriage question, scoped or not (ADR-0028 §9).
    ///
    /// One function for both because the paging, the session discipline and the refusal are
    /// identical; `scope` is the only difference and it is the whole difference. Passing
    /// `Some(peer)` asks the store for that peer's mail only — the scoping happens in
    /// `otwono-stored`, not here, so this daemon never holds the full bag while answering a
    /// scoped question and cannot leak it by mistake.
    fn carried(
        &self,
        after: Option<&str>,
        max_entries: u32,
        session: &mut Session,
        scope: Option<&NodeId>,
    ) -> Response {
        let empty = || Response::Carried(otwono_net::content::CarriedPage { entries: Vec::new() });

        let cached = if scope.is_some() {
            &mut session.addressed_index
        } else {
            &mut session.carried_index
        };
        if cached.is_none() {
            let Some(entries) = self.ask_carried_index(scope) else {
                return empty();
            };
            *cached = Some(entries);
        }
        let all = cached.as_ref().expect("set just above");
        let start = match after {
            Some(after) => match all.iter().position(|e| e.envelope_id == after) {
                Some(i) => i + 1,
                None => return empty(),
            },
            None => 0,
        };
        let entries: Vec<_> = all
            .iter()
            .skip(start)
            .take(max_entries as usize)
            .cloned()
            .collect();
        Response::Carried(otwono_net::content::CarriedPage { entries })
    }

    /// Whether this node holds custody of an object, for the serving decision.
    ///
    /// Cached for the session like every other index here, and for the same reason: producing
    /// it is a call into another daemon, and a peer that could force one per chunk request
    /// would have a cheap way to make a small node miserable. The same cost follows — an
    /// envelope taken into custody *during* a session is not servable within that session —
    /// and it is the right trade, because the alternative is a store round trip on the
    /// hottest path in the protocol.
    fn carries(&self, content_id: &str, session: &mut Session) -> bool {
        if session.carried_custody.is_none() {
            let held = self
                .ask_carried_index(None)
                .map(|entries| entries.into_iter().map(|e| e.envelope_id).collect())
                .unwrap_or_default();
            session.carried_custody = Some(held);
        }
        session
            .carried_custody
            .as_ref()
            .is_some_and(|ids| ids.contains(content_id))
    }

    fn ask_carried_index(&self, scope: Option<&NodeId>) -> Option<Vec<otwono_net::content::CarriedEntry>> {
        let params = match scope {
            Some(peer) => json!({ "recipient": peer.to_text() }),
            None => json!({}),
        };
        let reply = self.call_store("envelope.held", params)?;
        let entries = reply.get("entries")?.as_array()?;
        entries
            .iter()
            .map(|e| {
                let envelope_id = e.get("envelope_id")?.as_str()?.to_string();
                // Checked here as well as at the store, for the reason the other indexes
                // check it: this daemon is what puts it on a wire.
                if !content::is_hex_digest(&envelope_id) {
                    return None;
                }
                let recipient = e.get("recipient")?.as_str()?.to_string();
                otwono_identity::NodeId::parse(&recipient).ok()?;
                Some(otwono_net::content::CarriedEntry {
                    envelope_id,
                    recipient,
                    size_bytes: e.get("size_bytes")?.as_u64()?,
                    expires_at_ms: e.get("expires_at_ms")?.as_u64()?,
                })
            })
            .collect()
    }

    fn ask_replicable_index(&self) -> Option<Vec<otwono_net::content::ReplicableEntry>> {
        let reply = self.call_store(
            "store.replicable",
            json!({ "max_entries": content::MAX_SHARED_ENTRIES_PER_REQUEST }),
        )?;
        let entries = reply.get("entries")?.as_array()?;
        entries
            .iter()
            .map(|e| {
                let content_id = e.get("content_id")?.as_str()?.to_string();
                // Checked here as well as at the store, for the same reason the sharing
                // index checks it: this daemon puts it on a wire.
                if !content::is_hex_digest(&content_id) {
                    return None;
                }
                Some(otwono_net::content::ReplicableEntry {
                    content_id,
                    size_bytes: e.get("size_bytes")?.as_u64()?,
                    ttl_days: u32::try_from(e.get("ttl_days")?.as_u64()?).ok()?,
                    max_size_bytes: e.get("max_size_bytes")?.as_u64()?,
                    allow_rereplication: e.get("allow_rereplication")?.as_bool()?,
                })
            })
            .collect()
    }

    fn ask_shared_index(&self, peer: &NodeId) -> Option<Vec<SharedIndexEntry>> {
        let reply = self.call_store(
            "store.shared_with",
            json!({
                "peer": peer.to_text(),
                "max_entries": content::MAX_SHARED_ENTRIES_PER_REQUEST,
            }),
        )?;
        let entries = reply.get("entries")?.as_array()?;
        entries
            .iter()
            .map(|e| {
                let content_id = e.get("content_id")?.as_str()?.to_string();
                // Checked here as well as at the store: this daemon puts it on a wire, and a
                // malformed id would be a request a peer cannot make sense of.
                if !content::is_hex_digest(&content_id) {
                    return None;
                }
                Some(SharedIndexEntry {
                    content_id,
                    plaintext_size_bytes: e.get("plaintext_size_bytes")?.as_u64()?,
                })
            })
            .collect()
    }

    /// Turn the store's reply into a wire reply, refusing on anything unexpected.
    ///
    /// This is where the second label check happens, on the way out.
    fn translate(
        &self,
        peer: &NodeId,
        request: &Request,
        reply: &serde_json::Value,
        session: &mut Session,
    ) -> Option<Response> {
        let label = reply.get("visibility").and_then(|v| v.as_str());
        // A malformed envelope is not an envelope. Parsed strictly here so a reply that
        // cannot be understood is refused rather than serialised half-understood.
        let sharing: Option<SharedEnvelope> = reply
            .get("sharing")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        // Manifests and chunks are judged differently, because only the manifest carries
        // the envelope. A manifest must name this peer in a sealed key; a chunk of a shared
        // object goes out only if this daemon already gave this peer that manifest in this
        // session. The store made its own decision from the recipient list it holds; this is
        // the decision this daemon can make without that list, and it catches a store that
        // answers a chunk request for an object it never advertised to this peer.
        let allowed = match request {
            // A shared object goes to the peer it was sealed to — *or* to anyone at all, if
            // this node is carrying it for somebody (ADR-0028 §11). Carrying is exactly the
            // act of holding another party's sealed bytes in order to pass them on, and the
            // ciphertext is opaque to every hop: a carrier that receives it, and the sealed
            // key it cannot use, learns nothing it did not already know from the descriptor.
            //
            // Without this an envelope can never reach a carrier, because ADR-0019's serving
            // rule admits only the recipient — which makes store-and-forward impossible by
            // construction. That was found by running it.
            Request::Manifest { .. } => {
                may_go_to_peer(label, sharing.as_ref(), peer)
                    || (label == Some("shared")
                        && request.content_id().is_some_and(|id| self.carries(id, session)))
            }
            // A chunk reply carrying an envelope is a reply nobody should be sending.
            Request::Chunk { .. } if sharing.is_some() => false,
            Request::Chunk { .. } if label == Some("shared") => {
                request.content_id().is_some_and(|id| session.was_released(id))
            }
            Request::Chunk { .. } => may_leave_a_node(label),
            // Never reaches translate: the index questions are answered from the session
            // snapshot in `answer`, and a pointer is answered before the store is asked for
            // an object at all.
            Request::SharedWithMe { .. }
            | Request::Replicable { .. }
            | Request::Relayable { .. }
            | Request::AddressedToMe { .. }
            | Request::Pointer { .. } => false,
        };
        if !allowed {
            // Reachable only if otwono-stored regressed: it has already applied its own
            // version of this rule. Loud, because it means the two independent checks
            // disagree and one of them is broken.
            eprintln!(
                "otwono-netd: refusing to serve {}: the store offered it labelled {:?} to {}",
                request.content_id().unwrap_or("<no object>"),
                label.unwrap_or("<absent>"),
                peer.fingerprint()
            );
            return None;
        }
        let content_id = reply.get("content_id")?.as_str()?.to_string();
        if Some(content_id.as_str()) != request.content_id() {
            return None;
        }
        if label == Some("shared") && matches!(request, Request::Manifest { .. }) {
            session.remember(&content_id);
        }
        match request {
            Request::Manifest { .. } => Some(Response::Manifest(ManifestPage {
                content_id,
                size_bytes: reply.get("size_bytes")?.as_u64()?,
                chunking: reply.get("chunking")?.as_str()?.to_string(),
                visibility: label?.to_string(),
                sharing,
                total_chunks: u32::try_from(reply.get("total_chunks")?.as_u64()?).ok()?,
                from_chunk: u32::try_from(reply.get("from_chunk")?.as_u64()?).ok()?,
                chunks: reply
                    .get("chunks")?
                    .as_array()?
                    .iter()
                    .map(|c| {
                        Some(ChunkEntry {
                            blake3: c.get("blake3")?.as_str()?.to_string(),
                            length: u32::try_from(c.get("length")?.as_u64()?).ok()?,
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
            })),
            // Unreachable: `allowed` above refuses all three, and `answer` routes a pointer
            // before the store is asked for an object. Written out rather than caught by a
            // wildcard so a future request kind has to be considered here instead of
            // silently becoming a manifest.
            Request::SharedWithMe { .. }
            | Request::Replicable { .. }
            | Request::Relayable { .. }
            | Request::AddressedToMe { .. }
            | Request::Pointer { .. } => None,
            Request::Chunk { digest, .. } => {
                let served = reply.get("digest")?.as_str()?;
                if served != digest {
                    return None;
                }
                Some(Response::Chunk(ChunkPart {
                    content_id,
                    digest: served.to_string(),
                    offset: u32::try_from(reply.get("offset")?.as_u64()?).ok()?,
                    total_length: u32::try_from(reply.get("total_length")?.as_u64()?).ok()?,
                    data: reply.get("data")?.as_str()?.to_string(),
                }))
            }
        }
    }

    fn ask_store(&self, peer: &NodeId, request: &Request) -> Option<serde_json::Value> {
        let (method, params) = match request {
            // A pointer is fetched by `pointer` above, not through the object path: it names
            // no content id, so there is no object for the store to authorize.
            Request::Pointer { .. } => return None,
            Request::Manifest {
                content_id,
                from_chunk,
                max_chunks,
            } => (
                "store.serve_manifest",
                json!({
                    "content_id": content_id,
                    "from_chunk": from_chunk,
                    "max_chunks": max_chunks,
                    "peer": peer.to_text(),
                }),
            ),
            Request::Chunk {
                content_id,
                digest,
                offset,
                max_bytes,
            } => (
                "store.serve_chunk",
                json!({
                    "content_id": content_id,
                    "digest": digest,
                    "offset": offset,
                    "max_bytes": max_bytes,
                    "peer": peer.to_text(),
                }),
            ),
            // Answered from a session snapshot, not per request (ADR-0020), so it never
            // reaches here.
            Request::SharedWithMe { .. }
            | Request::Replicable { .. }
            | Request::Relayable { .. }
            | Request::AddressedToMe { .. } => return None,
        };
        self.call_store(method, params)
    }

    /// One guarded call to `otwono-stored`, with the token retry every path here needs.
    fn call_store(&self, method: &str, params: serde_json::Value) -> Option<serde_json::Value> {
        let mut token = self.token()?;
        for attempt in 0..2 {
            let mut client = Client::connect(&self.store_socket).ok()?;
            match client.call_with_capability(method, params.clone(), &token).ok()? {
                Ok(value) => {
                    *self.token.lock().expect("token lock poisoned") = Some(token);
                    return Some(value);
                }
                // A token expiring mid-session is normal, not an error to surface. Exactly
                // one retry, so a genuine denial still fails fast.
                Err(e) if e.code == code::UNAUTHORIZED && attempt == 0 => {
                    *self.token.lock().expect("token lock poisoned") = None;
                    token = self.token()?;
                }
                Err(_) => return None,
            }
        }
        None
    }

    fn token(&self) -> Option<String> {
        if let Some(t) = self.token.lock().expect("token lock poisoned").clone() {
            return Some(t);
        }
        let token = request_token(
            &self.perm_socket,
            otwono_stored::CAPABILITY_SERVE,
            "otwono-netd answers peers' content requests from the local store",
        )
        .ok()?;
        Some(token)
    }

    /// Put content just fetched from a peer into the cluster cache.
    ///
    /// Never automatic. "Serving is carrying" — caching a peer's content means storing bytes
    /// the operator did not choose one at a time — so `net.fetch` only does this when its
    /// caller asked, and the caller had to hold `net.content` to ask at all.
    ///
    /// The label check is `otwono-stored`'s: `cache.put` refuses anything but `Public` and
    /// `Replicated`, and an unrecognised label parses as `Private` and is refused with them.
    /// This daemon holds `cache.write` and not `store.write`, so there is no call it can make
    /// that reaches the user's own store.
    pub fn cache(&self, fetched: &FetchedObject) -> Result<Value, String> {
        let token = request_token(
            &self.perm_socket,
            otwono_stored::CAPABILITY_CACHE_WRITE,
            "otwono-netd keeps content fetched from a peer for the cluster",
        )?;
        let mut client = Client::connect(&self.store_socket)
            .map_err(|e| format!("{}: {e}", self.store_socket.display()))?;
        client
            .call_with_capability(
                "cache.put",
                json!({
                    "data": data_encoding::BASE64.encode(&fetched.bytes),
                    "visibility": fetched.visibility,
                }),
                &token,
            )
            .map_err(|e| format!("cache.put: {e}"))?
            .map_err(|e| e.message)
    }
}

/// This daemon's view of the cluster cache, over the control plane (ADR-0026 §10).
///
/// The cache belongs to `otwono-stored` — that daemon opens it, holds its index in memory,
/// and persists on every change. This daemon must not open the same directory: two
/// processes each with their own copy of one index lose each other's updates and
/// double-count the same budget. So every question goes over the socket, to the one process
/// that can answer it correctly.
///
/// A token per call rather than a cached one, unlike [`ContentResponder`]'s `store.serve`:
/// a pass happens when this node meets a peer, which is rare enough that the extra round
/// trip costs nothing, and it means an operator who revokes `cache.replicate` stops
/// replication at the next peer rather than whenever a cached token happens to expire.
pub struct BrokeredCache {
    store_socket: PathBuf,
    perm_socket: PathBuf,
    /// Whether the "not replicating" line has already been printed.
    ///
    /// A stock node grants no `cache.replicate`, so without this the daemon would say the
    /// same thing on every connection for the life of the machine. A daemon that repeats an
    /// unchanging fact into the journal is teaching its operator to stop reading the
    /// journal, and the one line that matters gets lost with it. Said once, then dropped.
    quiet: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for BrokeredCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokeredCache")
            .field("store", &self.store_socket)
            .finish_non_exhaustive()
    }
}

impl BrokeredCache {
    pub fn new(store_socket: impl AsRef<Path>, perm_socket: impl AsRef<Path>) -> Self {
        BrokeredCache {
            store_socket: store_socket.as_ref().to_path_buf(),
            perm_socket: perm_socket.as_ref().to_path_buf(),
            quiet: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn call(&self, method: &str, params: Value, reason: &str) -> Result<Value, String> {
        let token = request_token(&self.perm_socket, otwono_stored::CAPABILITY_REPLICATE, reason)?;
        let mut client = Client::connect(&self.store_socket)
            .map_err(|e| format!("{}: {e}", self.store_socket.display()))?;
        client
            .call_with_capability(method, params, &token)
            .map_err(|e| format!("{method}: {e}"))?
            .map_err(|e| e.message)
    }
}

impl otwono_store::ReplicaHolder for BrokeredCache {
    /// A refusal reads as "this node does not replicate", and that is deliberate.
    ///
    /// The broker denying `cache.replicate`, the store being down, and a node configured
    /// with no cache all mean the same thing to a pass: nothing will be held, so ask nobody.
    /// Distinguishing them here would only give the pass more ways to be wrong; the operator
    /// learns which it was from the log line, not from the network.
    fn replica_room(&self, candidates: &[String], _now_ms: u64) -> Option<otwono_store::ReplicaRoom> {
        let reply = match self.call(
            "cache.replica_room",
            json!({ "candidates": candidates }),
            "otwono-netd is deciding whether to hold a replica for the cluster",
        ) {
            Ok(v) => v,
            Err(e) => {
                if !self.quiet.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!("otwono-netd: not replicating: {e}");
                }
                return None;
            }
        };
        if !reply.get("replicating").and_then(Value::as_bool).unwrap_or(false) {
            return None;
        }
        Some(otwono_store::ReplicaRoom {
            room_bytes: reply.get("room_bytes").and_then(Value::as_u64).unwrap_or(0),
            already_held: reply
                .get("already_held")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
        })
    }

    fn take_replica(
        &self,
        bytes: &[u8],
        policy: &otwono_store::object::Replication,
        _now_ms: u64,
    ) -> Result<Option<otwono_store::TakenReplica>, String> {
        let reply = self.call(
            "cache.take_replica",
            json!({
                "data": data_encoding::BASE64.encode(bytes),
                "ttl_days": policy.ttl_days,
                "max_size_bytes": policy.max_size_bytes,
                "allow_rereplication": policy.allow_rereplication,
            }),
            "otwono-netd is holding a replica offered by a peer",
        )?;
        if !reply.get("taken").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(None);
        }
        // `taken: true` with no id would mean the store and this daemon disagree about what
        // just happened, which is worth an error rather than a silent `None`.
        let content_id = reply
            .get("content_id")
            .and_then(Value::as_str)
            .ok_or("cache.take_replica said taken but named no object")?
            .to_string();
        let size_bytes = reply
            .get("size_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(bytes.len() as u64);
        Ok(Some(otwono_store::TakenReplica {
            content_id,
            size_bytes,
        }))
    }
}

/// This daemon's memory of the pointer sequences it has seen, over the control plane.
///
/// Same shape and same reason as [`BrokeredCache`] (ADR-0026 §10): the sequence log is
/// durable state owned by `otwono-stored`, and two processes writing one log would each lose
/// the other's updates — which for this log means silently losing rollback protection rather
/// than merely miscounting bytes.
///
/// # A failure here refuses the record
///
/// Every other brokered call in this file treats a refusal as "then do less": no cache means
/// no replication, and that is safe. This one is the opposite. If the store cannot say what
/// sequence it last saw, the choice is between refusing the record and accepting it with no
/// rollback protection — and accepting it would hand the rollback to anyone who can stop the
/// local store, which is a far easier attack than forging a signature. So it refuses, loudly,
/// as [`otwono_pointer::PointerError::MemoryUnavailable`].
pub struct BrokeredPointers {
    store_socket: PathBuf,
    perm_socket: PathBuf,
}

impl std::fmt::Debug for BrokeredPointers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokeredPointers")
            .field("store", &self.store_socket)
            .finish_non_exhaustive()
    }
}

impl BrokeredPointers {
    pub fn new(store_socket: impl AsRef<Path>, perm_socket: impl AsRef<Path>) -> Self {
        BrokeredPointers {
            store_socket: store_socket.as_ref().to_path_buf(),
            perm_socket: perm_socket.as_ref().to_path_buf(),
        }
    }
}

impl otwono_pointer::SequenceMemory for BrokeredPointers {
    fn accept(
        &self,
        pointer: &otwono_pointer::Pointer,
        public_key: &[u8; 32],
        expected: &otwono_pointer::PointerKey,
    ) -> Result<otwono_pointer::Accepted, otwono_pointer::PointerError> {
        use otwono_pointer::{Accepted, PointerError};

        // A token per call, not a cached one. `pointer.write` is reversible and cheap to
        // ask for, and a pointer is read when a person asks for one -- so the extra local
        // round trip is invisible, and an operator who revokes it stops pointer reads at the
        // next read rather than whenever a cached token happened to expire.
        let unavailable = |e: String| PointerError::MemoryUnavailable(e);
        let token = request_token(
            &self.perm_socket,
            otwono_stored::CAPABILITY_POINTER_WRITE,
            "otwono-netd is checking a peer's pointer against the sequences this node has seen",
        )
        .map_err(unavailable)?;
        let mut client = Client::connect(&self.store_socket)
            .map_err(|e| unavailable(format!("{}: {e}", self.store_socket.display())))?;
        let reply = client
            .call_with_capability(
                "pointer.accept",
                json!({
                    "record": pointer,
                    "public_key": data_encoding::BASE64.encode(public_key),
                    "node_id": expected.node_id,
                    "service": expected.service,
                    "name": expected.name,
                }),
                &token,
            )
            .map_err(|e| unavailable(format!("pointer.accept: {e}")))?
            // A store that answers "no" to the call itself -- an unparseable record, a bad
            // signature -- is answering about the record, not failing to remember. That is a
            // refusal of this pointer, not an unavailable memory.
            .map_err(|e| PointerError::Malformed(e.message))?;

        if reply.get("accepted").and_then(Value::as_bool).unwrap_or(false) {
            let to = reply
                .get("sequence")
                .and_then(Value::as_u64)
                .unwrap_or(pointer.sequence);
            return match reply.get("outcome").and_then(Value::as_str) {
                Some("first_seen") => Ok(Accepted::FirstSeen),
                Some("advanced") => Ok(Accepted::Advanced {
                    from: reply.get("from").and_then(Value::as_u64).unwrap_or(0),
                    to,
                }),
                Some("unchanged") => Ok(Accepted::Unchanged { sequence: to }),
                // An outcome this daemon does not know is not a success it can report. A
                // newer store saying something older code cannot read must not be flattened
                // into whichever variant happens to be first.
                other => Err(PointerError::Malformed(format!(
                    "pointer.accept said it accepted the record but described it as {other:?}"
                ))),
            };
        }
        if reply.get("rollback").and_then(Value::as_bool).unwrap_or(false) {
            return Err(PointerError::Rollback {
                seen: reply.get("seen").and_then(Value::as_u64).unwrap_or(0),
                offered: reply
                    .get("offered")
                    .and_then(Value::as_u64)
                    .unwrap_or(pointer.sequence),
            });
        }
        if reply
            .get("equivocation")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(PointerError::Equivocation {
                sequence: reply
                    .get("sequence")
                    .and_then(Value::as_u64)
                    .unwrap_or(pointer.sequence),
            });
        }
        // `accepted: false` with no reason means this daemon and the store disagree about
        // what the answer looks like. Refusing is the only safe reading of a reply we do not
        // understand.
        Err(PointerError::Malformed(
            "pointer.accept neither accepted the record nor said why".into(),
        ))
    }
}

/// This daemon's view of the carriage store, over the control plane (ADR-0026 §10's shape).
///
/// The envelopes belong to `otwono-stored` — that daemon opens the store, knows the budget
/// from the capability profile, and sweeps lapsed custody. This daemon must not open the same
/// directory: two processes sweeping one store would each drop what the other was holding.
///
/// A token per call rather than a cached one, as [`BrokeredCache`] does and for the same
/// reason: a pass happens when this node meets a peer, so the extra local round trip costs
/// nothing, and an operator who revokes `envelope.carry` stops carriage at the next peer.
pub struct BrokeredCarrier {
    store_socket: PathBuf,
    perm_socket: PathBuf,
    /// Whether the "not carrying" line has already been printed. A stock node grants no
    /// `envelope.carry`, so without this the daemon would say the same thing on every
    /// connection for the life of the machine.
    quiet: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for BrokeredCarrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokeredCarrier")
            .field("store", &self.store_socket)
            .finish_non_exhaustive()
    }
}

impl BrokeredCarrier {
    pub fn new(store_socket: impl AsRef<Path>, perm_socket: impl AsRef<Path>) -> Self {
        BrokeredCarrier {
            store_socket: store_socket.as_ref().to_path_buf(),
            perm_socket: perm_socket.as_ref().to_path_buf(),
            quiet: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn call(&self, method: &str, params: Value, reason: &str) -> Result<Value, String> {
        let token = request_token(&self.perm_socket, otwono_stored::CAPABILITY_CARRY, reason)?;
        let mut client = Client::connect(&self.store_socket)
            .map_err(|e| format!("{}: {e}", self.store_socket.display()))?;
        client
            .call_with_capability(method, params, &token)
            .map_err(|e| format!("{method}: {e}"))?
            .map_err(|e| e.message)
    }

    /// What this node holds for one recipient, or for everyone.
    pub fn held(&self, recipient: Option<&str>) -> Result<Vec<Value>, String> {
        let params = match recipient {
            Some(r) => json!({ "recipient": r }),
            None => json!({}),
        };
        let reply = self.call(
            "envelope.held",
            params,
            "otwono-netd is answering a carriage question",
        )?;
        Ok(reply
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Give up custody of one envelope.
    pub fn release(&self, envelope_id: &str) -> Result<(), String> {
        self.call(
            "envelope.release",
            json!({ "envelope_id": envelope_id }),
            "otwono-netd is giving up custody of a delivered envelope",
        )
        .map(|_| ())
    }
}

impl otwono_store::Carrier for BrokeredCarrier {
    /// A refusal reads as "this node carries no mail", and that is deliberate.
    ///
    /// The broker denying `envelope.carry`, the store being down, and a node whose profile
    /// sets the budget to zero all mean the same thing to a pass: nothing will be held, so
    /// ask nobody. The operator learns which it was from the log line, not from the network.
    fn carriage_room(&self, candidates: &[String], _now_ms: u64) -> Option<otwono_store::CarriageRoom> {
        let entries = match self.held(None) {
            Ok(v) => v,
            Err(e) => {
                if !self.quiet.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!("otwono-netd: not carrying mail: {e}");
                }
                return None;
            }
        };
        let already_held: Vec<String> = entries
            .iter()
            .filter_map(|e| e.get("envelope_id").and_then(Value::as_str).map(str::to_string))
            .filter(|id| candidates.is_empty() || candidates.contains(id))
            .collect();
        // The store applies the budget when it takes custody, and applies it against the
        // number from the capability profile that this daemon does not have. Reporting a
        // ceiling here rather than a computed figure keeps that decision in one place
        // (CLAUDE.md §2.6); a pass that offered too much is refused at the take.
        Some(otwono_store::CarriageRoom {
            room_bytes: otwono_envelope::MAX_ENVELOPE_BYTES,
            already_held,
        })
    }

    fn take_custody(
        &self,
        envelope: &otwono_envelope::Envelope,
        _now_ms: u64,
    ) -> Result<Option<otwono_envelope::Custody>, String> {
        let reply = self.call(
            "envelope.take",
            json!({ "envelope": envelope }),
            "otwono-netd is taking custody of an envelope for a peer it may meet later",
        )?;
        if !reply.get("taken").and_then(Value::as_bool).unwrap_or(false) {
            // A named refusal is not an error: a full node saying "not this one" is the
            // normal answer on a small machine (ADR-0026 §8).
            return Ok(None);
        }
        let until_ms = reply
            .get("until_ms")
            .and_then(Value::as_u64)
            .ok_or("envelope.take said taken but named no deadline")?;
        Ok(Some(otwono_envelope::Custody::taken(
            envelope, until_ms, until_ms,
        )))
    }
}

pub(crate) fn request_token(perm_socket: &Path, action: &str, reason: &str) -> Result<String, String> {
    let mut broker = Client::connect(perm_socket).map_err(|e| format!("{}: {e}", perm_socket.display()))?;
    let value = broker
        .call("perm.request", json!({ "action": action, "reason": reason }))
        .map_err(|e| format!("perm.request: {e}"))?
        .map_err(|e| format!("{action} refused: {}", e.message))?;
    value
        .get("token")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| "perm.request returned no token".to_string())
}

/// Serve one peer until the channel closes.
///
/// Runs on the accepting side. Every frame is a request; anything else ends the session,
/// because a peer speaking out of turn is a peer this daemon has no state machine for.
pub fn serve_session<L: LinkAdapter>(
    channel: &mut SecureChannel<L>,
    responder: &ContentResponder,
) -> Result<(), String> {
    let peer = channel.peer().node_id;
    // Shared objects this daemon has already handed *this* peer a manifest for, in this
    // session. See [`Session`].
    let mut released = Session::default();
    loop {
        let frame = match channel.recv() {
            Ok(f) => f,
            // A closed link is how a session ends, not a failure to report.
            Err(_) => return Ok(()),
        };
        let request: Request = match content::decode(&frame) {
            Ok(r) => r,
            Err(e) => {
                return Err(format!(
                    "{} sent something that is not a request: {e}",
                    peer.fingerprint()
                ))
            }
        };
        let response = responder.answer(&peer, &request, &mut released);
        let encoded = content::encode(&response).map_err(|e| e.to_string())?;
        channel.send(&encoded).map_err(|e| e.to_string())?;
    }
}

/// An object fetched from a peer, verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedObject {
    pub content_id: String,
    pub visibility: String,
    pub chunking: String,
    /// Present when the object is `shared`: what this node needs to open the ciphertext it
    /// just downloaded (ADR-0019). Checked before any chunk was asked for.
    pub sharing: Option<SharedEnvelope>,
    pub bytes: Vec<u8>,
}

/// Ask one peer what it has sealed to this node (ADR-0020).
///
/// Pages until the peer stops offering, so the caller gets the whole answer or the failure
/// that stopped it. Bounded twice over: by what the link will carry in one reply, and by a
/// ceiling on the total, because a peer that answered forever would otherwise be a way to
/// make this node allocate forever.
///
/// An empty answer is not an error. It means either that nothing has been sealed to this
/// node or that the peer will not say — deliberately indistinguishable, so that asking is
/// not a way to learn whether a node shares with anyone (ADR-0020).
/// Ask a peer what it is willing to have copied (ADR-0026 §7).
///
/// The mirror of [`fetch_shared_index`], with the same paging, the same round-trip budget,
/// and the same refusal to let a peer that stops making progress page forever.
pub fn fetch_replicable_index<L: LinkAdapter>(
    channel: &mut SecureChannel<L>,
    link: &LinkProperties,
) -> Result<Vec<content::ReplicableEntry>, ProtocolError> {
    let window = content::max_shared_entries_per_page(link);
    let mut all: Vec<content::ReplicableEntry> = Vec::new();
    let mut budget = MAX_ROUND_TRIPS;

    for _ in 0..MAX_OFFER_PAGES_PER_PASS {
        let after = all.last().map(|e| e.content_id.clone());
        let page = match round_trip(
            channel,
            &Request::Replicable {
                after,
                max_entries: window,
            },
            &mut budget,
        )? {
            Response::Replicable(p) => p,
            Response::Carried(_) => {
                return Err(ProtocolError::Mismatched(
                    "a carriage page in place of a replication offer".into(),
                ))
            }
            Response::NotAvailable { .. } => return Ok(all),
            Response::Manifest(_)
            | Response::Chunk(_)
            | Response::SharedWithYou(_)
            | Response::Pointer(_) => {
                return Err(ProtocolError::Mismatched(
                    "the wrong kind of reply in place of an offer page".into(),
                ))
            }
        };
        if page.entries.is_empty() {
            return Ok(all);
        }
        if page.entries.len() > window as usize {
            return Err(ProtocolError::TooLarge {
                field: "entries",
                asked: page.entries.len() as u64,
                ceiling: window as u64,
            });
        }
        for entry in &page.entries {
            if !content::is_hex_digest(&entry.content_id) {
                return Err(ProtocolError::NotHex { field: "content_id" });
            }
        }
        if let Some(last) = all.last() {
            if page.entries[0].content_id <= last.content_id {
                return Err(ProtocolError::NoProgress);
            }
        }
        all.extend(page.entries.iter().cloned());
        if all.len() > MAX_SHARED_INDEX_ENTRIES {
            return Err(ProtocolError::TooLarge {
                field: "entries",
                asked: all.len() as u64,
                ceiling: MAX_SHARED_INDEX_ENTRIES as u64,
            });
        }
    }
    // Running out of pages is not an error. A peer with more to offer than four pages hold
    // is one this node will meet again, and stopping here is the difference between a
    // bounded pass and a peer that can hold the discovery thread open at will.
    Ok(all)
}

/// What one replication pass did, so a caller can report it without guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicationPass {
    /// This node does not replicate, or has no budget. Nothing was asked.
    NotReplicating,
    /// The peer offered nothing, or nothing this node could take.
    NothingTaken { offered: usize },
    /// One object was copied and is now held to a TTL.
    Took { content_id: String, size_bytes: u64 },
}

/// One replication pass against one peer (ADR-0026 §9).
///
/// Ask what is on offer, take **at most one** object, and hold it. One rather than all,
/// because a node that took everything could have its whole replication budget filled by
/// the first peer it meets — which is neither fair to other peers nor what "spread across
/// the cluster" means. One per connection converges over time, and §2 already said
/// `target_replicas` is a wish.
///
/// The choice among offers is deliberately **the smallest that fits**. Not random, which is
/// untestable; not largest-first, which fills a budget with one object; and explicitly not
/// by peer capability, which §6 refuses. Smallest-first also means a small node makes
/// progress rather than repeatedly failing to fit anything.
///
/// Lapsed holds are swept first, on the same trigger, so the subsystem needs no timer.
pub fn replication_pass<L: LinkAdapter, H: otwono_store::ReplicaHolder + ?Sized>(
    channel: &mut SecureChannel<L>,
    link: &LinkProperties,
    holder: &H,
    now_ms: u64,
) -> Result<ReplicationPass, ProtocolError> {
    // The operator's consent, checked before anything reaches the wire. A node that does
    // not replicate makes no replication traffic at all rather than asking and discarding.
    // Lapsed holds are swept inside this call, on the same trigger, so the subsystem needs
    // no timer (ADR-0026 §9).
    let Some(room) = holder.replica_room(&[], now_ms) else {
        return Ok(ReplicationPass::NotReplicating);
    };
    if room.room_bytes == 0 {
        return Ok(ReplicationPass::NotReplicating);
    }

    let mut offers = fetch_replicable_index(channel, link)?;
    let offered = offers.len();
    if offers.is_empty() {
        return Ok(ReplicationPass::NothingTaken { offered });
    }
    offers.retain(|o| o.size_bytes <= o.max_size_bytes && o.size_bytes <= room.room_bytes);

    // One call for the whole page rather than one per offer: the holder may be another
    // process, and a round trip per offer would make a large page cost more than the
    // transfer it is trying to avoid.
    let ids: Vec<String> = offers.iter().map(|o| o.content_id.clone()).collect();
    let held = match holder.replica_room(&ids, now_ms) {
        Some(r) => r.already_held,
        // Between the two questions this node stopped replicating. Nothing has been taken
        // and nothing needs undoing.
        None => return Ok(ReplicationPass::NotReplicating),
    };
    offers.retain(|o| !held.contains(&o.content_id));

    offers.sort_by_key(|o| (o.size_bytes, o.content_id.clone()));
    let Some(pick) = offers.first().cloned() else {
        return Ok(ReplicationPass::NothingTaken { offered });
    };

    let fetched = fetch_object(channel, &pick.content_id, link)?;
    // The label the *peer* put on it, checked before this node keeps anything. An offer
    // list saying "replicated" and a manifest saying otherwise is a peer contradicting
    // itself, and the manifest is the one that carries the object.
    if fetched.visibility != "replicated" {
        return Err(ProtocolError::Mismatched(format!(
            "offered for replication but served as {}",
            fetched.visibility
        )));
    }
    let policy = otwono_store::object::Replication {
        target_replicas: 1,
        ttl_days: pick.ttl_days,
        max_size_bytes: pick.max_size_bytes,
        allow_rereplication: pick.allow_rereplication,
    };
    match holder.take_replica(&fetched.bytes, &policy, now_ms) {
        // Refused between the check and the take -- another thread filled the budget, or
        // the object turned out larger than advertised. Not an error: "not this one".
        Ok(None) => Ok(ReplicationPass::NothingTaken { offered }),
        Ok(Some(taken)) => Ok(ReplicationPass::Took {
            content_id: taken.content_id,
            size_bytes: taken.size_bytes,
        }),
        Err(e) => Err(ProtocolError::Mismatched(format!(
            "could not hold the replica: {e}"
        ))),
    }
}

/// Ask a peer what it is carrying, in pages (ADR-0028 §9).
///
/// `scoped` picks the question: `false` asks `content.relayable` — everything the carrier
/// holds and may pass on — and `true` asks `content.addressed_to_me`, which returns only what
/// is addressed to this node. Bounded by the same page count as the replication offer, and
/// for the same reason: this runs on the discovery thread.
fn fetch_carried_index<L: LinkAdapter>(
    channel: &mut SecureChannel<L>,
    link: &LinkProperties,
    scoped: bool,
) -> Result<Vec<content::CarriedEntry>, ProtocolError> {
    let window = content::max_shared_entries_per_page(link);
    let mut all: Vec<content::CarriedEntry> = Vec::new();
    let mut after: Option<String> = None;

    for _ in 0..MAX_OFFER_PAGES_PER_PASS {
        let mut budget = MAX_ROUND_TRIPS;
        let request = if scoped {
            Request::AddressedToMe {
                after: after.clone(),
                max_entries: window,
            }
        } else {
            Request::Relayable {
                after: after.clone(),
                max_entries: window,
            }
        };
        let page = match round_trip(channel, &request, &mut budget)? {
            Response::Carried(p) => p,
            Response::NotAvailable { .. } => break,
            Response::Manifest(_)
            | Response::Chunk(_)
            | Response::SharedWithYou(_)
            | Response::Replicable(_)
            | Response::Pointer(_) => {
                return Err(ProtocolError::Mismatched(
                    "the wrong kind of reply in place of a carriage page".into(),
                ))
            }
        };
        if page.entries.is_empty() {
            break;
        }
        after = page.entries.last().map(|e| e.envelope_id.clone());
        all.extend(page.entries);
    }
    Ok(all)
}

/// What one carry pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarryPass {
    /// This node carries no mail, or has no room. Nothing was asked.
    NotCarrying,
    /// The peer offered nothing, or nothing this node would take.
    NothingTaken { offered: usize },
    /// One envelope was taken into custody until a deadline this node chose.
    Took {
        envelope_id: String,
        size_bytes: u64,
        until_ms: u64,
    },
}

/// One carry pass against one peer (ADR-0028 §2).
///
/// Deliberately the same shape as [`replication_pass`], because it is the same problem: a
/// node with room asks a peer what is on offer and takes at most one. Writing a second
/// pattern for it would mean two places to get the budget arithmetic wrong.
///
/// **Pulled, never pushed.** The consent check happens before anything reaches the wire, so a
/// node that carries no mail makes no carriage traffic at all rather than asking and
/// discarding — ADR-0028 §2's rule kept structural rather than left to a check somebody could
/// forget.
///
/// At most one envelope per pass, for [`replication_pass`]'s reason: a node that took
/// everything could have its whole carriage budget filled by the first peer it meets. The
/// choice among offers is **the one expiring soonest**, which differs from replication's
/// smallest-first on purpose — a replica is about durability and any of them will do, while
/// an envelope has a deadline and the one closest to missing it is the one worth moving.
pub fn carry_pass<L: LinkAdapter, C: otwono_store::Carrier + ?Sized>(
    channel: &mut SecureChannel<L>,
    link: &LinkProperties,
    carrier: &C,
    now_ms: u64,
) -> Result<CarryPass, ProtocolError> {
    let Some(room) = carrier.carriage_room(&[], now_ms) else {
        return Ok(CarryPass::NotCarrying);
    };
    if room.room_bytes == 0 {
        return Ok(CarryPass::NotCarrying);
    }

    let mut offers = fetch_carried_index(channel, link, false)?;
    let offered = offers.len();
    if offers.is_empty() {
        return Ok(CarryPass::NothingTaken { offered });
    }

    // Never take an envelope addressed to this node through the *carriage* path: that is
    // collection, it belongs on the scoped question, and taking custody of one's own mail
    // would leave it sitting in the carriage store instead of being opened.
    let me = channel.local();
    offers.retain(|o| o.recipient != me.to_text());
    offers.retain(|o| o.size_bytes <= room.room_bytes && !o.expires_at_ms.eq(&0));

    let ids: Vec<String> = offers.iter().map(|o| o.envelope_id.clone()).collect();
    let held = match carrier.carriage_room(&ids, now_ms) {
        Some(r) => r.already_held,
        None => return Ok(CarryPass::NotCarrying),
    };
    offers.retain(|o| !held.contains(&o.envelope_id));

    // Soonest deadline first, ties broken by id so the choice is total and deterministic.
    offers.sort_by_key(|o| (o.expires_at_ms, o.envelope_id.clone()));
    let Some(pick) = offers.first().cloned() else {
        return Ok(CarryPass::NothingTaken { offered });
    };

    let envelope = otwono_envelope::Envelope {
        schema_version: otwono_envelope::SCHEMA_VERSION.to_string(),
        envelope_id: pick.envelope_id.clone(),
        recipient: pick.recipient.clone(),
        size_bytes: pick.size_bytes,
        expires_at_ms: pick.expires_at_ms,
    };
    // Checked before the bytes are asked for: a descriptor that does not parse is a peer
    // sending nonsense, and fetching first would mean paying for it.
    envelope
        .validate()
        .map_err(|e| ProtocolError::Mismatched(format!("offered envelope: {e}")))?;

    let fetched = fetch_object(channel, &pick.envelope_id, link)?;
    // An envelope is a SHARED object: sealed to exactly one recipient (ADR-0019). A carrier
    // offering one as anything else is contradicting itself, and the manifest is the half
    // that carries the object.
    if fetched.visibility != "shared" {
        return Err(ProtocolError::Mismatched(format!(
            "offered as an envelope but served as {}",
            fetched.visibility
        )));
    }
    if fetched.bytes.len() as u64 != pick.size_bytes {
        return Err(ProtocolError::Mismatched(format!(
            "offered {} bytes and served {}",
            pick.size_bytes,
            fetched.bytes.len()
        )));
    }

    match carrier.take_custody(&envelope, now_ms) {
        Ok(None) => Ok(CarryPass::NothingTaken { offered }),
        Ok(Some(custody)) => Ok(CarryPass::Took {
            envelope_id: custody.envelope.envelope_id,
            size_bytes: custody.envelope.size_bytes,
            until_ms: custody.until_ms,
        }),
        Err(e) => Err(ProtocolError::Mismatched(format!("could not take custody: {e}"))),
    }
}

/// Ask a peer what it is holding for *this* node, and fetch it (ADR-0028 §9).
///
/// The collection half. Uses the scoped question, so this node never receives a list of who
/// else the carrier serves — which is the reason §9 has two methods at all.
///
/// Returns what was fetched, for the caller to put where a recipient can open it. Nothing is
/// stored here: this function moves bytes, and where they land is a decision with a capability
/// attached to it.
pub fn collect_addressed<L: LinkAdapter>(
    channel: &mut SecureChannel<L>,
    link: &LinkProperties,
) -> Result<Vec<FetchedObject>, ProtocolError> {
    let mut collected = Vec::new();
    for entry in fetch_carried_index(channel, link, true)? {
        // A carrier answering the scoped question with somebody else's mail is either
        // confused or probing; either way this node did not ask for it.
        if entry.recipient != channel.local().to_text() {
            return Err(ProtocolError::Mismatched(
                "a carrier offered mail addressed to somebody else".into(),
            ));
        }
        let fetched = fetch_object(channel, &entry.envelope_id, link)?;
        if fetched.visibility != "shared" {
            return Err(ProtocolError::Mismatched(format!(
                "an envelope served as {}",
                fetched.visibility
            )));
        }
        collected.push(fetched);
    }
    Ok(collected)
}

/// Ask a peer what one of its names points at, and verify the answer (ADR-0027).
///
/// # The key comes from the handshake, not from the record
///
/// This is what makes the narrow scope worth having. The Noise handshake already proved the
/// peer's public key, so a pointer fetched **from its owner** is verified against a key this
/// node established independently of anything the peer said afterwards. There is no key
/// distribution problem here at all, and no third party to trust — which is exactly the
/// property that would be lost if a peer could serve somebody else's pointer.
///
/// `Ok(None)` means the peer does not publish that name, or would not say. Those are one
/// answer on purpose: telling them apart would let a stranger enumerate a node's names.
///
/// # Why a memory is a parameter and not an option
///
/// A signature proves who wrote a record, never that it is current (ADR-0027 §1), so the
/// only thing standing between this function and a replay is what the reader remembers. That
/// memory is therefore passed in and consulted on every fetch. A caller that genuinely wants
/// verification alone passes [`otwono_pointer::NoMemory`] and has written down that every
/// read is a first read; there is no way to get here having simply forgotten.
pub fn fetch_pointer<L: LinkAdapter, M: otwono_pointer::SequenceMemory + ?Sized>(
    channel: &mut SecureChannel<L>,
    service: &str,
    name: &str,
    link: &LinkProperties,
    memory: &M,
) -> Result<Option<otwono_pointer::Pointer>, ProtocolError> {
    // Taken before the round trip so the owner cannot be whatever the reply claims.
    let owner = channel.peer().node_id;
    let public_key = channel.peer().public_key;

    let mut budget = MAX_ROUND_TRIPS;
    let reply = match round_trip(
        channel,
        &Request::Pointer {
            service: service.to_string(),
            name: name.to_string(),
        },
        &mut budget,
    )? {
        Response::Pointer(p) => p,
        Response::NotAvailable { .. } => return Ok(None),
        Response::Manifest(_)
        | Response::Chunk(_)
        | Response::SharedWithYou(_)
        | Response::Replicable(_)
        | Response::Carried(_) => {
            return Err(ProtocolError::Mismatched(
                "the wrong kind of reply in place of a pointer".into(),
            ))
        }
    };
    let _ = link;

    let pointer: otwono_pointer::Pointer = serde_json::from_value(reply.record)
        .map_err(|e| ProtocolError::Mismatched(format!("not a pointer record: {e}")))?;

    // What was asked for, not what arrived. A record valid for a different name would
    // otherwise be accepted under this one (ADR-0027 §6). The memory checks this, the
    // signature, and the sequence together, in that order, and remembers the result -- so
    // there is one place where "is this record acceptable" is decided, rather than a check
    // here and a different one wherever the record is stored.
    let expected = otwono_pointer::PointerKey {
        node_id: owner.to_text(),
        service: service.to_string(),
        name: name.to_string(),
    };
    match memory.accept(&pointer, &public_key, &expected) {
        Ok(_) => Ok(Some(pointer)),
        // Surfaced as itself, not folded into `Mismatched`: this is the attack the whole
        // design exists to stop, and an operator seeing it needs to know that what failed
        // was a replay and not a broken peer.
        Err(otwono_pointer::PointerError::Rollback { seen, offered }) => {
            Err(ProtocolError::Rollback { seen, offered })
        }
        Err(e) => Err(ProtocolError::Mismatched(format!("pointer: {e}"))),
    }
}

pub fn fetch_shared_index<L: LinkAdapter>(
    channel: &mut SecureChannel<L>,
    link: &LinkProperties,
) -> Result<Vec<SharedIndexEntry>, ProtocolError> {
    let window = content::max_shared_entries_per_page(link);
    let mut all: Vec<SharedIndexEntry> = Vec::new();
    let mut budget = MAX_ROUND_TRIPS;

    loop {
        let after = all.last().map(|e| e.content_id.clone());
        let page = match round_trip(
            channel,
            &Request::SharedWithMe {
                after,
                max_entries: window,
            },
            &mut budget,
        )? {
            Response::SharedWithYou(p) => p,
            Response::NotAvailable { .. } => return Ok(all),
            Response::Manifest(_)
            | Response::Chunk(_)
            | Response::Replicable(_)
            | Response::Carried(_)
            | Response::Pointer(_) => {
                return Err(ProtocolError::Mismatched(
                    "the wrong kind of reply in place of a sharing index page".into(),
                ))
            }
        };
        if page.entries.is_empty() {
            return Ok(all);
        }
        if page.entries.len() > window as usize {
            return Err(ProtocolError::TooLarge {
                field: "entries",
                asked: page.entries.len() as u64,
                ceiling: window as u64,
            });
        }
        for entry in &page.entries {
            if !content::is_hex_digest(&entry.content_id) {
                return Err(ProtocolError::NotHex { field: "content_id" });
            }
        }
        // A peer that keeps sending the same ids would page forever. Ids are ordered, so
        // anything not strictly after the last one this node holds is a peer that is not
        // making progress -- which is the same judgement `fetch_manifest` makes about a
        // window that adds nothing.
        if let Some(last) = all.last() {
            if page.entries[0].content_id <= last.content_id {
                return Err(ProtocolError::NoProgress);
            }
        }
        all.extend(page.entries.iter().cloned());
        if all.len() > MAX_SHARED_INDEX_ENTRIES {
            return Err(ProtocolError::TooLarge {
                field: "entries",
                asked: all.len() as u64,
                ceiling: MAX_SHARED_INDEX_ENTRIES as u64,
            });
        }
    }
}

/// Ceiling on how much of one peer's index this node will assemble.
///
/// A peer answering forever is a peer making this node allocate forever. Sixteen thousand
/// objects sealed to one node is far past any household, and a recipient that genuinely has
/// more can page again after fetching what it has.
pub const MAX_SHARED_INDEX_ENTRIES: usize = 16_384;

/// How many offer pages one replication pass will read before choosing.
///
/// The pass takes at most one object, so it does not need a peer's whole catalogue — a
/// bounded sample is enough, and a peer with more to offer will be met again.
///
/// Bounded in **pages rather than entries**, because the pass runs on the discovery thread
/// (ADR-0026 §10) and an entry ceiling does not bound time: `MAX_SHARED_INDEX_ENTRIES` with
/// a trickle link's one-entry window is sixteen thousand round trips, each of which can sit
/// on a fifteen-second socket timeout. That would let one slow or hostile peer stall this
/// node's peer discovery entirely. Four pages costs four round trips whatever the link, and
/// yields up to 1024 offers on a fast one.
pub const MAX_OFFER_PAGES_PER_PASS: usize = 4;

/// Fetch one object from a peer over an established channel.
///
/// Runs on the dialling side. Every chunk is hashed as it arrives, and the assembled chunk
/// list is checked against the id that was asked for, so a peer can neither substitute
/// content nor make this daemon buffer more than one chunk before the first check fires.
pub fn fetch_object<L: LinkAdapter>(
    channel: &mut SecureChannel<L>,
    content_id: &str,
    link: &LinkProperties,
) -> Result<FetchedObject, ProtocolError> {
    if !content::is_hex_digest(content_id) {
        return Err(ProtocolError::NotHex { field: "content_id" });
    }
    // Said here rather than discovered as a PayloadTooLarge three layers down. A Trickle
    // link can carry chunk replies but not a manifest window, so a fetch over one cannot
    // begin — see `content::carries_a_manifest` and OQ-23.
    if !content::carries_a_manifest(link) {
        return Err(ProtocolError::TooLarge {
            field: "manifest window",
            asked: (content::MANIFEST_ENVELOPE_RESERVE + content::NOISE_TAG_BYTES) as u64,
            ceiling: link.bandwidth_class.max_reasonable_payload() as u64,
        });
    }
    let mut budget = MAX_ROUND_TRIPS;
    let (header, entries) = fetch_manifest(channel, content_id, link, &mut budget)?;

    // Before any chunk is asked for: the declared chunk list must hash to the id that was
    // requested. A peer with a substituted manifest is caught here rather than after a
    // gigabyte of perfectly-verifying wrong chunks.
    verified_refs(content_id, &entries)?;

    // Checked here rather than in fetch_manifest: the answer goes back on a control-plane
    // line, which is this destination's limit and not the peer's problem.
    if header.size_bytes > MAX_FETCH_BYTES {
        return Err(ProtocolError::TooLarge {
            field: "size_bytes",
            asked: header.size_bytes,
            ceiling: MAX_FETCH_BYTES,
        });
    }

    let mut bytes = Vec::with_capacity(header.size_bytes.min(MAX_FETCH_BYTES) as usize);
    for entry in &entries {
        let chunk = fetch_chunk(channel, content_id, entry, link, &mut budget)?;
        let got = ChunkRef::of(&chunk);
        if got.hex() != entry.blake3 {
            return Err(ProtocolError::ChunkDigestMismatch {
                expected: entry.blake3.clone(),
                actual: got.hex(),
            });
        }
        bytes.extend_from_slice(&chunk);
    }

    Ok(FetchedObject {
        content_id: content_id.to_string(),
        visibility: header.visibility,
        chunking: header.chunking,
        sharing: header.sharing,
        bytes,
    })
}

/// Turn a manifest's declared chunk list into refs, and check it describes the object that
/// was asked for.
///
/// **This is checkable before a single chunk is fetched**, because a `ContentId` is the
/// BLAKE3 of the chunk list itself. The first version of this module only noticed a
/// substituted manifest after reassembling everything — every chunk verified against the
/// digest the liar had declared, so nothing failed until the final comparison. A peer could
/// therefore make this node download a gigabyte before being caught.
///
/// Checking here closes that, and it is what makes fan-out safe: once the manifest is known
/// authentic, any peer may serve any chunk and be verified against it independently.
fn verified_refs(content_id: &str, entries: &[ChunkEntry]) -> Result<Vec<ChunkRef>, ProtocolError> {
    let mut refs = Vec::with_capacity(entries.len());
    for entry in entries {
        let digest: [u8; 32] = data_encoding::HEXLOWER
            .decode(entry.blake3.as_bytes())
            .ok()
            .and_then(|v| v.try_into().ok())
            .ok_or(ProtocolError::NotHex { field: "blake3" })?;
        refs.push(ChunkRef {
            digest,
            length: entry.length,
        });
    }
    let derived = ContentId::of(&refs).to_hex();
    if derived != content_id {
        return Err(ProtocolError::ObjectIdMismatch {
            expected: content_id.to_string(),
            actual: derived,
        });
    }
    Ok(refs)
}

/// Every window of the chunk list, checked as it arrives.
fn fetch_manifest<L: LinkAdapter>(
    channel: &mut SecureChannel<L>,
    content_id: &str,
    link: &LinkProperties,
    budget: &mut usize,
) -> Result<(ManifestPage, Vec<ChunkEntry>), ProtocolError> {
    let me = channel.local().to_text();
    let window = content::max_chunks_per_page(link);
    let mut entries: Vec<ChunkEntry> = Vec::new();
    let mut header: Option<ManifestPage> = None;
    let mut stalled = 0;

    loop {
        let from = u32::try_from(entries.len()).map_err(|_| ProtocolError::NoProgress)?;
        let page = match round_trip(
            channel,
            &Request::Manifest {
                content_id: content_id.to_string(),
                from_chunk: from,
                max_chunks: window,
            },
            budget,
        )? {
            Response::Manifest(p) => p,
            Response::NotAvailable { .. } => return Err(ProtocolError::NotAvailable(content_id.to_string())),
            Response::Chunk(_) => {
                return Err(ProtocolError::Mismatched("a chunk in place of a manifest".into()))
            }
            Response::SharedWithYou(_)
            | Response::Replicable(_)
            | Response::Carried(_)
            | Response::Pointer(_) => {
                return Err(ProtocolError::Mismatched(
                    "an index page in place of a manifest".into(),
                ))
            }
        };

        if page.content_id != content_id {
            return Err(ProtocolError::Mismatched(format!(
                "manifest for {} rather than {content_id}",
                page.content_id
            )));
        }
        if page.from_chunk != from {
            return Err(ProtocolError::Mismatched(format!(
                "manifest window at {} rather than {from}",
                page.from_chunk
            )));
        }
        // This node's own label check, on the way in. Caching content a peer has labelled
        // in a way this node does not recognise is not something to do by default.
        //
        // `shared` is accepted only when the manifest carries a key sealed to *this* node,
        // which `sharing_is_consistent` checks. Doing it here rather than after the download
        // is the same rule as defect 34's: everything that can be checked before asking for
        // a chunk is checked before asking for a chunk.
        if !may_leave_a_node(Some(&page.visibility)) && page.visibility != "shared" {
            return Err(ProtocolError::Mismatched(format!(
                "the peer offered {content_id} labelled {:?}, which is not a label content \
                 may be served under",
                page.visibility
            )));
        }
        page.sharing_is_consistent(&me)?;
        if let Some(first) = &header {
            if first.sharing != page.sharing {
                return Err(ProtocolError::Mismatched(
                    "the peer changed the sealed key between windows".into(),
                ));
            }
        }
        // Deliberately no size check here. Whether an object is too large is a property of
        // where it is going, not of the peer offering it — and a failure raised inside a
        // per-peer manifest fetch is indistinguishable, to the fan-out loop above, from
        // "this peer does not have it". That is how a 641 KiB object came back as
        // NotAvailable instead of TooLarge. The caller checks, once it knows the
        // destination.
        let total = page.total_chunks as usize;
        if total > MAX_FETCH_CHUNKS {
            return Err(ProtocolError::TooLarge {
                field: "total_chunks",
                asked: total as u64,
                ceiling: MAX_FETCH_CHUNKS as u64,
            });
        }
        if let Some(first) = &header {
            if first.total_chunks != page.total_chunks || first.size_bytes != page.size_bytes {
                return Err(ProtocolError::Mismatched(
                    "the peer changed the object's shape between windows".into(),
                ));
            }
        }
        if entries.len() + page.chunks.len() > total {
            return Err(ProtocolError::Mismatched(
                "the peer sent more chunk entries than it said the object had".into(),
            ));
        }

        // A peer that answers but never advances is a peer to give up on.
        if page.chunks.is_empty() {
            stalled += 1;
            if stalled >= 2 {
                return Err(ProtocolError::NoProgress);
            }
        } else {
            stalled = 0;
        }
        entries.extend(page.chunks.iter().cloned());
        if header.is_none() {
            header = Some(page);
        }
        if entries.len() == total {
            return Ok((header.expect("set on the first window"), entries));
        }
    }
}

/// One chunk, in as many ranges as the link needs.
fn fetch_chunk<L: LinkAdapter>(
    channel: &mut SecureChannel<L>,
    content_id: &str,
    entry: &ChunkEntry,
    link: &LinkProperties,
    budget: &mut usize,
) -> Result<Vec<u8>, ProtocolError> {
    if entry.length > content::MAX_CHUNK_BYTES {
        return Err(ProtocolError::TooLarge {
            field: "length",
            asked: entry.length as u64,
            ceiling: content::MAX_CHUNK_BYTES as u64,
        });
    }
    let span = content::max_body_bytes(link);
    let mut got: Vec<u8> = Vec::with_capacity(entry.length as usize);
    let mut stalled = 0;

    while (got.len() as u32) < entry.length {
        let offset = got.len() as u32;
        let part = match round_trip(
            channel,
            &Request::Chunk {
                content_id: content_id.to_string(),
                digest: entry.blake3.clone(),
                offset,
                max_bytes: span.min(entry.length - offset),
            },
            budget,
        )? {
            Response::Chunk(p) => p,
            Response::NotAvailable { .. } => return Err(ProtocolError::NotAvailable(content_id.to_string())),
            Response::Manifest(_) => {
                return Err(ProtocolError::Mismatched("a manifest in place of a chunk".into()))
            }
            Response::SharedWithYou(_)
            | Response::Replicable(_)
            | Response::Carried(_)
            | Response::Pointer(_) => {
                return Err(ProtocolError::Mismatched(
                    "an index page in place of a chunk".into(),
                ))
            }
        };

        if part.content_id != content_id || part.digest != entry.blake3 {
            return Err(ProtocolError::Mismatched(
                "the peer answered about different content".into(),
            ));
        }
        if part.offset != offset || part.total_length != entry.length {
            return Err(ProtocolError::Mismatched(format!(
                "asked for {} at {offset}, got {} at {}",
                entry.length, part.total_length, part.offset
            )));
        }
        let data = data_encoding::BASE64
            .decode(part.data.as_bytes())
            .map_err(|e| ProtocolError::Malformed(format!("chunk data is not base64: {e}")))?;
        if got.len() + data.len() > entry.length as usize {
            return Err(ProtocolError::Mismatched(
                "the peer sent more bytes than the chunk holds".into(),
            ));
        }
        if data.is_empty() {
            stalled += 1;
            if stalled >= 2 {
                return Err(ProtocolError::NoProgress);
            }
        } else {
            stalled = 0;
        }
        got.extend_from_slice(&data);
    }
    Ok(got)
}

/// Where a fan-out fetch puts the chunks as they arrive.
///
/// Two shapes, one worker loop. The difference matters on a small board: `Memory` holds the
/// whole object, and `File` holds one chunk per peer no matter how large the object is
/// (OQ-25). A T0 node with 512 MiB of RAM can fetch a 2 GiB object through the second and
/// cannot through the first.
enum Destination {
    /// Slot per chunk, assembled in order at the end.
    Memory(Mutex<Vec<Option<Vec<u8>>>>),
    /// Written straight to the file at each chunk's offset.
    ///
    /// Chunks arrive out of order from parallel peers, which would mean buffering — except
    /// that the manifest gives every chunk's length, so every offset is known before a
    /// single byte is asked for. `pwrite` at a computed offset needs no ordering and no
    /// shared cursor.
    File {
        file: std::fs::File,
        offsets: Vec<u64>,
        /// Which chunks have landed. The file itself cannot be asked, because a hole and a
        /// chunk of zeroes look the same.
        written: Mutex<Vec<bool>>,
    },
}

impl Destination {
    fn put(&self, index: usize, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Destination::Memory(slots) => {
                slots.lock().expect("fan-out slots poisoned")[index] = Some(bytes.to_vec());
                Ok(())
            }
            Destination::File {
                file,
                offsets,
                written,
            } => {
                use std::os::unix::fs::FileExt;
                file.write_all_at(bytes, offsets[index])?;
                written.lock().expect("fan-out slots poisoned")[index] = true;
                Ok(())
            }
        }
    }

    fn missing(&self) -> bool {
        match self {
            Destination::Memory(slots) => slots
                .lock()
                .expect("fan-out slots poisoned")
                .iter()
                .any(|s| s.is_none()),
            Destination::File { written, .. } => {
                written.lock().expect("fan-out slots poisoned").iter().any(|w| !w)
            }
        }
    }
}

/// One peer, ready to be asked for chunks.
pub struct PeerSource<L: LinkAdapter> {
    /// How this peer is named in the report. A NodeID fingerprint on a real node.
    pub name: String,
    pub channel: SecureChannel<L>,
    pub link: LinkProperties,
}

/// Why one peer's worker stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerEnd {
    /// The queue emptied, or the destination broke. Nothing to say about the peer.
    Done,
    /// It lied or broke too often.
    Faulty,
    /// It ran out of chunks it had. Not its fault and not a judgement.
    Exhausted,
}

/// Who served what, after a fan-out fetch.
///
/// Worth returning rather than logging: ADR-0015's central claim is that a dense
/// cluster transfers faster because every holder is as good as any other, and this is
/// the only thing that shows whether that actually happened on a given fetch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FanOutReport {
    /// Which peer the (verified) chunk list came from.
    pub manifest_from: String,
    /// Chunks successfully served, per peer.
    pub chunks_from: std::collections::BTreeMap<String, usize>,
    /// Failures per peer: a refusal, a bad digest, or a dead link.
    pub demerits: std::collections::BTreeMap<String, usize>,
    /// Peers dropped mid-transfer for lying or breaking. A judgement.
    pub dropped: Vec<String>,
    /// Peers that simply ran out of chunks they had. Not a judgement, and reported
    /// separately so that "this neighbour is faulty" and "this neighbour had a small share"
    /// never look the same in a log.
    pub exhausted: Vec<String>,
}

impl FanOutReport {
    pub fn peers_that_served(&self) -> usize {
        self.chunks_from.values().filter(|n| **n > 0).count()
    }
}

/// How many *faults* a peer gets before this transfer stops asking it.
///
/// A fault is a lie or a broken link: a chunk whose bytes do not match the digest the
/// manifest declared, a reply that is not the answer to the question, or a dead channel.
///
/// It is **not** a peer saying it does not have a chunk. That distinction is the whole of
/// this rule and getting it wrong broke the first version: with no want-list, a peer holding
/// a third of an object is asked for chunks it lacks about twice for every one it has, so
/// counting "not available" as a failure drops exactly the honest partial peers the fan-out
/// exists to combine.
///
/// Per transfer, not remembered. A peer that is merely slow is not an enemy, and a
/// persistent judgement about neighbours is the beginning of the reputation system ADR-0015
/// declined to build (OQ-17).
pub const MAX_PEER_FAULTS: usize = 3;

/// How long a worker waits when every outstanding chunk is either being fetched by somebody
/// else or is one this peer has already said it lacks.
///
/// Short, because the thing it is waiting for is another peer's round trip finishing, and
/// the only alternative is to exit and possibly leave work undone.
const IDLE_WAIT: Duration = Duration::from_millis(5);

/// Fetch one object from several peers at once, verifying every chunk on arrival.
///
/// The mechanism ADR-0015 is about. Once the manifest is known authentic — it hashes to the
/// id that was asked for, checked before any chunk is requested — **any** holder of a chunk
/// is as good as any other, because the digest is checked at this end. So the chunks are
/// spread across every reachable peer, and a peer that lies or refuses simply loses its
/// share of the work.
///
/// A peer that serves rubbish wastes this node's bandwidth and cannot corrupt its data.
/// That is the whole security argument, and it is one hash long.
///
/// One thread per peer, one request outstanding per peer. There is no pipelining, so the
/// speedup is in parallelism across peers rather than depth per peer — which is the axis
/// that gets better as a street gets denser, and the one this is for.
pub fn fetch_object_from_peers<L: LinkAdapter + Send + 'static>(
    peers: Vec<PeerSource<L>>,
    content_id: &str,
) -> Result<(FetchedObject, FanOutReport), ProtocolError> {
    let (header, report, destination) = fan_out(peers, content_id, None)?;
    let Destination::Memory(slots) = destination else {
        unreachable!("a fetch with no file asked for assembles in memory");
    };
    let slots = slots.into_inner().expect("fan-out slots poisoned");
    let mut bytes = Vec::with_capacity(header.size_bytes as usize);
    for chunk in slots.into_iter().flatten() {
        bytes.extend_from_slice(&chunk);
    }
    Ok((
        FetchedObject {
            content_id: content_id.to_string(),
            visibility: header.visibility,
            chunking: header.chunking,
            sharing: header.sharing,
            bytes,
        },
        report,
    ))
}

/// Fetch one object from several peers straight into a file, holding one chunk per peer.
///
/// The difference from [`fetch_object_from_peers`] is memory, and on a small board it is the
/// whole difference: this holds `peers x MAX_CHUNK` at a time — under a megabyte for three
/// peers — where the in-memory form holds the entire object. A T0 node with 512 MiB can
/// fetch a 2 GiB object through this and cannot through that (OQ-25).
///
/// The file is left at the object's exact length and every byte of it is verified. On any
/// failure it is truncated to nothing before returning, because a partially-written file
/// that looks complete is worse than no file.
pub fn fetch_object_to_file<L: LinkAdapter + Send + 'static>(
    peers: Vec<PeerSource<L>>,
    content_id: &str,
    file: std::fs::File,
) -> Result<(FetchedMeta, FanOutReport), ProtocolError> {
    let (header, report, _) = fan_out(peers, content_id, Some(file))?;
    Ok((
        FetchedMeta {
            content_id: content_id.to_string(),
            visibility: header.visibility,
            chunking: header.chunking,
            size_bytes: header.size_bytes,
            sharing: header.sharing,
        },
        report,
    ))
}

/// What is known about an object fetched to a file: everything except its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedMeta {
    pub content_id: String,
    pub visibility: String,
    pub chunking: String,
    pub size_bytes: u64,
    /// Present when the object is `shared`. The file on disk is ciphertext until this is
    /// used, which is why it travels with the metadata rather than being left behind.
    pub sharing: Option<SharedEnvelope>,
}

fn fan_out<L: LinkAdapter + Send + 'static>(
    peers: Vec<PeerSource<L>>,
    content_id: &str,
    into: Option<std::fs::File>,
) -> Result<(ManifestPage, FanOutReport, Destination), ProtocolError> {
    if !content::is_hex_digest(content_id) {
        return Err(ProtocolError::NotHex { field: "content_id" });
    }
    if peers.is_empty() {
        return Err(ProtocolError::NotAvailable(content_id.to_string()));
    }

    let mut report = FanOutReport::default();
    let mut peers = peers;

    // The manifest, from whichever peer can produce an authentic one. Sequential: it is one
    // small exchange, and asking everyone at once to save a round trip on a LAN is not worth
    // the shape it would impose on the rest of this function.
    let mut header = None;
    let mut entries = Vec::new();
    let mut refs = Vec::new();
    let mut usable = Vec::new();
    for mut peer in peers.drain(..) {
        if header.is_some() {
            usable.push(peer);
            continue;
        }
        let mut budget = MAX_ROUND_TRIPS;
        match fetch_manifest(&mut peer.channel, content_id, &peer.link, &mut budget)
            .and_then(|(h, e)| verified_refs(content_id, &e).map(|r| (h, e, r)))
        {
            Ok((h, e, r)) => {
                report.manifest_from = peer.name.clone();
                header = Some(h);
                entries = e;
                refs = r;
                usable.push(peer);
            }
            Err(_) => {
                // Not fatal and not interesting: a peer that does not have the object is the
                // ordinary case, and one that lied about it has just been caught for free.
                *report.demerits.entry(peer.name.clone()).or_insert(0) += 1;
                usable.push(peer);
            }
        }
    }
    let Some(header) = header else {
        return Err(ProtocolError::NotAvailable(content_id.to_string()));
    };

    let total: u64 = refs.iter().map(|r| r.length as u64).sum();
    // The in-memory path is capped because the reply carries the object on one
    // control-plane line (ADR-0018). The file path is not: that is what it is for.
    if into.is_none() && total > MAX_FETCH_BYTES {
        return Err(ProtocolError::TooLarge {
            field: "size_bytes",
            asked: total,
            ceiling: MAX_FETCH_BYTES,
        });
    }

    let destination = Arc::new(match into {
        Some(file) => {
            // Sized up front so every offset is inside the file before any worker writes,
            // and so a short disk is discovered now rather than at the last chunk.
            file.set_len(total)
                .map_err(|e| ProtocolError::Malformed(format!("cannot size the fetch file: {e}")))?;
            let mut offsets = Vec::with_capacity(refs.len());
            let mut at = 0u64;
            for r in &refs {
                offsets.push(at);
                at += r.length as u64;
            }
            Destination::File {
                file,
                offsets,
                written: Mutex::new(vec![false; refs.len()]),
            }
        }
        None => Destination::Memory(Mutex::new(vec![None; entries.len()])),
    });

    // Outstanding chunks, and which are being fetched right now.
    //
    // The first version was a queue that workers popped from and pushed back to on failure,
    // which made a peer ask for the same missing chunk over and over: with no want-list, a
    // peer holding a third of an object is refused twice for every hit, and a rotating queue
    // turns that into an unbounded spin. Each worker now remembers what *this* peer has said
    // it lacks and never asks twice, so a worker ends exactly when every chunk still needed
    // is one it has already been refused. No heuristic, no arbitrary miss count.
    let outstanding: Arc<Mutex<std::collections::BTreeSet<usize>>> =
        Arc::new(Mutex::new((0..entries.len()).collect()));
    let in_flight: Arc<Mutex<std::collections::BTreeSet<usize>>> =
        Arc::new(Mutex::new(std::collections::BTreeSet::new()));
    let entries = Arc::new(entries);
    let id = content_id.to_string();

    let mut workers = Vec::new();
    for mut peer in usable {
        let outstanding = Arc::clone(&outstanding);
        let in_flight = Arc::clone(&in_flight);
        let destination = Arc::clone(&destination);
        let entries = Arc::clone(&entries);
        let id = id.clone();
        workers.push(std::thread::spawn(move || {
            let mut served = 0usize;
            let mut faults = 0usize;
            let mut budget = MAX_ROUND_TRIPS;
            // What *this* peer has said it does not have. Never asked for twice.
            let mut lacks = std::collections::BTreeSet::new();

            let outcome = loop {
                if faults >= MAX_PEER_FAULTS {
                    break WorkerEnd::Faulty;
                }
                // Take the first chunk still needed that this peer has not already refused
                // and nobody else is currently fetching.
                let claimed = {
                    let outstanding = outstanding.lock().expect("fan-out set poisoned");
                    let mut busy = in_flight.lock().expect("fan-out set poisoned");
                    match outstanding
                        .iter()
                        .find(|i| !lacks.contains(*i) && !busy.contains(*i))
                    {
                        Some(&i) => {
                            busy.insert(i);
                            Some(i)
                        }
                        None => {
                            // Nothing for this peer right now. If somebody else is still
                            // working, wait for them: their attempt may fail and leave a
                            // chunk this peer does have.
                            if busy.is_empty() {
                                None
                            } else {
                                Some(usize::MAX)
                            }
                        }
                    }
                };
                let index = match claimed {
                    Some(usize::MAX) => {
                        std::thread::sleep(IDLE_WAIT);
                        continue;
                    }
                    Some(i) => i,
                    None => break WorkerEnd::Exhausted,
                };

                let entry = &entries[index];
                let result = fetch_chunk(&mut peer.channel, &id, entry, &peer.link, &mut budget);
                let mut done = false;
                match result {
                    Ok(bytes) if ChunkRef::of(&bytes).hex() == entry.blake3 => {
                        // Verified before it is written, always. A chunk that fails here
                        // never reaches the destination, in memory or on disk.
                        match destination.put(index, &bytes) {
                            Ok(()) => {
                                served += 1;
                                done = true;
                            }
                            Err(_) => {
                                // The destination is broken, not the peer. Stop this worker
                                // rather than blaming a neighbour for a full disk.
                                in_flight.lock().expect("fan-out set poisoned").remove(&index);
                                break WorkerEnd::Done;
                            }
                        }
                    }
                    // Not having a chunk is an ordinary answer, not a fault -- and with no
                    // want-list it is also the common answer, which is exactly why it must
                    // not count against a peer. Remembered, so it is never asked again.
                    Err(ProtocolError::NotAvailable(_)) => {
                        lacks.insert(index);
                    }
                    // Everything else is a fault: a wrong digest, a reply to a different
                    // question, a dead link.
                    _ => {
                        faults += 1;
                        lacks.insert(index);
                    }
                }
                in_flight.lock().expect("fan-out set poisoned").remove(&index);
                if done {
                    outstanding.lock().expect("fan-out set poisoned").remove(&index);
                }
                if outstanding.lock().expect("fan-out set poisoned").is_empty() {
                    break WorkerEnd::Done;
                }
            };
            (peer.name, served, faults, outcome)
        }));
    }

    for worker in workers {
        let (name, served, faults, outcome) = worker
            .join()
            .map_err(|_| ProtocolError::Malformed("a fan-out worker panicked".into()))?;
        report.chunks_from.insert(name.clone(), served);
        if faults > 0 {
            *report.demerits.entry(name.clone()).or_insert(0) += faults;
        }
        match outcome {
            WorkerEnd::Faulty => report.dropped.push(name),
            WorkerEnd::Exhausted => report.exhausted.push(name),
            WorkerEnd::Done => {}
        }
    }

    let destination = Arc::try_unwrap(destination)
        .map_err(|_| ProtocolError::Malformed("the fetch destination outlived its workers".into()))?;

    // No peer could supply some chunk. Reported as the object not being available rather
    // than naming which: a partial object is not a smaller answer.
    if destination.missing() {
        if let Destination::File { file, .. } = &destination {
            // Leave nothing that looks complete. A file of the right length full of holes
            // would hash to something, and that something is not what was asked for.
            let _ = file.set_len(0);
        }
        return Err(ProtocolError::NotAvailable(id));
    }
    if let Destination::File { file, .. } = &destination {
        file.sync_all()
            .map_err(|e| ProtocolError::Malformed(format!("cannot flush the fetch file: {e}")))?;
    }

    Ok((header, report, destination))
}

fn round_trip<L: LinkAdapter>(
    channel: &mut SecureChannel<L>,
    request: &Request,
    budget: &mut usize,
) -> Result<Response, ProtocolError> {
    if *budget == 0 {
        return Err(ProtocolError::NoProgress);
    }
    *budget -= 1;
    request.validate()?;
    let encoded = content::encode(request)?;
    channel
        .send(&encoded)
        .map_err(|e| ProtocolError::Malformed(e.to_string()))?;
    let frame = channel
        .recv()
        .map_err(|e| ProtocolError::Malformed(e.to_string()))?;
    content::decode(&frame)
}

/// How long a fetch session will wait on a peer before giving up.
pub const FETCH_TIMEOUT: Duration = HANDSHAKE_TIMEOUT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_and_replicated_may_leave_a_node() {
        assert!(may_leave_a_node(Some("public")));
        assert!(may_leave_a_node(Some("replicated")));
    }

    #[test]
    fn every_other_label_is_refused_including_ones_that_do_not_exist_yet() {
        for label in [
            Some("private"),
            Some("shared"),
            Some("Public"),
            Some(""),
            Some("some-future-label"),
            None,
        ] {
            assert!(!may_leave_a_node(label), "{label:?} must not leave a node");
        }
    }

    #[test]
    fn the_allow_list_is_an_allow_list() {
        // Guards the shape of the check itself: if someone rewrites it as a deny-list, a
        // new label would start defaulting to servable and this would fail.
        assert_eq!(SERVABLE_LABELS.len(), 2);
        assert!(!may_leave_a_node(Some("anything-not-in-the-list")));
    }
}

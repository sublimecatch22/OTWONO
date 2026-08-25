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
        let id = request.content_id().to_string();
        if request.validate().is_err() {
            return Response::not_available(id);
        }
        match self.ask_store(peer, request) {
            Some(reply) => self
                .translate(peer, request, &reply, session)
                .unwrap_or(Response::not_available(id)),
            None => Response::not_available(id),
        }
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
            Request::Manifest { .. } => may_go_to_peer(label, sharing.as_ref(), peer),
            // A chunk reply carrying an envelope is a reply nobody should be sending.
            Request::Chunk { .. } if sharing.is_some() => false,
            Request::Chunk { .. } if label == Some("shared") => session.was_released(request.content_id()),
            Request::Chunk { .. } => may_leave_a_node(label),
        };
        if !allowed {
            // Reachable only if otwono-stored regressed: it has already applied its own
            // version of this rule. Loud, because it means the two independent checks
            // disagree and one of them is broken.
            eprintln!(
                "otwono-netd: refusing to serve {}: the store offered it labelled {:?} to {}",
                request.content_id(),
                label.unwrap_or("<absent>"),
                peer.fingerprint()
            );
            return None;
        }
        let content_id = reply.get("content_id")?.as_str()?.to_string();
        if content_id != request.content_id() {
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
        };

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

    /// Put content just fetched from a peer into the neighbourhood cache.
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
            "otwono-netd keeps content fetched from a peer for the neighbourhood",
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

fn request_token(perm_socket: &Path, action: &str, reason: &str) -> Result<String, String> {
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
/// neighbourhood transfers faster because every holder is as good as any other, and this is
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

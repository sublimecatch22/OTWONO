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
use otwono_net::content::{self, ChunkEntry, ChunkPart, ManifestPage, ProtocolError, Request, Response};
use otwono_net::{LinkAdapter, LinkProperties, SecureChannel};
use otwono_proto::{code, Client};
use otwono_store::{ChunkRef, ContentId};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// The labels this daemon will put on a link. Anything else — `private`, `shared`, a typo,
/// a label from a future version — is refused.
pub const SERVABLE_LABELS: [&str; 2] = ["public", "replicated"];

/// Largest object one `net.fetch` will assemble in memory. Matches `store.put`'s inline
/// cap, because an object this daemon cannot hand to the store is one it should not have
/// spent a peer's bandwidth on.
pub const MAX_FETCH_BYTES: u64 = 32 * 1024 * 1024;

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
    pub fn answer(&self, peer: &NodeId, request: &Request) -> Response {
        let id = request.content_id().to_string();
        if request.validate().is_err() {
            return Response::not_available(id);
        }
        match self.ask_store(peer, request) {
            Some(reply) => self
                .translate(request, &reply)
                .unwrap_or(Response::not_available(id)),
            None => Response::not_available(id),
        }
    }

    /// Turn the store's reply into a wire reply, refusing on anything unexpected.
    ///
    /// This is where the second label check happens, on the way out.
    fn translate(&self, request: &Request, reply: &serde_json::Value) -> Option<Response> {
        let label = reply.get("visibility").and_then(|v| v.as_str());
        if !may_leave_a_node(label) {
            // Only reachable if otwono-stored regressed. Loud, because it means the two
            // checks disagree and one of them is broken.
            eprintln!(
                "otwono-netd: refusing to serve {}: the store offered it labelled {:?}",
                request.content_id(),
                label.unwrap_or("<absent>")
            );
            return None;
        }
        let content_id = reply.get("content_id")?.as_str()?.to_string();
        if content_id != request.content_id() {
            return None;
        }
        match request {
            Request::Manifest { .. } => Some(Response::Manifest(ManifestPage {
                content_id,
                size_bytes: reply.get("size_bytes")?.as_u64()?,
                chunking: reply.get("chunking")?.as_str()?.to_string(),
                visibility: label?.to_string(),
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
        let mut broker = Client::connect(&self.perm_socket).ok()?;
        let value = broker
            .call(
                "perm.request",
                json!({
                    "action": otwono_stored::CAPABILITY_SERVE,
                    "reason": "otwono-netd answers peers' content requests from the local store",
                }),
            )
            .ok()?
            .ok()?;
        value.get("token")?.as_str().map(str::to_string)
    }
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
        let response = responder.answer(&peer, &request);
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

    let mut bytes = Vec::with_capacity(header.size_bytes.min(MAX_FETCH_BYTES) as usize);
    let mut refs = Vec::with_capacity(entries.len());
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
        refs.push(got);
    }

    // The last check: the chunks the peer served must be the object that was asked for.
    let derived = ContentId::of(&refs).to_hex();
    if derived != content_id {
        return Err(ProtocolError::ObjectIdMismatch {
            expected: content_id.to_string(),
            actual: derived,
        });
    }

    Ok(FetchedObject {
        content_id: content_id.to_string(),
        visibility: header.visibility,
        chunking: header.chunking,
        bytes,
    })
}

/// Every window of the chunk list, checked as it arrives.
fn fetch_manifest<L: LinkAdapter>(
    channel: &mut SecureChannel<L>,
    content_id: &str,
    link: &LinkProperties,
    budget: &mut usize,
) -> Result<(ManifestPage, Vec<ChunkEntry>), ProtocolError> {
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
        if !may_leave_a_node(Some(&page.visibility)) {
            return Err(ProtocolError::Mismatched(format!(
                "the peer offered {content_id} labelled {:?}, which is not a label content \
                 may be served under",
                page.visibility
            )));
        }
        if page.size_bytes > MAX_FETCH_BYTES {
            return Err(ProtocolError::TooLarge {
                field: "size_bytes",
                asked: page.size_bytes,
                ceiling: MAX_FETCH_BYTES,
            });
        }
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

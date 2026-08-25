//! OTWONO content store daemon.
//!
//! # The one method that matters
//!
//! `store.get` and `store.serve` return the same bytes for the same object, and they are
//! different methods on purpose.
//!
//! `store.get` is a **local** read: a caller on this node's own socket, holding a
//! `store.read` capability, may read anything the store holds. Labels do not gate it,
//! because the label is about the network boundary and not about the local one.
//!
//! `store.serve` is that boundary. It is what `otwono-netd` calls when a peer asks for
//! content, and it **refuses anything but `Public` and `Replicated`** — before it looks at
//! whether the chunks are even present, so that a refusal costs the same whatever the store
//! holds. It carries its own capability so a network daemon can be granted the ability to
//! serve peers without being granted the ability to read the user's private notes.
//!
//! `DATA-VISIBILITY.md` §4 wants this check duplicated in `otwono-netd` as well. That is
//! deliberate defence in depth and it is not an excuse for this one being weak: a bug in
//! either must not be enough to leak.
//!
//! # A refusal must not be a disclosure
//!
//! Asking to serve a `Private` object and asking to serve one that does not exist return
//! the *same* answer. An authorization error would tell a peer that a particular object
//! exists on this node, which for a content-addressed store means confirming the node holds
//! specific bytes the asker already guessed.

#![forbid(unsafe_code)]

use otwono_proto::{
    unknown_method, CallContext, Client, MethodDescription, RpcError, Service, ServiceDescription,
};
use otwono_store::cas::Recipient;
use otwono_store::{
    Cache, CacheError, ContentId, ContentKey, Handoff, HandoffError, Object, Store, StoreError, Visibility,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;

pub const SERVICE_NAME: &str = "otwono-stored";
pub const DESCRIBE_SCHEMA_VERSION: &str = "1.0.0";

pub const CAPABILITY_READ: &str = "store.read";
pub const CAPABILITY_WRITE: &str = "store.write";
pub const CAPABILITY_SERVE: &str = "store.serve";
pub const CAPABILITY_SHARE: &str = "store.share";
/// The capability `otwono-idd` requires to unwrap a content key, named here because
/// `store.open_shared` is guarded by the *same* one and forwards the caller's token.
///
/// A capability token names one action, so this cannot be a different capability that the
/// daemon then trades for an unwrap: doing that would mean anyone with `store.read` could
/// open every object shared with this node, which is precisely the split ADR-0019 §3 makes.
/// A test asserts this string still matches the daemon that checks it.
pub const CAPABILITY_UNWRAP: &str = "id.unwrap_shared";
/// The neighbourhood cache's own pair, deliberately not `store.read`/`store.write`:
/// `otwono-netd` must be able to add what it fetched to the shared cache without being able
/// to write the user's own store (ADR-0015).
pub const CAPABILITY_CACHE_READ: &str = "cache.read";
pub const CAPABILITY_CACHE_WRITE: &str = "cache.write";

/// A cap on one inline object, in **raw** bytes before base64.
///
/// Derived from the transport, not chosen: the control plane is newline-delimited JSON with
/// a 1 MiB line limit (`otwono_proto::MAX_LINE_BYTES`), and base64 costs four characters per
/// three bytes. An earlier version of this constant said 32 MiB, which was a number no
/// caller could ever reach — the server closed the connection on the over-long line and the
/// caller saw a broken pipe rather than a limit. A cap that the transport makes unreachable
/// is not a cap, it is a misleading comment.
///
/// Lifting this needs a streaming interface (a file descriptor passed over the socket, or a
/// chunk-at-a-time method), which this daemon does not have. Until then it is the real
/// ceiling on `store.put`, `store.get`, `cache.put`, `cache.get` and the inline `store.serve`.
pub const MAX_INLINE_BYTES: usize = 640 * 1024;

/// Room left for the JSON-RPC envelope around a base64 body on one line.
const ENVELOPE_RESERVE: usize = 8 * 1024;

/// The arithmetic behind [`MAX_INLINE_BYTES`], asserted rather than trusted.
const _: () = assert!(
    MAX_INLINE_BYTES.div_ceil(3) * 4 + ENVELOPE_RESERVE <= otwono_proto::MAX_LINE_BYTES,
    "MAX_INLINE_BYTES base64s to more than one control-plane line"
);

/// Largest manifest window one `store.serve_manifest` will build. A page is assembled in
/// memory, so an unbounded request would be an allocation a peer chooses. Matches the
/// wire's own ceiling in `otwono_net::content`; the control-plane test asserts they agree,
/// because this daemon must not depend on the transport crate to learn its own limit.
pub const MAX_SERVE_CHUNKS: usize = 4096;

pub struct StoreService {
    store: Store,
    /// Where objects too large for one control-plane line are handed over as files
    /// (ADR-0018). `None` on a daemon started without one, and then `store.export` and
    /// `store.import` say so rather than silently capping at the inline size.
    handoff: Option<Handoff>,
    /// The neighbourhood cache, when this machine contributes one. `None` on a machine
    /// whose capability profile set the budget to zero, or when no cache directory was
    /// configured — and then every cache method answers "not available" rather than
    /// pretending to have cached something.
    cache: Option<Cache>,
    /// Where `otwono-idd` listens, so a content key sealed to this node can be unwrapped
    /// without this daemon ever holding the sharing key (ADR-0019 §3). `None` on a daemon
    /// started without one, and then `store.open_shared` says so.
    id_socket: Option<PathBuf>,
    perm_socket: PathBuf,
}

#[derive(Debug, Deserialize)]
struct DemoteParams {
    content_id: String,
    visibility: Visibility,
}

#[derive(Debug, Deserialize)]
struct PutParams {
    /// Base64. Inline because the control plane is newline-delimited JSON; see
    /// [`MAX_INLINE_BYTES`].
    data: String,
    #[serde(default)]
    visibility: Visibility,
    /// Objects this content was derived from. The stored label is the most restrictive of
    /// these and the requested one, so derivation cannot launder a label.
    #[serde(default)]
    derived_from: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct IdParams {
    content_id: String,
}

/// One window of an object's chunk list, for a peer.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServeManifestParams {
    content_id: String,
    #[serde(default)]
    from_chunk: usize,
    max_chunks: usize,
    #[serde(default)]
    peer: Option<String>,
}

/// One range of one chunk of one object, for a peer.
///
/// `content_id` is not redundant next to `digest`: it is the whole reason a peer cannot use
/// this method to ask whether the node holds arbitrary bytes. See ADR-0017.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServeChunkParams {
    content_id: String,
    digest: String,
    #[serde(default)]
    offset: usize,
    max_bytes: usize,
    #[serde(default)]
    peer: Option<String>,
}

/// Put content fetched from a peer into the neighbourhood cache.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachePutParams {
    /// Base64, subject to [`MAX_INLINE_BYTES`] like `store.put`.
    data: String,
    /// The label the object was served under. Only `public` and `replicated` are accepted,
    /// and an unrecognised label parses as `private` and is therefore refused.
    #[serde(default)]
    visibility: Visibility,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachePinParams {
    content_id: String,
    pinned: bool,
}

/// Write an object out as a file for the caller (ADR-0018).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportParams {
    content_id: String,
}

/// Take an object in from a file the caller owns (ADR-0018).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportParams {
    /// A path the caller owns. Opened with `O_NOFOLLOW` and checked on the descriptor.
    path: String,
    #[serde(default)]
    visibility: Visibility,
    #[serde(default)]
    derived_from: Vec<String>,
}

/// Store bytes encrypted to a set of named recipients (ADR-0019).
///
/// Recipients arrive as **signed bindings**, not bare keys. A bare X25519 key says nothing
/// about whose it is, and this daemon has no way to find out — so it verifies each binding
/// itself and seals to what the signature vouches for. Whoever calls is responsible for
/// having obtained the bindings; this daemon needs no network access to check them.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShareParams {
    /// Base64, subject to [`MAX_INLINE_BYTES`] like `store.put`.
    data: String,
    recipients: Vec<otwono_identity::SharingBinding>,
}

/// The same, from a file the caller owns, for objects past the inline cap.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShareFileParams {
    path: String,
    recipients: Vec<otwono_identity::SharingBinding>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenSharedParams {
    content_id: String,
}

#[derive(Debug, Deserialize)]
struct ServeParams {
    content_id: String,
    /// Which peer is asking, as `otwono-netd` authenticated it.
    ///
    /// It does not affect the decision for `Public` or `Replicated`, which are public to
    /// everyone by definition. For `Shared` it *is* the decision — see
    /// [`StoreService::may_go_to`] for what that means and what it does not.
    #[serde(default)]
    peer: Option<String>,
}

/// Which of this daemon's two stores an object came out of.
///
/// Not exposed on the wire. A peer must not be able to tell whether this node keeps
/// something because its own user wanted it or because a neighbour did — that is exactly
/// the "holding is publishing" cost ADR-0015 names, and there is no reason to make it
/// sharper than it already is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Own,
    Cached,
}

impl StoreService {
    pub fn new(store: Store, perm_socket: PathBuf) -> Self {
        StoreService {
            store,
            handoff: None,
            cache: None,
            id_socket: None,
            perm_socket,
        }
    }

    /// Tell this daemon where the identity daemon is, so it can unwrap what was shared
    /// with this node (ADR-0019 §3).
    ///
    /// Optional, like the export directory: a node that has never been shared with does not
    /// need it, and a daemon started without it says so rather than pretending.
    pub fn with_identity(mut self, id_socket: PathBuf) -> Self {
        self.id_socket = Some(id_socket);
        self
    }

    fn id_socket(&self) -> Result<&PathBuf, RpcError> {
        self.id_socket.as_ref().ok_or_else(|| {
            RpcError::unavailable(
                "this daemon was started without an identity socket, so it cannot ask for a \
                 content key to be unwrapped",
            )
        })
    }

    /// Give this daemon somewhere to hand large objects over.
    pub fn with_handoff(mut self, handoff: Handoff) -> Self {
        self.handoff = Some(handoff);
        self
    }

    fn handoff(&self) -> Result<&Handoff, RpcError> {
        self.handoff.as_ref().ok_or_else(|| {
            RpcError::unavailable(
                "this daemon was started without an export directory, so objects larger than \
                 the inline cap cannot be moved",
            )
        })
    }

    /// Give this daemon a neighbourhood cache to hold peers' content in.
    pub fn with_cache(mut self, cache: Cache) -> Self {
        self.cache = Some(cache);
        self
    }

    fn cache(&self) -> Result<&Cache, RpcError> {
        self.cache.as_ref().ok_or_else(|| {
            RpcError::unavailable(
                "this node contributes no neighbourhood cache; its capability profile set \
                 the budget to zero, or none was configured",
            )
        })
    }

    fn authorize(&self, ctx: &CallContext, action: &str) -> Result<(), RpcError> {
        let token = ctx.capability.as_deref().ok_or_else(|| {
            RpcError::unauthorized(format!(
                "{action} requires a capability token; request one from otwono-permd"
            ))
        })?;
        let mut client = Client::connect(&self.perm_socket).map_err(|e| {
            RpcError::unavailable(format!(
                "cannot reach the permission broker at {}: {e}",
                self.perm_socket.display()
            ))
        })?;
        client
            .call(
                "perm.verify",
                json!({ "token": token, "action": action, "subject": ctx.peer.subject() }),
            )
            .map_err(|e| RpcError::unavailable(format!("permission broker call failed: {e}")))?
            .map(|_| ())
    }

    /// Refuse an inline reply that would not fit one control-plane line.
    ///
    /// Checked here, before the bytes are read, so the caller gets a sentence naming the
    /// method that *can* carry it. Without this the object is assembled, base64-ed, written
    /// to the socket, and then refused by the caller's own reader -- which surfaces as a
    /// transport error with nothing in it about what to do instead.
    fn must_fit_inline(&self, object: &Object) -> Result<(), RpcError> {
        if object.size_bytes > MAX_INLINE_BYTES as u64 {
            return Err(RpcError::invalid_params(format!(
                "{} is {} bytes, over the {MAX_INLINE_BYTES}-byte inline cap; use store.export, \
                 which hands it over as a file (ADR-0018)",
                object.content_id.to_hex(),
                object.size_bytes
            )));
        }
        Ok(())
    }

    fn parse_id(raw: &str) -> Result<ContentId, RpcError> {
        ContentId::from_hex(raw)
            .ok_or_else(|| RpcError::invalid_params(format!("{raw:?} is not a content id")))
    }

    fn handle_put(&self, params: Value) -> Result<Value, RpcError> {
        let p: PutParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.put: {e}")))?;
        let bytes = data_encoding::BASE64
            .decode(p.data.as_bytes())
            .map_err(|e| RpcError::invalid_params(format!("data must be base64: {e}")))?;
        if bytes.len() > MAX_INLINE_BYTES {
            return Err(RpcError::invalid_params(format!(
                "{} bytes is over the {MAX_INLINE_BYTES}-byte inline cap",
                bytes.len()
            )));
        }
        let inputs: Vec<ContentId> = p
            .derived_from
            .iter()
            .map(|s| Self::parse_id(s))
            .collect::<Result<_, _>>()?;
        let object = self
            .store
            .put_derived(&bytes, p.visibility, &inputs)
            .map_err(rpc)?;
        let mut out = record(&object);
        // Said back explicitly, because a caller that asked for Public over a Private input
        // gets Private and needs to know rather than assume.
        out["requested_visibility"] = json!(p.visibility.as_str());
        out["derived_from"] = json!(p.derived_from);
        Ok(out)
    }

    /// Turn signed bindings into recipients, refusing anything that does not check out.
    ///
    /// Every failure is the caller's: they handed over a binding that does not verify, or
    /// two bindings for one node. Sealing to an unverified key would mean sealing to
    /// whoever claimed to be the recipient, which is the one mistake this whole mechanism
    /// exists to prevent.
    fn recipients(bindings: &[otwono_identity::SharingBinding]) -> Result<Vec<Recipient>, RpcError> {
        if bindings.is_empty() {
            return Err(RpcError::invalid_params(
                "no recipients; an object nobody can open is not a shared object",
            ));
        }
        let mut out = Vec::with_capacity(bindings.len());
        for binding in bindings {
            let key = binding.verify().map_err(|e| {
                RpcError::invalid_params(format!(
                    "the binding for {} does not check out ({e}), so there is no key it is \
                     safe to seal to",
                    binding.node_id.to_text()
                ))
            })?;
            let node_id = binding.node_id.to_text();
            if out.iter().any(|r: &Recipient| r.node_id == node_id) {
                return Err(RpcError::invalid_params(format!(
                    "{node_id} appears twice, and there is no way to tell which key is meant"
                )));
            }
            out.push(Recipient {
                node_id,
                sharing_public_key: key,
            });
        }
        Ok(out)
    }

    fn handle_share(&self, params: Value) -> Result<Value, RpcError> {
        let p: ShareParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.share: {e}")))?;
        let bytes = data_encoding::BASE64
            .decode(p.data.as_bytes())
            .map_err(|e| RpcError::invalid_params(format!("data must be base64: {e}")))?;
        if bytes.len() > MAX_INLINE_BYTES {
            return Err(RpcError::invalid_params(format!(
                "{} bytes is over the {MAX_INLINE_BYTES}-byte inline cap; use store.share_file",
                bytes.len()
            )));
        }
        let recipients = Self::recipients(&p.recipients)?;
        let (object, _) = self
            .store
            .put_shared_reader(bytes.as_slice(), &recipients)
            .map_err(rpc)?;
        Ok(record(&object))
    }

    fn handle_share_file(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        let p: ShareFileParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.share_file: {e}")))?;
        // Checked on the descriptor before the recipients are, so a caller naming a file
        // that is not theirs learns nothing about whether their bindings were good.
        let mut file =
            Handoff::open_owned(std::path::Path::new(&p.path), ctx.peer.uid).map_err(handoff_rpc)?;
        let recipients = Self::recipients(&p.recipients)?;
        let (object, _) = self
            .store
            .put_shared_reader(&mut file, &recipients)
            .map_err(rpc)?;
        let mut out = record(&object);
        out["imported_from"] = json!(p.path);
        Ok(out)
    }

    /// Open an object that was shared *with* this node.
    ///
    /// The content key is unwrapped by `otwono-idd`, which holds the sharing key; it comes
    /// back here, where the storage key already is, so no new trust boundary is crossed
    /// (ADR-0019 §3). This daemon never sees the sharing secret and `otwono-netd` is not in
    /// the path at all.
    fn handle_open_shared(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        let p: OpenSharedParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.open_shared: {e}")))?;
        let object = self
            .store
            .get_object(&Self::parse_id(&p.content_id)?)
            .map_err(rpc)?;
        let sharing = object.sharing.as_ref().ok_or_else(|| {
            RpcError::invalid_params(format!(
                "{} is not a sealed object; store.get reads it",
                object.content_id.to_hex()
            ))
        })?;
        if sharing.plaintext_size_bytes > MAX_INLINE_BYTES as u64 {
            return Err(RpcError::invalid_params(format!(
                "{} bytes of plaintext is over the {MAX_INLINE_BYTES}-byte inline cap",
                sharing.plaintext_size_bytes
            )));
        }

        let mut idd = Client::connect(self.id_socket()?).map_err(|e| {
            RpcError::unavailable(format!(
                "cannot reach the identity daemon at {}: {e}",
                self.id_socket().expect("checked above").display()
            ))
        })?;
        let me = idd
            .call("id.fingerprint", json!({}))
            .map_err(|e| RpcError::unavailable(format!("id.fingerprint failed: {e}")))?
            .map_err(|e| RpcError::unavailable(format!("id.fingerprint refused: {}", e.message)))?
            .get("node_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| RpcError::internal("id.fingerprint did not name this node"))?;

        let copy = sharing.copy_for(&me).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "{} was not shared with this node, so there is no copy of its key here",
                object.content_id.to_hex()
            ))
        })?;

        // The caller's own token is forwarded: unwrapping happens on their authority, not
        // on this daemon's, so a caller without id.unwrap_shared cannot borrow one by
        // asking the store instead.
        let token = ctx.capability.as_deref().ok_or_else(|| {
            RpcError::unauthorized(format!("store.open_shared requires a {CAPABILITY_UNWRAP} token"))
        })?;
        let unwrapped = idd
            .call_with_capability("id.unwrap_shared", json!({ "sealed_key": copy }), token)
            .map_err(|e| RpcError::unavailable(format!("id.unwrap_shared failed: {e}")))?
            .map_err(|e| RpcError::unauthorized(format!("id.unwrap_shared refused: {}", e.message)))?;
        let key_bytes: [u8; 32] = data_encoding::BASE64
            .decode(
                unwrapped
                    .get("content_key")
                    .and_then(Value::as_str)
                    .ok_or_else(|| RpcError::internal("id.unwrap_shared returned no key"))?
                    .as_bytes(),
            )
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| RpcError::internal("id.unwrap_shared returned something else"))?;

        let mut plaintext = Vec::with_capacity(sharing.plaintext_size_bytes as usize);
        self.store
            .open_shared(&object, &ContentKey::from_bytes(key_bytes), &mut plaintext)
            .map_err(rpc)?;
        let mut out = record(&object);
        out["data"] = json!(data_encoding::BASE64.encode(&plaintext));
        Ok(out)
    }

    fn handle_get(&self, params: Value) -> Result<Value, RpcError> {
        let p: IdParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.get: {e}")))?;
        let object = self
            .store
            .get_object(&Self::parse_id(&p.content_id)?)
            .map_err(rpc)?;
        self.must_fit_inline(&object)?;
        let bytes = self.store.read_object(&object).map_err(rpc)?;
        let mut out = record(&object);
        out["data"] = json!(data_encoding::BASE64.encode(&bytes));
        Ok(out)
    }

    fn handle_stat(&self, params: Value) -> Result<Value, RpcError> {
        let p: IdParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.stat: {e}")))?;
        let object = self
            .store
            .get_object(&Self::parse_id(&p.content_id)?)
            .map_err(rpc)?;
        let mut out = record(&object);
        out["complete"] = json!(self.store.is_complete(&object));
        Ok(out)
    }

    /// Make an object more restrictive.
    ///
    /// Only ever more restrictive. Widening is `label.promote`, which always confirms, and
    /// this daemon does not hold that capability — a caller wanting it goes to the broker.
    ///
    /// Demotion stops **future** serving. It cannot recall what peers already hold, and the
    /// reply says so rather than letting a UI imply otherwise.
    fn handle_demote(&self, params: Value) -> Result<Value, RpcError> {
        let p: DemoteParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.demote: {e}")))?;
        let object = self
            .store
            .demote(&Self::parse_id(&p.content_id)?, p.visibility)
            .map_err(rpc)?;
        let mut out = record(&object);
        out["recalled_from_peers"] = json!(false);
        out["note"] = json!("future serving stops now; anything a peer already holds cannot be recalled");
        Ok(out)
    }

    /// The whole of the network boundary, in one place.
    ///
    /// Every peer-facing method goes through this and nothing else, so there is one label
    /// check to audit rather than three that must be kept in step. It returns the same
    /// error for absent, private, shared, and unreadable, because a peer that can tell
    /// those apart can enumerate what this node holds.
    fn servable(&self, id: &ContentId, peer: Option<&str>) -> Result<(Object, Source), RpcError> {
        let own = self.store.get_object(id).ok().map(|o| (o, Source::Own));
        // The node's own copy first, then the cache. Both are subject to the same label
        // check below — a cached object carries the label it was served under, and an
        // unrecognised one fails closed like everything else.
        let found = own.or_else(|| {
            self.cache
                .as_ref()
                .and_then(|c| c.stat(id).ok())
                .map(|o| (o, Source::Cached))
        });
        found
            .filter(|(o, source)| Self::may_go_to(o, *source, peer))
            .ok_or_else(|| not_available(id))
    }

    /// Whether this object may go to this peer (ADR-0019 §4).
    ///
    /// `Public` and `Replicated` go to anybody, which is what the label means.
    ///
    /// `Shared` goes only to a peer named in its own envelope, and only out of this node's
    /// own store — never out of the cache. A cached object came from somewhere else, and
    /// `Shared` is not cacheable in the first place; refusing here as well means a bug in
    /// the cache cannot turn into a disclosure.
    ///
    /// **Who says which peer this is.** The name is asserted by `otwono-netd`, after it has
    /// authenticated the peer's NodeID through the Noise handshake. `otwono-netd` is the Z3
    /// hostile-input daemon, so it is worth being precise about what its compromise would
    /// cost here: a compromised `otwono-netd` naming an authorized peer obtains the object's
    /// **ciphertext** and a sealed key it cannot open, because it does not hold the sharing
    /// key (ADR-0010, ADR-0019 §3). The confidentiality of a `SHARED` object is carried by
    /// the encryption; this check limits who can obtain the ciphertext at all, which bounds
    /// offline attack and keeps the recipient list from being enumerable. It is defence in
    /// depth, and it is not the thing standing between a peer and the plaintext.
    ///
    /// A peer that is not named gets exactly what a peer asking for an object this node does
    /// not have gets. Distinguishing them would let anyone enumerate both what a node holds
    /// and who it shares with.
    fn may_go_to(object: &Object, source: Source, peer: Option<&str>) -> bool {
        if object.visibility.may_leave_the_node_unattended() {
            return true;
        }
        if object.visibility != Visibility::Shared || source != Source::Own {
            return false;
        }
        match (peer, &object.sharing) {
            (Some(peer), Some(sharing)) => sharing.names(peer),
            // An anonymous request cannot be on anybody's list. This is the case a local
            // caller hits by leaving `peer` out, and it must fail closed rather than
            // matching everyone.
            _ => false,
        }
    }

    /// Read one chunk from wherever the object was found.
    ///
    /// Neither path counts as a use of the cache: a peer must not be able to keep something
    /// alive in this node's cache by asking for it.
    fn chunk_from(&self, source: Source, r: &otwono_store::ChunkRef) -> Result<Vec<u8>, StoreError> {
        match source {
            Source::Own => self.store.get_chunk(r),
            Source::Cached => self
                .cache
                .as_ref()
                .ok_or_else(|| StoreError::NotFound(r.hex()))?
                .chunk(r)
                .map_err(|e| match e {
                    CacheError::Store(e) => e,
                    other => StoreError::NotFound(other.to_string()),
                }),
        }
    }

    fn read_from(&self, source: Source, object: &Object) -> Result<Vec<u8>, StoreError> {
        match source {
            Source::Own => self.store.read_object(object),
            Source::Cached => self
                .cache
                .as_ref()
                .ok_or_else(|| StoreError::NotFound(object.content_id.to_hex()))?
                .read_for_peer(object)
                .map_err(|e| match e {
                    CacheError::Store(e) => e,
                    other => StoreError::NotFound(other.to_string()),
                }),
        }
    }

    /// One window of an object's chunk list.
    ///
    /// Paginated because a large object's chunk list does not fit in a link frame — a 1 GiB
    /// object is roughly 16000 chunks at ADR-0016's average. `total_chunks` comes back in
    /// every window so a requester can size the job from the first one.
    fn handle_serve_manifest(&self, params: Value) -> Result<Value, RpcError> {
        let p: ServeManifestParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.serve_manifest: {e}")))?;
        if p.max_chunks == 0 {
            return Err(RpcError::invalid_params("max_chunks must be greater than zero"));
        }
        let id = Self::parse_id(&p.content_id)?;
        let (object, _) = self.servable(&id, p.peer.as_deref())?;

        // Saturating rather than indexing: a peer naming a window past the end gets an
        // empty one, which is a true answer, not an error worth distinguishing.
        let want = p.max_chunks.min(MAX_SERVE_CHUNKS);
        let from = p.from_chunk.min(object.chunks.len());
        let to = from.saturating_add(want).min(object.chunks.len());
        let chunks: Vec<Value> = object.chunks[from..to]
            .iter()
            .map(|c| json!({ "blake3": c.blake3, "length": c.length }))
            .collect();

        let mut out = json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "content_id": object.content_id.to_hex(),
            "size_bytes": object.size_bytes,
            "chunking": object.chunking,
            "visibility": object.visibility.as_str(),
            "total_chunks": object.chunks.len(),
            "from_chunk": from,
            "chunks": chunks,
            "served_to": p.peer,
        });
        // What this peer needs to open what it is about to download, and nothing more.
        //
        // Only *their* copy of the content key travels. The object record here holds the
        // whole list; sending it would tell a recipient who else this node shares with,
        // which is OQ-28's leak reaching a stranger rather than staying on disk. Sending
        // one copy is a partial answer to it, and the part that is cheap.
        //
        // Unreachable unless the object is Shared and this peer is named, because
        // `servable` already refused every other case.
        if let (Visibility::Shared, Some(sharing), Some(peer)) =
            (object.visibility, &object.sharing, p.peer.as_deref())
        {
            if let Some(copy) = sharing.copy_for(peer) {
                out["sharing"] = json!({
                    "encryption": sharing.encryption,
                    "nonce_prefix": sharing.nonce_prefix,
                    "plaintext_size_bytes": sharing.plaintext_size_bytes,
                    "sealed_key": copy,
                });
            }
        }
        Ok(out)
    }

    /// One range of one chunk of one object.
    ///
    /// The digest must be in *that object's* chunk list. Without that check this method
    /// would answer "do you hold these exact bytes" for any digest a peer cared to guess,
    /// whatever label the object carrying them had — chunks are shared between objects by
    /// design, so a private object and a public one can contain the same one (ADR-0017).
    fn handle_serve_chunk(&self, params: Value) -> Result<Value, RpcError> {
        let p: ServeChunkParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.serve_chunk: {e}")))?;
        if p.max_bytes == 0 {
            return Err(RpcError::invalid_params("max_bytes must be greater than zero"));
        }
        let id = Self::parse_id(&p.content_id)?;
        let (object, source) = self.servable(&id, p.peer.as_deref())?;

        let entry = object
            .chunks
            .iter()
            .find(|c| c.blake3 == p.digest)
            .ok_or_else(|| not_available(&id))?;
        let digest: [u8; 32] = data_encoding::HEXLOWER
            .decode(entry.blake3.as_bytes())
            .ok()
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| RpcError::internal("a stored chunk digest is not 32 hex bytes"))?;
        let chunk_ref = otwono_store::ChunkRef {
            digest,
            length: entry.length,
        };
        // A damaged or missing chunk is not-available too. The peer asked for an object
        // this node advertises; that it cannot produce the bytes is this node's problem.
        let bytes = self
            .chunk_from(source, &chunk_ref)
            .map_err(|_| not_available(&id))?;

        let from = p.offset.min(bytes.len());
        let to = from
            .saturating_add(p.max_bytes.min(otwono_store::chunk::MAX_CHUNK))
            .min(bytes.len());

        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "content_id": object.content_id.to_hex(),
            "digest": entry.blake3,
            // Sent so otwono-netd can make its own decision before a byte reaches a link
            // (DATA-VISIBILITY.md §4). Public, replicated, or -- since ADR-0019 §4 --
            // shared, when this peer is named in the object's envelope; servable() already
            // refused everything else. The envelope itself is not repeated here: it travels
            // once, with the manifest, and otwono-netd will not release a chunk of a shared
            // object to a peer it has not already given that manifest to.
            "visibility": object.visibility.as_str(),
            "offset": from,
            "total_length": bytes.len(),
            "data": data_encoding::BASE64.encode(&bytes[from..to]),
            "served_to": p.peer,
        }))
    }

    /// The network boundary.
    ///
    /// The label is checked **before** the store is consulted, so that "you may not have
    /// this" and "this is not here" are the same answer and take the same path. A peer
    /// learns nothing about what this node holds by asking.
    fn handle_serve(&self, params: Value) -> Result<Value, RpcError> {
        let p: ServeParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.serve: {e}")))?;
        let id = Self::parse_id(&p.content_id)?;

        let (object, source) = self.servable(&id, p.peer.as_deref())?;
        // A peer asking for something too large for one line is told the object is not
        // available, like every other refusal here: the chunk-at-a-time methods are what a
        // peer uses, and telling one that this node holds something too big to inline would
        // be the disclosure the uniform refusal exists to prevent.
        if object.size_bytes > MAX_INLINE_BYTES as u64 {
            return Err(not_available(&id));
        }
        let bytes = self.read_from(source, &object).map_err(|_| not_available(&id))?;

        let mut out = record(&object);
        out["data"] = json!(data_encoding::BASE64.encode(&bytes));
        out["served_to"] = json!(p.peer);
        Ok(out)
    }

    /// Put content fetched from a peer into the cache.
    ///
    /// The label decides, in the cache itself rather than here: `Private` and `Shared` are
    /// refused whatever a caller claims, and a label this build does not recognise parses as
    /// `Private` and is refused with them.
    ///
    /// The reply says what was evicted to make room, because "serving is carrying" and an
    /// operator who cannot see the cache turning over cannot reason about it.
    fn handle_cache_put(&self, params: Value) -> Result<Value, RpcError> {
        let p: CachePutParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("cache.put: {e}")))?;
        let bytes = data_encoding::BASE64
            .decode(p.data.as_bytes())
            .map_err(|e| RpcError::invalid_params(format!("data must be base64: {e}")))?;
        if bytes.len() > MAX_INLINE_BYTES {
            return Err(RpcError::invalid_params(format!(
                "{} bytes is over the {MAX_INLINE_BYTES}-byte inline cap",
                bytes.len()
            )));
        }
        let cache = self.cache()?;
        let before = cache.used_bytes();
        let object = cache
            .insert(&bytes, p.visibility, otwono_store::cache::now_unix_ms())
            .map_err(cache_rpc)?;
        let after = cache.used_bytes();
        let mut out = record(&object);
        out["cached"] = json!(true);
        out["cache_used_bytes"] = json!(after);
        out["cache_budget_bytes"] = json!(cache.budget_bytes());
        out["evicted_bytes"] = json!((before + object.size_bytes).saturating_sub(after));
        Ok(out)
    }

    /// Read a cached object, counting it as a local use.
    ///
    /// Unlike `store.serve_*`, this **does** refresh the object's place in the eviction
    /// order: a caller on this node's own socket is the operator, and what the operator
    /// reads is what the cache should keep.
    fn handle_cache_get(&self, params: Value) -> Result<Value, RpcError> {
        let p: IdParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("cache.get: {e}")))?;
        let cache = self.cache()?;
        let id = Self::parse_id(&p.content_id)?;
        let object = cache.stat(&id).map_err(cache_rpc)?;
        self.must_fit_inline(&object)?;
        let bytes = cache
            .get(&id, otwono_store::cache::now_unix_ms())
            .map_err(cache_rpc)?;
        let mut out = record(&object);
        out["data"] = json!(data_encoding::BASE64.encode(&bytes));
        out["cached"] = json!(true);
        Ok(out)
    }

    /// What the cache holds, and how much room is left.
    fn handle_cache_status(&self, _params: Value) -> Result<Value, RpcError> {
        let cache = self.cache()?;
        let entries: Vec<Value> = cache
            .entries()
            .iter()
            .map(|e| {
                json!({
                    "content_id": e.content_id,
                    "size_bytes": e.size_bytes,
                    "last_access_ms": e.last_access_ms,
                    "pinned": e.pinned,
                })
            })
            .collect();
        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "budget_bytes": cache.budget_bytes(),
            "used_bytes": cache.used_bytes(),
            "objects": entries.len(),
            "entries": entries,
            // Stated on every call, because an operator has to be told and a UI that has to
            // remember to say it is a UI that will forget.
            "note": "holding is publishing: serving a chunk tells your neighbours you have it",
        }))
    }

    fn handle_cache_pin(&self, params: Value) -> Result<Value, RpcError> {
        let p: CachePinParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("cache.pin: {e}")))?;
        let id = Self::parse_id(&p.content_id)?;
        let found = self.cache()?.set_pinned(&id, p.pinned).map_err(cache_rpc)?;
        if !found {
            return Err(RpcError::invalid_params(format!(
                "{} is not in the cache",
                id.to_hex()
            )));
        }
        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "content_id": id.to_hex(),
            "pinned": p.pinned,
        }))
    }

    /// Empty the cache, pinned objects included.
    ///
    /// "Serving is carrying": an operator holds bytes they did not choose one at a time, so
    /// a purge is always one action away (`NEIGHBOURHOOD-CACHE.md` §6). It touches only the
    /// cache — the user's own store is a different directory and this has no path to it.
    fn handle_cache_purge(&self, _params: Value) -> Result<Value, RpcError> {
        let cache = self.cache()?;
        let freed = cache.purge().map_err(cache_rpc)?;
        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "freed_bytes": freed,
            "used_bytes": cache.used_bytes(),
            "note": "the cache is empty; nothing in the node's own store was touched",
        }))
    }

    /// Write an object out as a file the caller owns.
    ///
    /// The label does not gate this. `store.get` does not either: a label governs the
    /// *network* boundary, and a caller on this node's own socket holding `store.read` is
    /// the operator asking for their own data. What gates it is that the resulting file is
    /// given to the uid the kernel says is on the other end of the socket, and to no one
    /// else.
    fn handle_export(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        let p: ExportParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.export: {e}")))?;
        let id = Self::parse_id(&p.content_id)?;
        let object = self.store.get_object(&id).map_err(rpc)?;
        let handoff = self.handoff()?;

        // Streamed chunk by chunk. The point of this method is objects that do not fit in
        // memory twice, and reading the whole thing to write the whole thing would keep the
        // inline path's worst property while losing its simplicity.
        let refs = object.chunk_refs();
        let store = &self.store;
        let exported = handoff
            .export(ctx.peer.uid, object.size_bytes, move |file| {
                use std::io::Write;
                for r in &refs {
                    let bytes = store
                        .get_chunk(r)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    file.write_all(&bytes)?;
                }
                Ok(())
            })
            .map_err(handoff_rpc)?;

        let mut out = record(&object);
        out["path"] = json!(exported.path.display().to_string());
        out["owner_uid"] = json!(exported.owner_uid);
        out["exported_bytes"] = json!(exported.size_bytes);
        out["note"] = json!(
            "this file is plaintext and yours; read it and unlink it. Anything left is \
             reaped after an hour."
        );
        Ok(out)
    }

    /// Take an object in from a file the caller owns.
    fn handle_import(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        let p: ImportParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.import: {e}")))?;
        // Opened and checked before anything else looks at the path, and never re-opened.
        let mut file =
            Handoff::open_owned(std::path::Path::new(&p.path), ctx.peer.uid).map_err(handoff_rpc)?;
        let inputs: Vec<ContentId> = p
            .derived_from
            .iter()
            .map(|s| Self::parse_id(s))
            .collect::<Result<_, _>>()?;

        let object = self
            .store
            .put_reader(&mut file, p.visibility, &inputs)
            .map_err(rpc)?;
        let mut out = record(&object);
        out["imported_from"] = json!(p.path);
        out["requested_visibility"] = json!(p.visibility.as_str());
        out["derived_from"] = json!(p.derived_from);
        Ok(out)
    }
}

/// A handoff failure, translated for a caller.
fn handoff_rpc(e: HandoffError) -> RpcError {
    match e {
        // The caller named something that is not theirs. One message for every reason.
        HandoffError::NotYours { .. } => RpcError::invalid_params(e.to_string()),
        HandoffError::NoSpace { .. } => RpcError::unavailable(e.to_string()),
        HandoffError::Io { .. } => RpcError::internal(e.to_string()),
    }
}

/// A cache failure, translated for a caller.
fn cache_rpc(e: CacheError) -> RpcError {
    match e {
        // The caller asked for something the label forbids. Their error, and named.
        CacheError::NotCacheable(_) => RpcError::invalid_params(e.to_string()),
        CacheError::LargerThanBudget { .. } => RpcError::invalid_params(e.to_string()),
        // This node's condition, not the caller's request.
        CacheError::NoSpace { .. } | CacheError::Disabled => RpcError::unavailable(e.to_string()),
        CacheError::Store(inner) => rpc(inner),
        CacheError::Io(_) => RpcError::internal(e.to_string()),
    }
}

/// The one thing a peer is ever told when it may not have something.
///
/// Deliberately identical for absent, private, shared, damaged, and
/// not-part-of-that-object. It names the id the peer already sent, and nothing else.
fn not_available(id: &ContentId) -> RpcError {
    RpcError::invalid_params(format!("{} is not available to peers", id.to_hex()))
}

fn record(o: &Object) -> Value {
    let mut out = json!({
        "schema_version": DESCRIBE_SCHEMA_VERSION,
        "content_id": o.content_id.to_hex(),
        "size_bytes": o.size_bytes,
        "chunks": o.chunks.len(),
        "visibility": o.visibility.as_str(),
        "chunking": o.chunking,
    });
    // The envelope, when there is one. Its recipient list is what a caller needs to see to
    // know who can open this — including to notice that it is not what they expected. The
    // sealed copies go with it: they are useless without a sharing secret, and a recipient
    // on another node needs its own copy to hand back for unwrapping.
    if let Some(sharing) = &o.sharing {
        out["sharing"] = json!({
            "encryption": sharing.encryption,
            "plaintext_size_bytes": sharing.plaintext_size_bytes,
            "authorized": sharing.authorized_nodes(),
            "sealed_keys": sharing.sealed_keys,
        });
    }
    out
}

fn rpc(e: StoreError) -> RpcError {
    match e {
        // A caller naming something that is not here, or naming it wrongly.
        StoreError::NotFound(_) => RpcError::invalid_params(e.to_string()),
        // Damage or a wrong key. The node's problem, not the caller's.
        StoreError::Corrupt { .. } | StoreError::Crypt(_) => RpcError::internal(e.to_string()),
        StoreError::Object(_) => RpcError::internal(e.to_string()),
        StoreError::Io { .. } => RpcError::internal(e.to_string()),
        // The caller asked for something only a person may authorize.
        StoreError::Promotion { .. } => RpcError::invalid_params(e.to_string()),
    }
}

impl Service for StoreService {
    fn describe(&self) -> ServiceDescription {
        ServiceDescription {
            schema_version: DESCRIBE_SCHEMA_VERSION.to_string(),
            service: SERVICE_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            methods: vec![
                MethodDescription::open("describe", "Describe this service"),
                MethodDescription::guarded(
                    "store.put",
                    "Store bytes, chunked and content-addressed, under a visibility label",
                    CAPABILITY_WRITE,
                ),
                MethodDescription::guarded(
                    "store.get",
                    "Read an object on this node, whatever its label",
                    CAPABILITY_READ,
                ),
                MethodDescription::guarded(
                    "store.stat",
                    "Describe an object without returning its bytes",
                    CAPABILITY_READ,
                ),
                MethodDescription::guarded(
                    "store.share",
                    "Encrypt bytes to named recipients and store the result (ADR-0019)",
                    CAPABILITY_SHARE,
                ),
                MethodDescription::guarded(
                    "store.share_file",
                    "The same, from a file the caller owns, for objects past the inline cap",
                    CAPABILITY_SHARE,
                ),
                MethodDescription::guarded(
                    "store.open_shared",
                    "Open an object shared with this node, asking otwono-idd for the key",
                    CAPABILITY_UNWRAP,
                ),
                MethodDescription::guarded(
                    "store.demote",
                    "Make an object more restrictive; widening is label.promote and needs a person",
                    CAPABILITY_WRITE,
                ),
                MethodDescription::guarded(
                    "store.serve",
                    "Hand an object to a peer, if its label permits leaving the node",
                    CAPABILITY_SERVE,
                ),
                MethodDescription::guarded(
                    "store.serve_manifest",
                    "One window of a servable object's chunk list, for a peer",
                    CAPABILITY_SERVE,
                ),
                MethodDescription::guarded(
                    "store.serve_chunk",
                    "One range of one chunk of a servable object, for a peer",
                    CAPABILITY_SERVE,
                ),
                MethodDescription::guarded(
                    "cache.put",
                    "Put content fetched from a peer into the neighbourhood cache",
                    CAPABILITY_CACHE_WRITE,
                ),
                MethodDescription::guarded(
                    "cache.get",
                    "Read a cached object, counting it as a local use",
                    CAPABILITY_CACHE_READ,
                ),
                MethodDescription::guarded(
                    "cache.status",
                    "The cache's budget, usage and contents",
                    CAPABILITY_CACHE_READ,
                ),
                MethodDescription::guarded(
                    "cache.pin",
                    "Keep an object in the cache regardless of use, or stop keeping it",
                    CAPABILITY_CACHE_WRITE,
                ),
                MethodDescription::guarded(
                    "store.export",
                    "Write an object out as a file owned by the caller, for objects too large to inline",
                    CAPABILITY_READ,
                ),
                MethodDescription::guarded(
                    "store.import",
                    "Take an object in from a file the caller owns",
                    CAPABILITY_WRITE,
                ),
                MethodDescription::guarded(
                    "cache.purge",
                    "Empty the neighbourhood cache; the node's own store is untouched",
                    CAPABILITY_CACHE_WRITE,
                ),
            ],
        }
    }

    fn call(&self, ctx: &CallContext, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "store.put" => {
                self.authorize(ctx, CAPABILITY_WRITE)?;
                self.handle_put(params)
            }
            "store.get" => {
                self.authorize(ctx, CAPABILITY_READ)?;
                self.handle_get(params)
            }
            "store.stat" => {
                self.authorize(ctx, CAPABILITY_READ)?;
                self.handle_stat(params)
            }
            "store.demote" => {
                self.authorize(ctx, CAPABILITY_WRITE)?;
                self.handle_demote(params)
            }
            "store.serve" => {
                self.authorize(ctx, CAPABILITY_SERVE)?;
                self.handle_serve(params)
            }
            "store.serve_manifest" => {
                self.authorize(ctx, CAPABILITY_SERVE)?;
                self.handle_serve_manifest(params)
            }
            "store.serve_chunk" => {
                self.authorize(ctx, CAPABILITY_SERVE)?;
                self.handle_serve_chunk(params)
            }
            "cache.put" => {
                self.authorize(ctx, CAPABILITY_CACHE_WRITE)?;
                self.handle_cache_put(params)
            }
            "cache.get" => {
                self.authorize(ctx, CAPABILITY_CACHE_READ)?;
                self.handle_cache_get(params)
            }
            "cache.status" => {
                self.authorize(ctx, CAPABILITY_CACHE_READ)?;
                self.handle_cache_status(params)
            }
            "cache.pin" => {
                self.authorize(ctx, CAPABILITY_CACHE_WRITE)?;
                self.handle_cache_pin(params)
            }
            "store.share" => {
                self.authorize(ctx, CAPABILITY_SHARE)?;
                self.handle_share(params)
            }
            "store.share_file" => {
                self.authorize(ctx, CAPABILITY_SHARE)?;
                self.handle_share_file(ctx, params)
            }
            // Guarded by the unwrap capability rather than store.read, because that is the
            // decision being made: opening what somebody shared with this node is the
            // unwrap. The same token is forwarded to otwono-idd, so a caller who can read
            // the store cannot open a shared object by asking the store instead.
            "store.open_shared" => {
                self.authorize(ctx, CAPABILITY_UNWRAP)?;
                self.handle_open_shared(ctx, params)
            }
            "store.export" => {
                self.authorize(ctx, CAPABILITY_READ)?;
                self.handle_export(ctx, params)
            }
            "store.import" => {
                self.authorize(ctx, CAPABILITY_WRITE)?;
                self.handle_import(ctx, params)
            }
            "cache.purge" => {
                self.authorize(ctx, CAPABILITY_CACHE_WRITE)?;
                self.handle_cache_purge(params)
            }
            other => Err(unknown_method(other)),
        }
    }
}

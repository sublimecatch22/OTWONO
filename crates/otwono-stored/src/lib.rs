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

/// Ceiling on one `store.shared_with` reply.
///
/// The scan behind it is over every object either way (ADR-0020), so this bounds the *reply*
/// rather than the work: a page has to fit one control-plane line, and a recipient with
/// thousands of objects pages rather than being handed a megabyte of ids.
pub const MAX_SHARED_ENTRIES: usize = 256;
/// The capability `otwono-idd` requires to unwrap a content key, named here because
/// `store.open_shared` is guarded by the *same* one and forwards the caller's token.
///
/// A capability token names one action, so this cannot be a different capability that the
/// daemon then trades for an unwrap: doing that would mean anyone with `store.read` could
/// open every object shared with this node, which is precisely the split ADR-0019 §3 makes.
/// A test asserts this string still matches the daemon that checks it.
pub const CAPABILITY_UNWRAP: &str = "id.unwrap_shared";
/// The cluster cache's own pair, deliberately not `store.read`/`store.write`:
/// `otwono-netd` must be able to add what it fetched to the shared cache without being able
/// to write the user's own store (ADR-0015).
pub const CAPABILITY_CACHE_READ: &str = "cache.read";
pub const CAPABILITY_CACHE_WRITE: &str = "cache.write";
/// Holding a replica for the cluster, deliberately not `cache.write` (ADR-0026 §10).
///
/// `cache.write` is *keep what I fetched* — the bytes are already on this machine because
/// someone here asked for them. This is *keep what a stranger offered*, on a node that may
/// be unattended. "Cache what I fetch, but do not host for strangers" is a sentence an
/// operator will want to say, and only a separate capability lets them say it.
pub const CAPABILITY_REPLICATE: &str = "cache.replicate";
/// Holding somebody else's addressed envelope until its recipient turns up (ADR-0028 §8).
///
/// Deliberately not `cache.replicate`. A node that caches for its neighbourhood has agreed
/// to hold `PUBLIC` and `REPLICATED` content it can inspect and purge on sight; it has not
/// thereby agreed to carry opaque ciphertext addressed to a stranger. One capability for
/// both would make the second a silent consequence of the first.
pub const CAPABILITY_CARRY: &str = "envelope.carry";
/// Publishing one of this node's own pointers (ADR-0027).
///
/// Its own capability rather than `store.write`: publishing changes what a *name* means to
/// every peer that reads it, where store.write only adds bytes nobody has asked for yet. A
/// person may reasonably want a node that stores things and publishes nothing.
pub const CAPABILITY_PUBLISH: &str = "pointer.publish";
/// Reading this node's own pointer state (ADR-0027).
///
/// Its own capability rather than `store.read`, and the difference is what each one reaches:
/// `store.read` opens objects — every byte the user has stored — where this reads only which
/// names this node publishes and at what sequence. A node that serves peers is deliberately
/// run without `store.read` so a bug in a label check cannot reach private data, and that
/// node still has to know its own next sequence in order to publish at all.
pub const CAPABILITY_POINTER_READ: &str = "pointer.read";
/// Recording what a peer published, and remembering its sequence (ADR-0027 §1).
///
/// Distinct from `pointer.publish`, which is Egress and is about this node's own names. This
/// writes local state only: it is how the rollback defence remembers, and a node that could
/// read pointers but never record them would have no protection at all after the first read.
pub const CAPABILITY_POINTER_WRITE: &str = "pointer.write";

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
    /// The cluster cache, when this machine contributes one. `None` on a machine
    /// whose capability profile set the budget to zero, or when no cache directory was
    /// configured — and then every cache method answers "not available" rather than
    /// pretending to have cached something.
    cache: Option<Cache>,
    /// The pointers this node publishes and has read (ADR-0027). `None` on a daemon started
    /// without one, and then every pointer method says so rather than answering "no such
    /// name" — which would be a different and misleading claim.
    pointers: Option<otwono_store::PointerStore>,
    /// Envelopes this node carries for other people, with the budget it agreed to spend
    /// (ADR-0028 §8). `None` means this node carries no mail at all — a machine whose
    /// capability profile sets the budget to zero, or one started without the store.
    envelopes: Option<(otwono_store::EnvelopeStore, u64)>,
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

/// Put content fetched from a peer into the cluster cache.
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

/// How much room this node has to promise, and what it need not be offered again.
///
/// `candidates` is bounded by the offer page the caller is filtering, and the reply names
/// only ids from it: this answers about what it was asked about, and never returns the
/// cache listing — that is `cache.status`, and it needs `cache.read` (ADR-0026 §10).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplicaRoomParams {
    #[serde(default)]
    candidates: Vec<String>,
}

/// Take one object as a replica and hold it for the owner's term.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TakeReplicaParams {
    /// Base64, subject to [`MAX_INLINE_BYTES`] like `store.put`.
    data: String,
    /// The owner's policy, as it was offered. Validated here rather than trusted: a peer
    /// that offers a zero TTL or a zero size cap is offering something no holder can honour.
    ttl_days: u32,
    max_size_bytes: u64,
    #[serde(default)]
    allow_rereplication: bool,
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

/// Widen an existing shared object's recipient list (ADR-0019 §5).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddRecipientsParams {
    content_id: String,
    recipients: Vec<otwono_identity::SharingBinding>,
}

/// Narrow it. Names, not bindings: removing somebody needs no key, only their name.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveRecipientsParams {
    content_id: String,
    node_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenSharedParams {
    content_id: String,
}

/// Hold a stranger's sealed mail while carrying it (ADR-0031).
///
/// The carriage counterpart of [`AcceptSharedParams`], and separate from it because the two
/// go to different places. A recipient's own mail belongs in the permanent store; a
/// stranger's belongs in the cache, where it has a budget, an eviction policy and a way to
/// be deleted again.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvelopeKeepParams {
    /// The descriptor the peer offered. Carried whole so the carriage policy can be asked
    /// the same question it will be asked again at `envelope.take` — one rule, one place,
    /// no way for the two steps to disagree about whether this envelope is carryable.
    envelope: otwono_envelope::Envelope,
    /// Base64 ciphertext, exactly as it arrived. The id is the envelope's.
    data: String,
    encryption: String,
    nonce_prefix: String,
    plaintext_size_bytes: u64,
    sealed_key: otwono_identity::SealedKey,
}

/// Keep a sealed object fetched from a peer, with the one key that came with it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptSharedParams {
    /// The id this node asked a peer for. Chunking is deterministic, so storing the same
    /// ciphertext must reproduce it; a mismatch is refused.
    content_id: String,
    /// Base64 ciphertext, exactly as it arrived.
    data: String,
    encryption: String,
    nonce_prefix: String,
    plaintext_size_bytes: u64,
    sealed_key: otwono_identity::SealedKey,
}

/// What has this node sealed to one peer (ADR-0020)?
/// What this node is willing to have copied (ADR-0026 §7).
///
/// No peer field, unlike `store.shared_with`: `REPLICATED` means copying is permitted, so
/// the answer is the same for everybody and there is nothing to scope.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplicableParams {
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    max_entries: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedWithParams {
    /// The peer asking, as `otwono-netd` authenticated it. Not optional, and there is no
    /// anonymous form: "what have you sealed to nobody" is not a question.
    peer: String,
    /// Continue after this content id. Absent starts at the beginning.
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    max_entries: Option<usize>,
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
            pointers: None,
            envelopes: None,
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

    /// Give this daemon a cluster cache to hold peers' content in.
    pub fn with_cache(mut self, cache: Cache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Give this daemon somewhere to keep pointers (ADR-0027).
    pub fn with_pointers(mut self, pointers: otwono_store::PointerStore) -> Self {
        self.pointers = Some(pointers);
        self
    }

    /// Give this daemon somewhere to keep other people's mail, and a budget for it.
    ///
    /// Separate from the cluster cache's budget, which is the whole of ADR-0028 §8: holding
    /// neighbourhood content an operator can inspect and purge is a different thing to agree
    /// to than carrying a stranger's sealed mail, and one budget would make the second a
    /// silent consequence of the first.
    pub fn with_envelopes(mut self, store: otwono_store::EnvelopeStore, budget_bytes: u64) -> Self {
        self.envelopes = Some((store, budget_bytes));
        self
    }

    fn envelopes(&self) -> Result<(&otwono_store::EnvelopeStore, u64), RpcError> {
        self.envelopes.as_ref().map(|(s, b)| (s, *b)).ok_or_else(|| {
            RpcError::unavailable(
                "this node carries no mail for other people; its capability profile \
                     set the budget to zero, or none was configured",
            )
        })
    }

    fn pointers(&self) -> Result<&otwono_store::PointerStore, RpcError> {
        self.pointers
            .as_ref()
            .ok_or_else(|| RpcError::unavailable("this node keeps no pointers; none was configured"))
    }

    /// Publish one of this node's own pointers.
    ///
    /// The record arrives already signed. This daemon does not sign and must not: signing
    /// needs the node key, which lives in `otwono-idd` and reaches nothing else (ADR-0010).
    /// What this adds is the monotonicity check, which is the one thing a store can enforce
    /// that a caller might forget — and forgetting it is unrecoverable, because a pointer
    /// that goes backwards is refused as a rollback by every peer that saw the higher one.
    fn handle_pointer_publish(&self, params: Value) -> Result<Value, RpcError> {
        let record = params
            .get("record")
            .ok_or_else(|| RpcError::invalid_params("pointer.publish needs a record"))?;
        let pointer: otwono_pointer::Pointer = serde_json::from_value(record.clone())
            .map_err(|e| RpcError::invalid_params(format!("pointer.publish: {e}")))?;
        self.pointers()?
            .publish(&pointer)
            .map_err(|e| RpcError::invalid_params(format!("pointer.publish: {e}")))?;
        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "service": pointer.service,
            "name": pointer.name,
            "sequence": pointer.sequence,
            "tombstone": pointer.is_tombstone(),
        }))
    }

    /// What sequence this node should sign next for a name.
    fn handle_pointer_next(&self, params: Value) -> Result<Value, RpcError> {
        let service = params.get("service").and_then(Value::as_str).unwrap_or_default();
        let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
        let next = self
            .pointers()?
            .next_sequence(service, name)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(json!({ "schema_version": DESCRIBE_SCHEMA_VERSION, "next_sequence": next }))
    }

    /// One of this node's own pointers, for serving to a peer.
    ///
    /// `record: null` for a name this node does not publish. A distinct error would let a
    /// caller — and through `otwono-netd`, a stranger — tell "no such name" from "refused",
    /// which is the enumeration `not_available` exists to prevent everywhere else.
    fn handle_pointer_mine(&self, params: Value) -> Result<Value, RpcError> {
        let service = params.get("service").and_then(Value::as_str).unwrap_or_default();
        let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
        let found = self
            .pointers()?
            .mine(service, name)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "record": found.map(|p| serde_json::to_value(p).unwrap_or(Value::Null)),
        }))
    }

    /// Take a record a peer served, applying the rollback rules (ADR-0027 §1).
    ///
    /// The whole reason the pointer subsystem has state. `otwono-netd` verifies what it
    /// fetched against the key the handshake proved, then hands it here — the daemon that
    /// owns the sequence log and can make the comparison durable. A rollback is refused with
    /// the sequences named, because "refused" without them is indistinguishable to an
    /// operator from a network fault.
    fn handle_pointer_accept(&self, params: Value) -> Result<Value, RpcError> {
        let record = params
            .get("record")
            .ok_or_else(|| RpcError::invalid_params("pointer.accept needs a record"))?;
        let pointer: otwono_pointer::Pointer = serde_json::from_value(record.clone())
            .map_err(|e| RpcError::invalid_params(format!("pointer.accept: {e}")))?;
        let public_key = params
            .get("public_key")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("pointer.accept needs the owner's public_key"))?;
        let key: [u8; 32] = data_encoding::BASE64
            .decode(public_key.as_bytes())
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| RpcError::invalid_params("public_key must be 32 base64 bytes"))?;
        let expected = otwono_pointer::PointerKey {
            node_id: params
                .get("node_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            service: params
                .get("service")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        };

        use otwono_pointer::SequenceMemory;
        match self.pointers()?.accept(&pointer, &key, &expected) {
            // Three different situations for a caller, so three names rather than a bool: a
            // first read had no rollback protection at all, an advance did, and an unchanged
            // read means nothing moved (ADR-0027 §1).
            Ok(accepted) => Ok(json!({
                "schema_version": DESCRIBE_SCHEMA_VERSION,
                "accepted": true,
                "outcome": match accepted {
                    otwono_pointer::Accepted::FirstSeen => "first_seen",
                    otwono_pointer::Accepted::Advanced { .. } => "advanced",
                    otwono_pointer::Accepted::Unchanged { .. } => "unchanged",
                },
                // What it moved from, so a caller can report the advance rather than just
                // the new number. Absent where nothing moved.
                "from": match accepted {
                    otwono_pointer::Accepted::Advanced { from, .. } => Some(from),
                    _ => None,
                },
                "sequence": pointer.sequence,
            })),
            // A refusal on the merits, not a fault: the record verified and the caller asked
            // correctly. Reported as a reply rather than an error so a caller can tell it
            // from a malformed request and say which of the two happened.
            Err(otwono_pointer::PointerError::Rollback { seen, offered }) => Ok(json!({
                "schema_version": DESCRIBE_SCHEMA_VERSION,
                "accepted": false,
                "rollback": true,
                "seen": seen,
                "offered": offered,
            })),
            Err(otwono_pointer::PointerError::Equivocation { sequence }) => Ok(json!({
                "schema_version": DESCRIBE_SCHEMA_VERSION,
                "accepted": false,
                "equivocation": true,
                "sequence": sequence,
            })),
            Err(e) => Err(RpcError::invalid_params(format!("pointer.accept: {e}"))),
        }
    }

    fn cache(&self) -> Result<&Cache, RpcError> {
        self.cache.as_ref().ok_or_else(|| {
            RpcError::unavailable(
                "this node contributes no cluster cache; its capability profile set \
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
    /// This node's own sharing binding, so it keeps a key to what it shares (ADR-0019 §5).
    fn my_sharing_binding(&self) -> Result<otwono_identity::SharingBinding, RpcError> {
        let socket = self.id_socket()?;
        let mut idd = Client::connect(socket).map_err(|e| {
            RpcError::unavailable(format!(
                "cannot reach the identity daemon at {}: {e}",
                socket.display()
            ))
        })?;
        let value = idd
            .call("id.sharing_binding", json!({}))
            .map_err(|e| RpcError::unavailable(format!("id.sharing_binding failed: {e}")))?
            .map_err(|e| RpcError::unavailable(format!("id.sharing_binding refused: {}", e.message)))?;
        serde_json::from_value(value)
            .map_err(|e| RpcError::internal(format!("id.sharing_binding returned {e}")))
    }

    /// The caller's recipients, plus this node — because an owner who cannot read what they
    /// shared has lost their own file (ADR-0019 §5).
    ///
    /// Refused rather than skipped when the identity daemon cannot be reached: creating an
    /// object whose own author cannot open it is data loss, and doing it quietly is worse
    /// than not doing it.
    fn recipients_including_me(
        &self,
        asked: &[otwono_identity::SharingBinding],
    ) -> Result<Vec<Recipient>, RpcError> {
        // Refused *before* this node is added, so an empty list stays an error. Adding self
        // is an addition to a list somebody meant, not a substitute for one: a caller who
        // named nobody meant to name somebody, and silently turning that into "shared with
        // just me" would answer a question they did not ask.
        if asked.is_empty() {
            return Err(RpcError::invalid_params(
                "no recipients; an object nobody can open is not a shared object",
            ));
        }
        let mine = self.my_sharing_binding()?;
        let mut all: Vec<otwono_identity::SharingBinding> = asked.to_vec();
        // Only if the caller did not already name us. `recipients` refuses a duplicate, and
        // naming yourself explicitly is a reasonable thing for a caller to do.
        if !all.iter().any(|b| b.node_id == mine.node_id) {
            all.push(mine);
        }
        Self::recipients(&all)
    }

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
        let recipients = self.recipients_including_me(&p.recipients)?;
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
        let recipients = self.recipients_including_me(&p.recipients)?;
        let (object, _) = self
            .store
            .put_shared_reader(&mut file, &recipients)
            .map_err(rpc)?;
        let mut out = record(&object);
        out["imported_from"] = json!(p.path);
        Ok(out)
    }

    /// Keep what a peer served, sealed as it arrived (ADR-0019).
    ///
    /// The bytes are ciphertext and are stored as ciphertext. Sealing them again would
    /// produce a different object under a key the sender never issued, and a recipient
    /// holding that would have something its sender could not recognise.
    ///
    /// Only the copy of the key that came with it is kept, so this node's record names one
    /// recipient — itself.
    fn handle_accept_shared(&self, params: Value) -> Result<Value, RpcError> {
        let p: AcceptSharedParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.accept_shared: {e}")))?;
        let bytes = data_encoding::BASE64
            .decode(p.data.as_bytes())
            .map_err(|e| RpcError::invalid_params(format!("data must be base64: {e}")))?;
        if bytes.len() > MAX_INLINE_BYTES {
            return Err(RpcError::invalid_params(format!(
                "{} bytes is over the {MAX_INLINE_BYTES}-byte inline cap",
                bytes.len()
            )));
        }
        let object = self
            .store
            .accept_shared(
                bytes.as_slice(),
                &Self::parse_id(&p.content_id)?,
                otwono_store::object::Sharing {
                    encryption: p.encryption,
                    nonce_prefix: p.nonce_prefix,
                    plaintext_size_bytes: p.plaintext_size_bytes,
                    sealed_keys: vec![p.sealed_key],
                },
            )
            .map_err(rpc)?;
        Ok(record(&object))
    }

    /// Add recipients to an object this node can already open (ADR-0019 §5).
    ///
    /// The content key comes from unwrapping this node's own copy — which is the access
    /// control, not an implementation detail: **you can only widen access to something you
    /// can already open.** Since §5a this node keeps a key to everything it shares, so its
    /// own objects are always addable; an object shared *to* this node is addable too, which
    /// is deliberate — a recipient may pass on what it was given, exactly as it could by
    /// re-sharing the plaintext, and pretending otherwise would be theatre.
    fn handle_add_recipients(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        let p: AddRecipientsParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.add_recipients: {e}")))?;
        let id = Self::parse_id(&p.content_id)?;
        let recipients = Self::recipients(&p.recipients)?;
        let content_key = self.content_key_for(ctx, &id)?;
        let object = self
            .store
            .add_recipients(&id, &content_key, &recipients)
            .map_err(rpc)?;
        Ok(record(&object))
    }

    /// Remove recipients, and say plainly what that does not do.
    fn handle_remove_recipients(&self, params: Value) -> Result<Value, RpcError> {
        let p: RemoveRecipientsParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.remove_recipients: {e}")))?;
        let id = Self::parse_id(&p.content_id)?;
        let (object, removed) = self.store.remove_recipients(&id, &p.node_ids).map_err(rpc)?;
        let mut out = record(&object);
        out["removed"] = json!(removed);
        // Said in the reply, the way store.demote already says it. A caller that reported
        // this as "access revoked" would be lying, and it is this daemon's job to give them
        // the words not to.
        out["note"] = json!(
            "their copy of the key is gone from this record and nothing else is: they may \
             already hold the ciphertext, and they still hold their own key. This stops \
             future serving, not what was already taken. Genuinely revoking access means \
             re-encrypting under a new key and sharing again."
        );
        Ok(out)
    }

    /// This node's own content key for a shared object, by asking `otwono-idd` to unwrap.
    ///
    /// Shared by `store.open_shared` and `store.add_recipients`, because both need exactly
    /// the same thing and getting it twice in two ways is how they drift apart.
    fn content_key_for(&self, ctx: &CallContext, id: &ContentId) -> Result<ContentKey, RpcError> {
        let object = self.store.get_object(id).map_err(rpc)?;
        let sharing = object.sharing.as_ref().ok_or_else(|| {
            RpcError::invalid_params(format!("{} is not a sealed object", object.content_id.to_hex()))
        })?;
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
        let token = ctx.capability.as_deref().ok_or_else(|| {
            RpcError::unauthorized(format!("this call requires a {CAPABILITY_UNWRAP} token"))
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
        Ok(ContentKey::from_bytes(key_bytes))
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
        let id = Self::parse_id(&p.content_id)?;
        let object = self.store.get_object(&id).map_err(rpc)?;
        let plaintext_size = object
            .sharing
            .as_ref()
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "{} is not a sealed object; store.get reads it",
                    object.content_id.to_hex()
                ))
            })?
            .plaintext_size_bytes;
        if plaintext_size > MAX_INLINE_BYTES as u64 {
            return Err(RpcError::invalid_params(format!(
                "{plaintext_size} bytes of plaintext is over the {MAX_INLINE_BYTES}-byte inline cap"
            )));
        }

        let content_key = self.content_key_for(ctx, &id)?;
        let mut plaintext = Vec::with_capacity(plaintext_size as usize);
        self.store
            .open_shared(&object, &content_key, &mut plaintext)
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

    /// Whether this node already holds one named object, and nothing else about it.
    ///
    /// Guarded by `store.write` rather than `store.read`, which looks backwards until you ask
    /// who needs it. `otwono-netd` collects mail addressed to this node on a timer, and a
    /// carrier keeps an envelope until it expires even after handing it over (ADR-0028 §7),
    /// so without this the daemon would re-download the same message every sweep for as long
    /// as the sender's expiry allowed. The authority it needs for that is the authority to
    /// avoid a redundant write — not the authority to read the user's store.
    ///
    /// That distinction is load-bearing. `otwono-netd` is the Z3 hostile-input daemon and
    /// does not hold `store.read`; `the_serving_node_serves_without_ever_holding_store_read`
    /// exists to keep it that way, and this method must not be the thing that quietly
    /// changes it.
    ///
    /// The reply is a bool. No size, no label, no chunk list — a caller that wants any of
    /// that is asking to read the object and can go and hold `store.read`. The caller must
    /// already name an exact content id, so this tells a writer whether its write would be
    /// redundant and does not let anyone enumerate anything.
    fn handle_holds(&self, params: Value) -> Result<Value, RpcError> {
        let p: IdParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.holds: {e}")))?;
        let id = Self::parse_id(&p.content_id)?;
        // Complete, not merely recorded. A half-transferred object is one this node cannot
        // serve or open, so answering "yes" for it would strand the very fetch that would
        // finish it.
        let holds = self
            .store
            .get_object(&id)
            .map(|o| self.store.is_complete(&o))
            .unwrap_or(false);
        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "holds": holds,
        }))
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
            .filter(|(o, source)| Self::may_go_to(o, *source, peer, || self.carried_for(id).is_some()))
            .ok_or_else(|| not_available(id))
    }

    /// Who this node is carrying `id` for, if it is carrying it at all (ADR-0028 §11).
    ///
    /// The exception to ADR-0019's serving rule, and the reason store-and-forward can exist:
    /// that rule admits only a named recipient, so without this an envelope can never reach a
    /// carrier — the sender would refuse to hand it over, and a message would only ever
    /// travel when the sender met the recipient directly, which is the case store-and-forward
    /// is for. Found by running it on three nodes and watching the sender refuse.
    ///
    /// It widens nothing beyond ciphertext. A carrier receives the sealed bytes, a key
    /// sealed to the recipient's sharing key that it cannot open, and the recipient's
    /// NodeID — which it already read off the descriptor before it asked.
    ///
    /// Returns the recipient rather than a bool because the manifest needs it: the copy of
    /// the content key that travels is *the recipient's*, not the asking peer's, and the
    /// asking peer has none.
    fn carried_for(&self, id: &ContentId) -> Option<String> {
        let (store, _) = self.envelopes.as_ref()?;
        let custody = store
            .get(&id.to_hex(), otwono_store::cache::now_unix_ms())
            .ok()??;
        Some(custody.envelope.recipient)
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
    /// `carried` is consulted **inside** the `Shared`-and-own-store guard, never beside it.
    /// Custody records are keyed by content id and created by a capability a local operator
    /// holds, so a carriage exception applied before the label check would be a way to make
    /// a `PRIVATE` object servable by taking custody of its id. Placed here it can only ever
    /// widen the audience of an object that was already going to leave this node sealed.
    ///
    /// A closure rather than a bool because it reads the custody store from disk, and the
    /// overwhelmingly common case — a public object, or a peer that is on the list — must
    /// not pay for it.
    fn may_go_to(object: &Object, source: Source, peer: Option<&str>, carried: impl Fn() -> bool) -> bool {
        if object.visibility.may_leave_the_node_unattended() {
            return true;
        }
        if object.visibility != Visibility::Shared {
            return false;
        }
        // Where it was found decides which rule applies, and the two are not the same rule.
        //
        // A `SHARED` object in **this node's own store** is normally mail addressed to this
        // node, which it can open. Serving those is ADR-0019's rule: the asking peer must be
        // named on the sealed-key list, and the carriage exception below is the one way past
        // it.
        //
        // A `SHARED` object in **the cache** is only ever a carried envelope (ADR-0031) —
        // `insert` refuses the label and `take_carried` is the one door that does not. So
        // custody is the whole of the rule there: no custody, no serving, and a recipient's
        // own mail can never reach this branch because it is never put in the cache.
        if source == Source::Cached {
            return carried();
        }
        match (peer, &object.sharing) {
            (Some(peer), Some(sharing)) => sharing.names(peer) || carried(),
            // An anonymous request cannot be on anybody's list. This is the case a local
            // caller hits by leaving `peer` out, and it must fail closed rather than
            // matching everyone. Carriage does not change that: an envelope goes to a peer
            // this daemon can name, and a request with no peer names nobody.
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
        //
        // When this node is *carrying* the object, the copy that travels is the one sealed
        // to the recipient named in the custody record, not to the peer asking: a carrier is
        // not on the recipient list and has no copy of its own, and an envelope that arrived
        // without a key would be undeliverable when the recipient finally collected it.
        let carried = self.carried_for(&id);
        let key_for = carried.as_deref().or(p.peer.as_deref());
        if let (Visibility::Shared, Some(sharing), Some(peer)) = (object.visibility, &object.sharing, key_for)
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
    /// Answer "what have you sealed to me?" for one peer (ADR-0020).
    ///
    /// Out of this node's own store only, never the cache: a cached object came from
    /// somewhere else, and `Shared` is not cacheable in the first place.
    ///
    /// A peer with nothing gets an empty list, and so does a peer this node has never shared
    /// with — the same answer, so asking cannot distinguish "nothing for you" from "nothing
    /// for anybody". That is the discipline `not_available` already applies to one object,
    /// applied to the question of whether there are any.
    fn handle_shared_with(&self, params: Value) -> Result<Value, RpcError> {
        let p: SharedWithParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.shared_with: {e}")))?;
        let after = match &p.after {
            Some(raw) => Some(Self::parse_id(raw)?),
            None => None,
        };
        let limit = p
            .max_entries
            .unwrap_or(MAX_SHARED_ENTRIES)
            .min(MAX_SHARED_ENTRIES);
        if limit == 0 {
            return Err(RpcError::invalid_params("max_entries must be greater than zero"));
        }
        let entries = self
            .store
            .shared_with(&p.peer, after.as_ref(), limit)
            .map_err(rpc)?;
        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "peer": p.peer,
            "entries": entries
                .iter()
                .map(|e| json!({
                    "content_id": e.content_id.to_hex(),
                    "plaintext_size_bytes": e.plaintext_size_bytes,
                }))
                .collect::<Vec<_>>(),
        }))
    }

    /// What this node is willing to have copied (ADR-0026 §7).
    ///
    /// `target_replicas` is deliberately absent from every entry: a holder cannot count
    /// replicas, so it could not act on the number, and returning a figure nobody can use
    /// invites a UI to be built on it.
    fn handle_replicable(&self, params: Value) -> Result<Value, RpcError> {
        let p: ReplicableParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.replicable: {e}")))?;
        let after = match &p.after {
            Some(raw) => Some(Self::parse_id(raw)?),
            None => None,
        };
        let limit = p
            .max_entries
            .unwrap_or(MAX_SHARED_ENTRIES)
            .min(MAX_SHARED_ENTRIES);
        if limit == 0 {
            return Err(RpcError::invalid_params("max_entries must be greater than zero"));
        }
        let entries = self.store.replicable(after.as_ref(), limit).map_err(rpc)?;
        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "entries": entries
                .iter()
                .map(|(id, policy, size)| json!({
                    "content_id": id.to_hex(),
                    "size_bytes": size,
                    "ttl_days": policy.ttl_days,
                    "max_size_bytes": policy.max_size_bytes,
                    "allow_rereplication": policy.allow_rereplication,
                }))
                .collect::<Vec<_>>(),
        }))
    }

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
    /// How much this node will still promise, and which offered ids it already has.
    ///
    /// Lapsed holds are swept before the arithmetic, because the question is about room and
    /// a hold that has run out is not occupying any (ADR-0026 §9). A node with no cache
    /// answers `replicating: false` rather than erroring: "this node does not replicate" is
    /// a true and useful answer to the question, not a fault.
    /// Take custody of one envelope, if this node's own terms allow it (ADR-0028 §2).
    ///
    /// The decision belongs to `CarryPolicy` and is not restated here. What this adds is the
    /// budget arithmetic — room is the agreed budget minus what is already committed — and
    /// the fact that a refusal is a *reply*, not an error. A full node saying "not this one"
    /// is the normal case on a small machine, exactly as `cache.take_replica` returns
    /// `Ok(None)` rather than failing (ADR-0026 §8).
    /// Hold the ciphertext of an envelope this node is about to carry (ADR-0031).
    ///
    /// Separate from `envelope.take`, and called first, because a custody record naming
    /// bytes this node does not have is a promise it cannot keep: the carrier would offer
    /// the envelope and fail to serve it, and on a mesh where the sender may be gone that
    /// loses the message.
    ///
    /// It goes to the **cache**, not the permanent store. A carrier accumulates other
    /// people's mail at their request, and the permanent store has no delete — releasing
    /// custody there frees the record and leaves the bytes for ever. The cache has a
    /// budget, eviction and `remove`, which is what makes ADR-0028 §7's bounds mean
    /// anything on disk as well as in the index.
    ///
    /// The same carriage policy that will decide `envelope.take` decides here, so an
    /// envelope this node will refuse to carry never costs it a byte. Refusal is reported
    /// the same way, with the same codes: a full node saying no is normal.
    fn handle_envelope_keep(&self, params: Value) -> Result<Value, RpcError> {
        let p: EnvelopeKeepParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("envelope.keep: {e}")))?;
        p.envelope
            .validate()
            .map_err(|e| RpcError::invalid_params(format!("envelope.keep: {e}")))?;
        let bytes = data_encoding::BASE64
            .decode(p.data.as_bytes())
            .map_err(|e| RpcError::invalid_params(format!("data must be base64: {e}")))?;
        if bytes.len() > MAX_INLINE_BYTES {
            return Err(RpcError::invalid_params(format!(
                "{} bytes is over the {MAX_INLINE_BYTES}-byte inline cap",
                bytes.len()
            )));
        }
        let id = Self::parse_id(&p.envelope.envelope_id)?;
        let cache = self.cache.as_ref().ok_or_else(|| {
            RpcError::unavailable("this node has no cache, so it has nowhere to put other people's mail")
        })?;

        let (store, budget) = self.envelopes()?;
        let now = otwono_store::cache::now_unix_ms();
        let committed = store
            .bytes_held(now)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        let policy = otwono_envelope::CarryPolicy::with_room(budget.saturating_sub(committed));
        let until_ms = match policy.decide(&p.envelope, now) {
            otwono_envelope::Carry::Accept { until_ms } => until_ms,
            otwono_envelope::Carry::Decline(why) => {
                return Ok(json!({
                    "schema_version": DESCRIBE_SCHEMA_VERSION,
                    "kept": false,
                    "declined": why.code(),
                    "detail": why.to_string(),
                }))
            }
        };

        let sharing = otwono_store::object::Sharing {
            encryption: p.encryption,
            nonce_prefix: p.nonce_prefix,
            plaintext_size_bytes: p.plaintext_size_bytes,
            sealed_keys: vec![p.sealed_key],
        };
        match cache
            .take_carried(bytes.as_slice(), &id, sharing, until_ms, now)
            .map_err(cache_rpc)?
        {
            Some(object) => Ok(json!({
                "schema_version": DESCRIBE_SCHEMA_VERSION,
                "kept": true,
                "content_id": object.content_id.to_hex(),
                "until_ms": until_ms,
            })),
            // The cache would not make room. Reported as a refusal rather than an error for
            // the reason every other carriage refusal is: a small machine declining is the
            // normal case, and the caller must simply not take custody.
            None => Ok(json!({
                "schema_version": DESCRIBE_SCHEMA_VERSION,
                "kept": false,
                "declined": "no_room",
                "detail": "the cache would not make room for it",
            })),
        }
    }

    fn handle_envelope_take(&self, params: Value) -> Result<Value, RpcError> {
        let record = params
            .get("envelope")
            .ok_or_else(|| RpcError::invalid_params("envelope.take needs an envelope"))?;
        let envelope: otwono_envelope::Envelope = serde_json::from_value(record.clone())
            .map_err(|e| RpcError::invalid_params(format!("envelope.take: {e}")))?;
        envelope
            .validate()
            .map_err(|e| RpcError::invalid_params(format!("envelope.take: {e}")))?;

        let (store, budget) = self.envelopes()?;
        let now = otwono_store::cache::now_unix_ms();
        let committed = store
            .bytes_held(now)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        let policy = otwono_envelope::CarryPolicy::with_room(budget.saturating_sub(committed));

        match store
            .take(&envelope, &policy, now)
            .map_err(|e| RpcError::internal(e.to_string()))?
        {
            Ok(held) => {
                // The bytes were kept a moment ago, by a policy asked the same question with
                // an earlier `now`, so its deadline is the earlier of the two. Without this
                // the record outlives the hold and the envelope can be evicted while this
                // node is still telling peers it holds it. A no-op when there is no cache
                // entry, which is every node that has not moved to ADR-0031's path yet.
                if let Some(cache) = self.cache.as_ref() {
                    if let Ok(id) = Self::parse_id(&envelope.envelope_id) {
                        let _ = cache.hold_carried_until(&id, held.until_ms);
                    }
                }
                Ok(json!({
                    "schema_version": DESCRIBE_SCHEMA_VERSION,
                    "taken": true,
                    "until_ms": held.until_ms,
                }))
            }
            // Named, so an operator reading a log can tell a full machine from a late
            // envelope from a peer sending nonsense. `declined` is the stable code to match
            // on; `detail` carries the numbers behind it, because "no_room" without them
            // does not distinguish an oversized envelope from a full disk.
            Err(why) => Ok(json!({
                "schema_version": DESCRIBE_SCHEMA_VERSION,
                "taken": false,
                "declined": why.code(),
                "detail": why.to_string(),
            })),
        }
    }

    /// What this node is carrying — everything, or one recipient's (ADR-0028 §9).
    ///
    /// The scoping happens *here*, not in `otwono-netd`, and that placement is the point: the
    /// daemon answering a scoped question never receives the full bag, so it cannot leak one
    /// by a mistake in its own filtering. `recipient` absent is the broad question a
    /// prospective carrier asks; present is the collection question, and the caller passes
    /// the NodeID its handshake authenticated rather than anything a peer sent.
    fn handle_envelope_held(&self, params: Value) -> Result<Value, RpcError> {
        let (store, _) = self.envelopes()?;
        let now = otwono_store::cache::now_unix_ms();
        let held = match params.get("recipient").and_then(Value::as_str) {
            Some(text) => {
                let node = otwono_identity::NodeId::parse(text)
                    .map_err(|e| RpcError::invalid_params(format!("recipient: {e}")))?;
                store.held_for(&node, now)
            }
            None => store.held(now),
        }
        .map_err(|e| RpcError::internal(e.to_string()))?;

        // Drop records whose bytes this node no longer has (ADR-0031).
        //
        // A custody record and the ciphertext it names live in two places and can come apart:
        // the cache evicted it, a keep failed after the record was written, a disk was
        // restored without one of them. What is left is a carrier that offers an envelope,
        // is asked for it, and cannot answer — and on a mesh where the sender may be gone
        // that is a message nobody can retrieve while a node keeps advertising it.
        //
        // Asked of both places, not just the cache: a node upgraded across ADR-0031 has
        // records pointing at objects in its permanent store, and treating those as missing
        // would throw away real mail. The question is only "do I have these bytes at all",
        // which is `servable`'s lookup without the label check.
        //
        // Swept here, like the expiry sweep above it, for ADR-0026 §9's reason: this is
        // already a moment when the node is doing carriage work, and a subsystem that needs a
        // timer needs a timer that runs.
        let held: Vec<_> = held
            .into_iter()
            .filter(|c| {
                let Ok(id) = Self::parse_id(&c.envelope.envelope_id) else {
                    return false;
                };
                if self.store.get_object(&id).is_ok()
                    || self.cache.as_ref().is_some_and(|cache| cache.contains(&id))
                {
                    return true;
                }
                eprintln!(
                    "otwono-stored: dropping custody of {} — this node no longer has the bytes",
                    c.envelope.envelope_id
                );
                let _ = store.release(&c.envelope.envelope_id);
                false
            })
            .collect();

        // The sender's descriptor, never this carrier's own commitments: `took_at_ms` and
        // `until_ms` are on this carrier's clock and mean nothing to anyone else (§10).
        let entries: Vec<Value> = held
            .iter()
            .map(|c| {
                json!({
                    "envelope_id": c.envelope.envelope_id,
                    "recipient": c.envelope.recipient,
                    "size_bytes": c.envelope.size_bytes,
                    "expires_at_ms": c.envelope.expires_at_ms,
                })
            })
            .collect();
        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "entries": entries,
        }))
    }

    /// Give up custody: delivered, or dropped.
    ///
    /// Idempotent, because delivery races the sweep and a carrier that handed an envelope
    /// over and found it already gone has nothing to worry about.
    fn handle_envelope_release(&self, params: Value) -> Result<Value, RpcError> {
        let envelope_id = params
            .get("envelope_id")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("envelope.release needs an envelope_id"))?;
        let (store, _) = self.envelopes()?;
        store
            .release(envelope_id)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        // And the bytes (ADR-0031). Releasing the record alone is what made drop on delivery
        // free a budget and no disk: the carriage index emptied and the ciphertext stayed.
        //
        // Only ever a carried envelope's own id, so this cannot be used to evict something
        // else — `envelope_id` *is* the content id, and reaching here means this node held a
        // custody record under it. A failure is not escalated: custody is already gone, the
        // caller has been told the truth, and an entry the cache would not drop is evictable
        // like anything else once its hold lapses.
        let freed = match (self.cache.as_ref(), Self::parse_id(envelope_id)) {
            (Some(cache), Ok(id)) => cache.remove(&id).unwrap_or(0),
            _ => 0,
        };
        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "released": true,
            "freed_bytes": freed,
        }))
    }

    fn handle_replica_room(&self, params: Value) -> Result<Value, RpcError> {
        let p: ReplicaRoomParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("cache.replica_room: {e}")))?;
        let Some(cache) = self.cache.as_ref() else {
            return Ok(json!({
                "schema_version": DESCRIBE_SCHEMA_VERSION,
                "replicating": false,
                "room_bytes": 0,
                "already_held": Vec::<String>::new(),
            }));
        };
        match otwono_store::ReplicaHolder::replica_room(
            cache,
            &p.candidates,
            otwono_store::cache::now_unix_ms(),
        ) {
            Some(room) => Ok(json!({
                "schema_version": DESCRIBE_SCHEMA_VERSION,
                "replicating": true,
                "room_bytes": room.room_bytes,
                "already_held": room.already_held,
            })),
            None => Ok(json!({
                "schema_version": DESCRIBE_SCHEMA_VERSION,
                "replicating": false,
                "room_bytes": 0,
                "already_held": Vec::<String>::new(),
            })),
        }
    }

    /// Hold one object as a replica for the cluster (ADR-0026).
    ///
    /// `cache.put` with a promise attached, and the same inline cap. The budget check is
    /// here rather than at the caller for the reason the whole method is here: this is the
    /// only process that can see the budget, and a caller's arithmetic about somebody
    /// else's index is a guess.
    ///
    /// `taken: false` is a refusal, not an error. The budget may have filled since the
    /// caller asked, or the object may be larger than it was advertised as — both are "not
    /// this one", and a caller that treated them as failures would retry a decision this
    /// node already made.
    fn handle_take_replica(&self, params: Value) -> Result<Value, RpcError> {
        let p: TakeReplicaParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("cache.take_replica: {e}")))?;
        let bytes = data_encoding::BASE64
            .decode(p.data.as_bytes())
            .map_err(|e| RpcError::invalid_params(format!("data must be base64: {e}")))?;
        if bytes.len() > MAX_INLINE_BYTES {
            return Err(RpcError::invalid_params(format!(
                "{} bytes is over the {MAX_INLINE_BYTES}-byte inline cap",
                bytes.len()
            )));
        }
        let policy = otwono_store::object::Replication {
            // Not the owner's number. A holder holds one copy; `target_replicas` is a wish
            // about the cluster that no single holder can act on (ADR-0026 §2, §3), and
            // carrying it here would invite somebody to try.
            target_replicas: 1,
            ttl_days: p.ttl_days,
            max_size_bytes: p.max_size_bytes,
            allow_rereplication: p.allow_rereplication,
        };
        policy
            .validate()
            .map_err(|e| RpcError::invalid_params(format!("cache.take_replica: {e}")))?;
        let cache = self.cache()?;
        let taken = otwono_store::ReplicaHolder::take_replica(
            cache,
            &bytes,
            &policy,
            otwono_store::cache::now_unix_ms(),
        )
        .map_err(RpcError::internal)?;
        Ok(match taken {
            Some(t) => json!({
                "schema_version": DESCRIBE_SCHEMA_VERSION,
                "taken": true,
                "content_id": t.content_id,
                "size_bytes": t.size_bytes,
                "cache_used_bytes": cache.used_bytes(),
                "cache_budget_bytes": cache.budget_bytes(),
            }),
            None => json!({
                "schema_version": DESCRIBE_SCHEMA_VERSION,
                "taken": false,
                "cache_used_bytes": cache.used_bytes(),
                "cache_budget_bytes": cache.budget_bytes(),
            }),
        })
    }

    fn handle_cache_status(&self, _params: Value) -> Result<Value, RpcError> {
        let cache = self.cache()?;
        let now = otwono_store::cache::now_unix_ms();
        let entries: Vec<Value> = cache
            .entries()
            .iter()
            .map(|e| {
                json!({
                    "content_id": e.content_id,
                    "size_bytes": e.size_bytes,
                    "last_access_ms": e.last_access_ms,
                    "pinned": e.pinned,
                    // Reported separately from `pinned` because they are separate decisions:
                    // a pin is a person keeping something, a hold is a promise to a peer
                    // that runs out (ADR-0026). Folding them together would let a TTL sweep
                    // look like it had unpinned something.
                    "holds_a_replica": e.holds_a_live_replica(now),
                    "replica_expires_ms": e.replica_expires_ms,
                })
            })
            .collect();
        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "budget_bytes": cache.budget_bytes(),
            "used_bytes": cache.used_bytes(),
            "objects": entries.len(),
            "replicas_held": cache.replicas_held(now),
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
    /// a purge is always one action away (`CLUSTER-CACHE.md` §6). It touches only the
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
                    "store.accept_shared",
                    "Keep a sealed object fetched from a peer, with the key that came with it",
                    CAPABILITY_WRITE,
                ),
                MethodDescription::guarded(
                    "store.add_recipients",
                    "Seal an object this node can open to further recipients (ADR-0019 §5)",
                    CAPABILITY_UNWRAP,
                ),
                MethodDescription::guarded(
                    "store.remove_recipients",
                    "Delete recipients' copies of the key. Recalls nothing already taken",
                    CAPABILITY_WRITE,
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
                    "store.shared_with",
                    "List what this node has sealed to one peer, and nothing else",
                    CAPABILITY_SERVE,
                ),
                MethodDescription::guarded(
                    "store.replicable",
                    "List what this node is willing to have copied (ADR-0026)",
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
                    "Put content fetched from a peer into the cluster cache",
                    CAPABILITY_CACHE_WRITE,
                ),
                MethodDescription::guarded(
                    "pointer.publish",
                    "Publish one of this node's own signed pointers",
                    CAPABILITY_PUBLISH,
                ),
                MethodDescription::guarded(
                    "pointer.next_sequence",
                    "What sequence this node should sign next for a name",
                    CAPABILITY_POINTER_READ,
                ),
                MethodDescription::guarded(
                    "pointer.accept",
                    "Take a record a peer served, refusing a rollback",
                    CAPABILITY_POINTER_WRITE,
                ),
                MethodDescription::guarded(
                    "envelope.keep",
                    "Hold the ciphertext of an envelope this node is about to carry",
                    CAPABILITY_CARRY,
                ),
                MethodDescription::guarded(
                    "envelope.take",
                    "Take custody of an envelope addressed to somebody else (ADR-0028)",
                    CAPABILITY_CARRY,
                ),
                MethodDescription::guarded(
                    "envelope.held",
                    "What this node carries; scoped to one recipient when asked",
                    CAPABILITY_CARRY,
                ),
                MethodDescription::guarded(
                    "envelope.release",
                    "Give up custody of an envelope, delivered or dropped",
                    CAPABILITY_CARRY,
                ),
                MethodDescription::guarded(
                    "pointer.mine",
                    "One of this node's own pointers, for serving to a peer",
                    CAPABILITY_SERVE,
                ),
                MethodDescription::guarded(
                    "cache.replica_room",
                    "How much this node will still promise, and which offered ids it holds",
                    CAPABILITY_REPLICATE,
                ),
                MethodDescription::guarded(
                    "cache.take_replica",
                    "Hold an object offered by a peer as a replica for the cluster",
                    CAPABILITY_REPLICATE,
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
                    "store.holds",
                    "Whether one named object is already here and complete, and nothing else about it",
                    CAPABILITY_WRITE,
                ),
                MethodDescription::guarded(
                    "cache.purge",
                    "Empty the cluster cache; the node's own store is untouched",
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
            "store.holds" => {
                self.authorize(ctx, CAPABILITY_WRITE)?;
                self.handle_holds(params)
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
            // store.serve, not a capability of its own. It is the same decision -- may this
            // daemon hand things to peers -- and every id in the reply is one the asking
            // peer could already fetch with this very capability. A separate one would also
            // mean otwono-netd carrying two tokens for one conversation (ADR-0019 §4).
            "store.shared_with" => {
                self.authorize(ctx, CAPABILITY_SERVE)?;
                self.handle_shared_with(params)
            }
            // Guarded by store.serve, like the sharing index and for the same reason: it is
            // otwono-netd handing things to peers, and every id in the reply is one the
            // asking peer could already fetch with this very capability.
            "store.replicable" => {
                self.authorize(ctx, CAPABILITY_SERVE)?;
                self.handle_replicable(params)
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
            "pointer.publish" => {
                self.authorize(ctx, CAPABILITY_PUBLISH)?;
                self.handle_pointer_publish(params)
            }
            // pointer.read, not pointer.publish and not store.read. Not publish, because
            // this reads local state and moves nothing off the machine -- and since Egress
            // tokens are one-shot by default, that guard would make every caller burn a
            // publish token to ask a question. Not store.read either: that opens objects,
            // and a node that serves peers is deliberately run without it.
            "pointer.next_sequence" => {
                self.authorize(ctx, CAPABILITY_POINTER_READ)?;
                self.handle_pointer_next(params)
            }
            // store.serve, not pointer.publish: this is the read side, and it is what
            // otwono-netd calls to answer a peer. Guarding it with the publish capability
            // would mean a node could not serve what it publishes without also being able to
            // publish more.
            "pointer.accept" => {
                self.authorize(ctx, CAPABILITY_POINTER_WRITE)?;
                self.handle_pointer_accept(params)
            }
            "envelope.keep" => {
                self.authorize(ctx, CAPABILITY_CARRY)?;
                self.handle_envelope_keep(params)
            }
            "envelope.take" => {
                self.authorize(ctx, CAPABILITY_CARRY)?;
                self.handle_envelope_take(params)
            }
            "envelope.held" => {
                self.authorize(ctx, CAPABILITY_CARRY)?;
                self.handle_envelope_held(params)
            }
            "envelope.release" => {
                self.authorize(ctx, CAPABILITY_CARRY)?;
                self.handle_envelope_release(params)
            }
            "pointer.mine" => {
                self.authorize(ctx, CAPABILITY_SERVE)?;
                self.handle_pointer_mine(params)
            }
            "cache.replica_room" => {
                self.authorize(ctx, CAPABILITY_REPLICATE)?;
                self.handle_replica_room(params)
            }
            "cache.take_replica" => {
                self.authorize(ctx, CAPABILITY_REPLICATE)?;
                self.handle_take_replica(params)
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
            // store.write, not store.share: this stores bytes a peer already handed over,
            // under a key this node was given. It names one recipient -- itself -- so it
            // makes nothing reachable that was not already.
            "store.accept_shared" => {
                self.authorize(ctx, CAPABILITY_WRITE)?;
                self.handle_accept_shared(params)
            }
            // Guarded by the unwrap capability, not store.share: adding a recipient needs
            // the content key, and the only way to have it is to open the object. The token
            // is forwarded to otwono-idd exactly as store.open_shared forwards it.
            "store.add_recipients" => {
                self.authorize(ctx, CAPABILITY_UNWRAP)?;
                self.handle_add_recipients(ctx, params)
            }
            // store.write, not store.share: removing is narrowing, like demote, and needs no
            // key at all.
            "store.remove_recipients" => {
                self.authorize(ctx, CAPABILITY_WRITE)?;
                self.handle_remove_recipients(params)
            }
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

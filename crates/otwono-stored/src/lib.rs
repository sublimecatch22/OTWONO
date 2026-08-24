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
use otwono_store::{ContentId, Object, Store, StoreError, Visibility};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;

pub const SERVICE_NAME: &str = "otwono-stored";
pub const DESCRIBE_SCHEMA_VERSION: &str = "1.0.0";

pub const CAPABILITY_READ: &str = "store.read";
pub const CAPABILITY_WRITE: &str = "store.write";
pub const CAPABILITY_SERVE: &str = "store.serve";

/// A cap on one `store.put`, so a caller cannot exhaust memory through the control plane.
/// Larger objects are a streaming interface this daemon does not have yet.
pub const MAX_INLINE_BYTES: usize = 32 * 1024 * 1024;

pub struct StoreService {
    store: Store,
    perm_socket: PathBuf,
}

#[derive(Debug, Deserialize)]
struct PutParams {
    /// Base64. Inline because the control plane is newline-delimited JSON; see
    /// [`MAX_INLINE_BYTES`].
    data: String,
    #[serde(default)]
    visibility: Visibility,
}

#[derive(Debug, Deserialize)]
struct IdParams {
    content_id: String,
}

#[derive(Debug, Deserialize)]
struct ServeParams {
    content_id: String,
    /// Which peer is asking. Recorded in the audit log; it does not affect the decision,
    /// because `Public` and `Replicated` are public to everyone by definition and `Shared`
    /// needs a per-peer authorization this daemon cannot yet make.
    #[serde(default)]
    peer: Option<String>,
}

impl StoreService {
    pub fn new(store: Store, perm_socket: PathBuf) -> Self {
        StoreService { store, perm_socket }
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
        let object = self.store.put_bytes(&bytes, p.visibility).map_err(rpc)?;
        Ok(record(&object))
    }

    fn handle_get(&self, params: Value) -> Result<Value, RpcError> {
        let p: IdParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.get: {e}")))?;
        let object = self
            .store
            .get_object(&Self::parse_id(&p.content_id)?)
            .map_err(rpc)?;
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

    /// The network boundary.
    ///
    /// The label is checked **before** the store is consulted, so that "you may not have
    /// this" and "this is not here" are the same answer and take the same path. A peer
    /// learns nothing about what this node holds by asking.
    fn handle_serve(&self, params: Value) -> Result<Value, RpcError> {
        let p: ServeParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("store.serve: {e}")))?;
        let id = Self::parse_id(&p.content_id)?;

        let permitted = self
            .store
            .get_object(&id)
            .ok()
            .filter(|o| o.visibility.may_leave_the_node_unattended());

        // One refusal for every reason. Absent, private, shared, damaged: all "not
        // available". Anything more specific tells a stranger what this node holds.
        let Some(object) = permitted else {
            return Err(RpcError::invalid_params(format!(
                "{} is not available to peers",
                id.to_hex()
            )));
        };
        let bytes = self
            .store
            .read_object(&object)
            .map_err(|_| RpcError::invalid_params(format!("{} is not available to peers", id.to_hex())))?;

        let mut out = record(&object);
        out["data"] = json!(data_encoding::BASE64.encode(&bytes));
        out["served_to"] = json!(p.peer);
        Ok(out)
    }
}

fn record(o: &Object) -> Value {
    json!({
        "schema_version": DESCRIBE_SCHEMA_VERSION,
        "content_id": o.content_id.to_hex(),
        "size_bytes": o.size_bytes,
        "chunks": o.chunks.len(),
        "visibility": o.visibility.as_str(),
        "chunking": o.chunking,
    })
}

fn rpc(e: StoreError) -> RpcError {
    match e {
        // A caller naming something that is not here, or naming it wrongly.
        StoreError::NotFound(_) => RpcError::invalid_params(e.to_string()),
        // Damage or a wrong key. The node's problem, not the caller's.
        StoreError::Corrupt { .. } | StoreError::Crypt(_) => RpcError::internal(e.to_string()),
        StoreError::Object(_) => RpcError::internal(e.to_string()),
        StoreError::Io { .. } => RpcError::internal(e.to_string()),
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
                    "store.serve",
                    "Hand an object to a peer, if its label permits leaving the node",
                    CAPABILITY_SERVE,
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
            "store.serve" => {
                self.authorize(ctx, CAPABILITY_SERVE)?;
                self.handle_serve(params)
            }
            other => Err(unknown_method(other)),
        }
    }
}

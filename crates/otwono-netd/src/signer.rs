//! Signing for a daemon that does not hold the node key.
//!
//! `otwono-netd` holds the X25519 agreement secret and nothing else. The two Ed25519
//! signatures a Noise handshake needs come from `otwono-idd` over the control plane
//! (ADR-0010):
//!
//! * the **agreement binding** is fetched once, at startup, when this daemon registers its
//!   agreement key with `id.bind_agreement`;
//! * the **session proof** is one `id.sign_session` call per handshake.
//!
//! # What this buys
//!
//! The daemon that parses input from the network cannot read `node.key`. Compromising it
//! costs the node its current sessions and its agreement key — both replaceable. It does
//! not cost the node its name, which is not replaceable: a NodeID can only be succeeded,
//! and every peer that trusted the old one has to be told.
//!
//! # What it costs
//!
//! A handshake now depends on `otwono-idd` *and* `otwono-permd` being up. That is
//! fail-closed, which is the right direction — a node that cannot prove who it is must not
//! pretend — but it is a real new failure mode, and it is why [`BrokeredSigner::bind`]
//! waits for both rather than starting a mesh that cannot authenticate.

use otwono_identity::{
    AgreementBinding, AgreementKey, NodeId, SessionSigner, SignerError, HANDSHAKE_HASH_LEN,
};
use otwono_proto::{code, Client, RpcError};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use zeroize::Zeroizing;

/// How long to wait for `otwono-idd` and `otwono-permd` at startup.
///
/// Both are ordered before this daemon, but systemd "started" is not "listening", and on a
/// cold SBC first boot `otwono-idd` may be blocked on the entropy pool.
pub const STARTUP_WAIT: Duration = Duration::from_secs(30);

/// A [`SessionSigner`] whose Ed25519 half lives in `otwono-idd`.
pub struct BrokeredSigner {
    agreement: AgreementKey,
    node_id: NodeId,
    /// Fetched once at bind time. Static until the signing key rotates, which invalidates
    /// this process's whole view and is handled by restarting rather than patching.
    binding: AgreementBinding,
    /// Where peers may seal to this node (ADR-0019), fetched once at bind time for exactly
    /// the same reason and with the same caveat: `id.rotate` changes the NodeID, so it
    /// invalidates `node_id` and both bindings together and this process restarts.
    ///
    /// Cached rather than fetched per handshake because it goes into every `Hello`, and a
    /// control-plane round trip on every inbound connection is a cost a mesh pays
    /// continuously. `None` on a node whose identity daemon had no sharing key to offer —
    /// a node nothing can be shared with, which still meshes.
    sharing: Option<otwono_identity::SharingBinding>,
    id_socket: PathBuf,
    perm_socket: PathBuf,
    /// Cached `id.sign_session` capability. Re-requested when it expires.
    token: Mutex<Option<String>>,
}

impl std::fmt::Debug for BrokeredSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokeredSigner")
            .field("node_id", &self.node_id.to_text())
            .field("idd", &self.id_socket)
            .finish_non_exhaustive()
    }
}

impl BrokeredSigner {
    /// Register this daemon's agreement key with `otwono-idd` and take the binding.
    ///
    /// This is where the node's *name* enters this process: it comes from the binding the
    /// signing key just made, never from anything on disk here. `otwono-netd` cannot
    /// derive a NodeID, because it cannot see the key a NodeID is the hash of.
    pub fn bind(
        agreement: AgreementKey,
        id_socket: impl AsRef<Path>,
        perm_socket: impl AsRef<Path>,
    ) -> Result<Self, BindError> {
        let id_socket = id_socket.as_ref().to_path_buf();
        let perm_socket = perm_socket.as_ref().to_path_buf();

        let token = request_token(&perm_socket, otwono_idd::CAPABILITY_BIND, STARTUP_WAIT)?;
        let mut idd = connect(&id_socket, STARTUP_WAIT)?;
        let public = agreement.public();
        let value = call_with_token(
            &mut idd,
            "id.bind_agreement",
            json!({ "agreement_public_key": data_encoding::BASE64.encode(&public) }),
            &token,
        )?;
        let binding: AgreementBinding =
            serde_json::from_value(value).map_err(|e| BindError::Malformed(e.to_string()))?;

        // Check what came back rather than trusting it. A binding that names a different
        // agreement key would fail every handshake later, with a confusing error; a
        // binding whose NodeID does not match its key would be a broken idd.
        let verified = binding
            .verify()
            .map_err(|e| BindError::Malformed(e.to_string()))?;
        if verified.agreement_public_key != public {
            return Err(BindError::Malformed(
                "otwono-idd vouched for a different agreement key than the one offered".into(),
            ));
        }

        // Where peers may seal to this node, taken while the socket is already open.
        // Verified here too, and against the NodeID the agreement binding just established:
        // an idd offering a sharing binding for somebody else is broken, and advertising it
        // would tell every peer to seal somebody else's data to a key this node cannot open.
        //
        // Optional. A node whose idd has no sharing key still meshes; it is simply a node
        // nothing can be shared with, and saying so by omission is the honest form.
        let sharing = idd
            .call("id.sharing_binding", json!({}))
            .ok()
            .and_then(|r| r.ok())
            .and_then(|v| serde_json::from_value::<otwono_identity::SharingBinding>(v).ok())
            .filter(|b| b.node_id == verified.node_id && b.verify().is_ok());

        Ok(BrokeredSigner {
            agreement,
            node_id: verified.node_id,
            binding,
            sharing,
            id_socket,
            perm_socket,
            token: Mutex::new(None),
        })
    }

    fn cached_token(&self) -> Option<String> {
        self.token.lock().expect("token lock poisoned").clone()
    }

    fn store_token(&self, token: Option<String>) {
        *self.token.lock().expect("token lock poisoned") = token;
    }

    /// One `id.sign_session` round trip, refreshing the token if the broker rejects it.
    ///
    /// Tokens expire, so the first call after a TTL boundary fails with `UNAUTHORIZED`.
    /// That is a normal event, not an error to surface — but it is retried exactly once,
    /// so a genuine policy denial still fails fast instead of looping.
    fn call_sign_session(&self, hash_b64: &str) -> Result<Vec<u8>, SignerError> {
        let mut token = match self.cached_token() {
            Some(t) => t,
            None => self.fresh_token()?,
        };
        for attempt in 0..2 {
            let mut idd = connect(&self.id_socket, Duration::from_secs(5))
                .map_err(|e| SignerError::Unavailable(e.to_string()))?;
            match call_with_token(
                &mut idd,
                "id.sign_session",
                json!({ "handshake_hash": hash_b64 }),
                &token,
            ) {
                Ok(value) => {
                    self.store_token(Some(token));
                    let sig = value.get("signature").and_then(|s| s.as_str()).ok_or_else(|| {
                        SignerError::Unavailable("id.sign_session returned no signature".into())
                    })?;
                    return data_encoding::BASE64
                        .decode(sig.as_bytes())
                        .map_err(|e| SignerError::Unavailable(format!("signature is not base64: {e}")));
                }
                Err(BindError::Refused(e)) if e.code == code::UNAUTHORIZED && attempt == 0 => {
                    self.store_token(None);
                    token = self.fresh_token()?;
                }
                Err(e) => return Err(SignerError::Unavailable(e.to_string())),
            }
        }
        Err(SignerError::Unavailable(
            "otwono-permd kept rejecting a freshly issued id.sign_session token".into(),
        ))
    }

    fn fresh_token(&self) -> Result<String, SignerError> {
        request_token(
            &self.perm_socket,
            otwono_idd::CAPABILITY_SIGN_SESSION,
            Duration::from_secs(5),
        )
        .map_err(|e| SignerError::Unavailable(e.to_string()))
    }
}

impl SessionSigner for BrokeredSigner {
    fn node_id(&self) -> NodeId {
        self.node_id
    }

    fn agreement_secret(&self) -> Zeroizing<[u8; 32]> {
        self.agreement.secret_bytes()
    }

    fn agreement_binding(&self) -> Result<AgreementBinding, SignerError> {
        Ok(self.binding.clone())
    }

    fn sharing_binding(&self) -> Result<otwono_identity::SharingBinding, SignerError> {
        self.sharing.clone().ok_or_else(|| {
            SignerError::Unavailable(
                "the identity daemon offered no sharing key when this daemon started".into(),
            )
        })
    }

    fn sign_session(&self, handshake_hash: &[u8]) -> Result<[u8; 64], SignerError> {
        // Check locally too. The daemon should not spend a control-plane round trip, or an
        // audit record, discovering that snow handed it something the wrong size.
        if handshake_hash.len() != HANDSHAKE_HASH_LEN {
            return Err(SignerError::BadHandshakeHash(handshake_hash.len()));
        }
        let signature = self.call_sign_session(&data_encoding::BASE64.encode(handshake_hash))?;
        signature
            .as_slice()
            .try_into()
            .map_err(|_| SignerError::Unavailable("a session signature is 64 bytes".into()))
    }
}

fn connect(path: &Path, wait: Duration) -> Result<Client, BindError> {
    Client::connect_waiting(path, wait)
        .map_err(|e| BindError::Unreachable(format!("{}: {e}", path.display())))
}

fn call_with_token(
    client: &mut Client,
    method: &str,
    params: serde_json::Value,
    token: &str,
) -> Result<serde_json::Value, BindError> {
    client
        .call_with_capability(method, params, token)
        .map_err(|e| BindError::Unreachable(format!("{method}: {e}")))?
        .map_err(BindError::Refused)
}

/// Ask the broker for a capability, waiting for it to come up.
fn request_token(perm_socket: &Path, action: &str, wait: Duration) -> Result<String, BindError> {
    let mut broker = connect(perm_socket, wait)?;
    let value = broker
        .call(
            "perm.request",
            json!({
                "action": action,
                "reason": "otwono-netd authenticates peers without holding the node key",
            }),
        )
        .map_err(|e| BindError::Unreachable(format!("perm.request: {e}")))?
        .map_err(BindError::Refused)?;
    value
        .get("token")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| BindError::Malformed("perm.request returned no token".into()))
}

#[derive(Debug)]
pub enum BindError {
    Unreachable(String),
    Refused(RpcError),
    Malformed(String),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::Unreachable(e) => write!(f, "cannot reach the control plane: {e}"),
            BindError::Refused(e) if e.code == code::FORBIDDEN => write!(
                f,
                "policy refuses this daemon the capability it needs to authenticate peers \
                 ({}); without it the node can discover peers but never prove who it is",
                e.message
            ),
            BindError::Refused(e) => write!(f, "{}", e.message),
            BindError::Malformed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BindError {}

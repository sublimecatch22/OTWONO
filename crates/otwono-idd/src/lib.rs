//! OTWONO identity daemon.
//!
//! Publishes the node's public identity and signs on behalf of authorized callers, so that
//! no other process needs to read the private key. Concentrating key use in one small
//! daemon is the point: the number of processes that can touch the node key is the number
//! of processes whose compromise costs the node its identity.
//!
//! # The only holder of the signing key
//!
//! Since ADR-0010 this is literally true: `otwono-idd` reads `node.key` and nothing else
//! does. `otwono-netd` holds only the X25519 agreement key and calls `id.sign_session`
//! for the one Ed25519 signature a Noise handshake needs. Neither half is enough on its
//! own — a caller with `id.sign_session` but no agreement key cannot complete a handshake,
//! because the binding would not match the static key it authenticated with.
//!
//! # Which methods are open
//!
//! `id.node`, `id.fingerprint`, `id.agreement_binding` and `id.succession` are
//! unauthenticated on the local socket. Everything they return is already published to any
//! peer that connects, so guarding them would be theatre. `id.sign`, `id.sign_session`,
//! `id.bind_agreement` and `id.rotate` are brokered, because they *use* the key rather
//! than describing it.

#![forbid(unsafe_code)]

use otwono_identity::{SharingKey, SigningIdentity, SigningKeystore};
use otwono_proto::{
    unknown_method, CallContext, Client, MethodDescription, RpcError, Service, ServiceDescription,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub const SERVICE_NAME: &str = "otwono-idd";
pub const DESCRIBE_SCHEMA_VERSION: &str = "1.0.0";
pub const CAPABILITY_SIGN: &str = "id.sign";
pub const CAPABILITY_SIGN_SESSION: &str = "id.sign_session";
pub const CAPABILITY_BIND: &str = "id.bind_agreement";
pub const CAPABILITY_ROTATE: &str = "id.rotate";
pub const CAPABILITY_UNWRAP: &str = "id.unwrap_shared";

/// Re-exported: the constant lives in `otwono-identity` so that libraries which verify
/// these signatures do not have to depend on this daemon to learn the prefix.
pub use otwono_identity::APPLICATION_DOMAIN;

pub struct IdentityService {
    keystore: SigningKeystore,
    identity: Mutex<Arc<SigningIdentity>>,
    /// The node's sharing key (ADR-0019). Held here rather than in `otwono-netd` so that a
    /// content key never exists in the process that parses input from the network.
    sharing: SharingKey,
    perm_socket: PathBuf,
}

#[derive(Debug, Deserialize)]
struct SignParams {
    /// Base64 payload to sign.
    payload: String,
}

#[derive(Debug, Deserialize)]
struct SignSessionParams {
    /// Base64 Noise handshake hash.
    handshake_hash: String,
}

/// One recipient's copy of a content key, as it appears in a `SHARED` object record.
#[derive(Debug, Deserialize)]
struct UnwrapParams {
    sealed_key: otwono_identity::SealedKey,
}

#[derive(Debug, Deserialize)]
struct BindParams {
    /// Base64 X25519 public key to vouch for.
    agreement_public_key: String,
}

impl IdentityService {
    /// Build the service, vouching for the sharing key as part of doing so.
    ///
    /// The binding is recorded here rather than by whoever parsed the arguments, so that
    /// "this service is running" and "this node's published identity names the key it
    /// would actually unwrap with" cannot come apart. A caller that forgot the step would
    /// otherwise get a daemon that answers `id.sharing_binding` correctly while `node.pub`
    /// on disk says nothing — and only a peer reading the file would ever notice.
    pub fn new(
        keystore: SigningKeystore,
        identity: SigningIdentity,
        sharing: SharingKey,
        perm_socket: PathBuf,
    ) -> Result<Self, otwono_identity::KeystoreError> {
        keystore.bind_sharing(&identity, &sharing.public())?;
        Ok(IdentityService {
            keystore,
            identity: Mutex::new(Arc::new(identity)),
            sharing,
            perm_socket,
        })
    }

    /// The sharing binding this node currently stands behind.
    ///
    /// Derived live from the two keys this daemon holds rather than read back from
    /// `node.key`, so it cannot disagree with the key that would actually do the
    /// unwrapping. What is recorded on disk exists so `node.pub` is right for a peer
    /// reading the file; this is what a peer asking the daemon gets.
    fn sharing_binding(&self) -> otwono_identity::SharingBinding {
        self.current().bind_sharing(&self.sharing.public())
    }

    fn current(&self) -> Arc<SigningIdentity> {
        Arc::clone(&self.identity.lock().expect("identity lock poisoned"))
    }

    /// The binding this node currently stands behind, if any.
    fn binding(&self) -> Result<otwono_identity::AgreementBinding, RpcError> {
        let bound = self
            .keystore
            .bound_agreement_public_key()
            .map_err(|e| RpcError::internal(format!("cannot read the keystore: {e}")))?
            .ok_or_else(|| {
                RpcError::unavailable(
                    "no agreement key is bound to this node yet; otwono-netd binds one at startup",
                )
            })?;
        Ok(self.current().bind_agreement(&bound))
    }

    /// Ask the broker whether this caller may do this. Fail closed if it cannot be reached.
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

    fn handle_sign(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        self.authorize(ctx, CAPABILITY_SIGN)?;
        let p: SignParams =
            serde_json::from_value(params).map_err(|e| RpcError::invalid_params(format!("id.sign: {e}")))?;
        let payload = data_encoding::BASE64
            .decode(p.payload.as_bytes())
            .map_err(|e| RpcError::invalid_params(format!("payload must be base64: {e}")))?;

        let identity = self.current();
        let signature = identity.sign(&domain_separated(&payload));
        Ok(json!({
            "node_id": identity.node_id().to_text(),
            "public_key": data_encoding::BASE64.encode(&identity.public_key_bytes()),
            "domain": String::from_utf8_lossy(APPLICATION_DOMAIN),
            "signature": data_encoding::BASE64.encode(&signature.to_bytes()),
        }))
    }

    /// Sign one Noise handshake hash on behalf of `otwono-netd`.
    ///
    /// This is the call that lets the mesh daemon authenticate without holding the node
    /// key. It is a signing oracle for the session domain, so it is deliberately narrow:
    /// the domain is fixed here, and the payload must be exactly a handshake hash. A
    /// caller cannot steer it into signing anything else.
    fn handle_sign_session(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        self.authorize(ctx, CAPABILITY_SIGN_SESSION)?;
        let p: SignSessionParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("id.sign_session: {e}")))?;
        let hash = data_encoding::BASE64
            .decode(p.handshake_hash.as_bytes())
            .map_err(|e| RpcError::invalid_params(format!("handshake_hash must be base64: {e}")))?;

        let identity = self.current();
        let signature = identity
            .sign_session(&hash)
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        Ok(json!({
            "node_id": identity.node_id().to_text(),
            "signature": data_encoding::BASE64.encode(&signature),
        }))
    }

    /// Vouch for an agreement key and remember that we did.
    ///
    /// Idempotent: re-binding the same key rewrites the same record and returns the same
    /// signature, which is what a restarting `otwono-netd` does on every boot.
    fn handle_bind_agreement(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        self.authorize(ctx, CAPABILITY_BIND)?;
        let p: BindParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("id.bind_agreement: {e}")))?;
        let key: [u8; 32] = data_encoding::BASE64
            .decode(p.agreement_public_key.as_bytes())
            .map_err(|e| RpcError::invalid_params(format!("agreement_public_key must be base64: {e}")))?
            .as_slice()
            .try_into()
            .map_err(|_| RpcError::invalid_params("an X25519 public key is 32 bytes"))?;

        let identity = self.current();
        self.keystore
            .bind_agreement(&identity, &key)
            .map_err(|e| RpcError::internal(format!("cannot record the binding: {e}")))?;
        serde_json::to_value(identity.bind_agreement(&key)).map_err(|e| RpcError::internal(e.to_string()))
    }

    /// Open a content key that was sealed to this node (ADR-0019).
    ///
    /// The sharing secret never leaves this daemon, and neither does anything derived from
    /// it except the one 32-byte content key the caller asked about. `otwono-stored` calls
    /// this, holds the key long enough to decrypt, and drops it.
    ///
    /// A copy addressed to another node is refused by name before any key agreement is
    /// attempted. It could not open anyway — the recipient is bound as additional data and
    /// the derivation includes the recipient's public key — but "this is not your copy" is
    /// a different fact from "this did not decrypt", and a caller that cannot tell them
    /// apart will retry forever against a copy that can never work.
    fn handle_unwrap_shared(&self, ctx: &CallContext, params: Value) -> Result<Value, RpcError> {
        self.authorize(ctx, CAPABILITY_UNWRAP)?;
        let p: UnwrapParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("id.unwrap_shared: {e}")))?;

        let me = self.current().node_id().to_text();
        if p.sealed_key.recipient != me {
            return Err(RpcError::invalid_params(format!(
                "that copy is sealed to {}, and this node is {me}",
                p.sealed_key.recipient
            )));
        }
        let content_key = self.sharing.open(&p.sealed_key).map_err(|e| {
            // Deliberately not the underlying error: whether a seal failed on the tag or
            // on the encoding is not something a caller needs, and AEAD failures are the
            // classic place to leak a distinction worth an oracle.
            RpcError::invalid_params(format!(
                "this sealed key does not open with this node's sharing key ({e})"
            ))
        })?;
        Ok(json!({
            "content_key": data_encoding::BASE64.encode(content_key.as_ref()),
        }))
    }

    fn handle_rotate(&self, ctx: &CallContext) -> Result<Value, RpcError> {
        self.authorize(ctx, CAPABILITY_ROTATE)?;
        let (new, record) = self
            .keystore
            .rotate(otwono_identity::now_unix_ms())
            .map_err(|e| RpcError::internal(format!("rotation failed: {e}")))?;
        // Rotation drops both bindings. The sharing one this daemon can restore itself,
        // because it holds both halves; the agreement key lives in otwono-netd, which has
        // to be told. Re-binding here rather than waiting for a restart means a rotated
        // node does not silently stop being shareable-with.
        self.keystore
            .bind_sharing(&new, &self.sharing.public())
            .map_err(|e| RpcError::internal(format!("cannot re-bind the sharing key: {e}")))?;
        let response = json!({
            "node_id": new.node_id().to_text(),
            "fingerprint": new.node_id().fingerprint(),
            "succession": record,
            // Saying so is what tells otwono-netd it must re-bind before it can handshake
            // again. Nothing republishes node.pub until it does.
            "agreement_rebind_required": true,
            "sharing_rebound": true,
        });
        *self.identity.lock().expect("identity lock poisoned") = Arc::new(new);
        Ok(response)
    }
}

pub use otwono_identity::domain_separated;

impl Service for IdentityService {
    fn describe(&self) -> ServiceDescription {
        ServiceDescription {
            schema_version: DESCRIBE_SCHEMA_VERSION.to_string(),
            service: SERVICE_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            methods: vec![
                MethodDescription::open("id.node", "This node's public identity"),
                MethodDescription::open("id.fingerprint", "The human-checkable fingerprint"),
                MethodDescription::open(
                    "id.public_key",
                    "This node's Ed25519 public key, for verifying records it signed",
                ),
                MethodDescription::open(
                    "id.agreement_binding",
                    "The signed binding between this NodeID and its X25519 agreement key",
                ),
                MethodDescription::open(
                    "id.sharing_binding",
                    "The signed binding between this NodeID and the X25519 key to seal to",
                ),
                MethodDescription::open("id.succession", "Signed key-rotation history"),
                MethodDescription::guarded(
                    "id.sign",
                    "Sign a base64 payload with the node key, under the application domain",
                    CAPABILITY_SIGN,
                ),
                MethodDescription::guarded(
                    "id.sign_session",
                    "Sign one Noise handshake hash, so otwono-netd need not hold the node key",
                    CAPABILITY_SIGN_SESSION,
                ),
                MethodDescription::guarded(
                    "id.bind_agreement",
                    "Vouch for an X25519 agreement key held by another daemon",
                    CAPABILITY_BIND,
                ),
                MethodDescription::guarded(
                    "id.unwrap_shared",
                    "Open a content key sealed to this node, without the sharing secret leaving",
                    CAPABILITY_UNWRAP,
                ),
                MethodDescription::guarded(
                    "id.rotate",
                    "Generate a new node identity, endorsed by the outgoing key",
                    CAPABILITY_ROTATE,
                ),
            ],
        }
    }

    fn call(&self, ctx: &CallContext, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "id.node" => {
                let bound = self
                    .keystore
                    .bound_agreement_public_key()
                    .map_err(|e| RpcError::internal(format!("cannot read the keystore: {e}")))?
                    .ok_or_else(|| {
                        RpcError::unavailable(
                            "this node has no agreement key bound yet, so it has no publishable \
                             identity; otwono-netd binds one at startup",
                        )
                    })?;
                let published = self
                    .current()
                    .to_public(&bound)
                    .with_sharing_binding(self.sharing_binding());
                serde_json::to_value(published).map_err(|e| RpcError::internal(e.to_string()))
            }
            "id.fingerprint" => {
                let identity = self.current();
                Ok(json!({
                    "node_id": identity.node_id().to_text(),
                    "fingerprint": identity.node_id().fingerprint(),
                }))
            }
            // The signing key's public half, and nothing bundled with it.
            //
            // `id.node` also carries it, and is the wrong method to reach for: it builds a
            // *publishable* identity and so needs an agreement key bound, which only
            // `otwono-netd` does at startup. A node with no network daemon still has a
            // signing key and still has records of its own to verify — a wiki page's chain,
            // for one — and coupling that to the mesh coming up would make local work
            // depend on the network, which §4.1 of DISTRIBUTED-SERVICES.md refuses.
            //
            // Open, like the fingerprint. A public key is public: it travels in every
            // handshake and in every `id.node` reply, and guarding it would protect nothing
            // while making a node unable to check its own writing.
            "id.public_key" => {
                let identity = self.current();
                Ok(json!({
                    "node_id": identity.node_id().to_text(),
                    "public_key": data_encoding::BASE64.encode(&identity.public_key_bytes()),
                }))
            }
            "id.agreement_binding" => {
                serde_json::to_value(self.binding()?).map_err(|e| RpcError::internal(e.to_string()))
            }
            "id.sharing_binding" => {
                serde_json::to_value(self.sharing_binding()).map_err(|e| RpcError::internal(e.to_string()))
            }
            "id.succession" => {
                let records = self
                    .keystore
                    .succession_records()
                    .map_err(|e| RpcError::internal(format!("cannot read succession: {e}")))?;
                serde_json::to_value(json!({ "records": records }))
                    .map_err(|e| RpcError::internal(e.to_string()))
            }
            "id.sign" => self.handle_sign(ctx, params),
            "id.sign_session" => self.handle_sign_session(ctx, params),
            "id.bind_agreement" => self.handle_bind_agreement(ctx, params),
            "id.unwrap_shared" => self.handle_unwrap_shared(ctx, params),
            "id.rotate" => self.handle_rotate(ctx),
            other => Err(unknown_method(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otwono_identity::{verify_signature, NodeIdentity};

    #[test]
    fn application_signatures_cannot_masquerade_as_protocol_messages() {
        // The signing-oracle attack: ask id.sign for a signature over bytes that are a
        // valid agreement binding, then present the result as the node's own binding.
        // Domain separation is what stops it.
        let identity = NodeIdentity::generate().unwrap();
        let agreement = identity.agreement_public().to_bytes();

        // Exactly the bytes an agreement binding signs, requested through id.sign.
        let mut forged_payload = b"otwono-agreement-binding-v1:".to_vec();
        forged_payload.extend_from_slice(&agreement);
        let oracle_signature = identity.sign(&domain_separated(&forged_payload));

        // As an application signature it verifies, because that is what it is.
        assert!(verify_signature(
            &identity.public_key_bytes(),
            &domain_separated(&forged_payload),
            &oracle_signature.to_bytes()
        )
        .is_ok());

        // As a binding signature it does not, which is the property that matters.
        assert!(verify_signature(
            &identity.public_key_bytes(),
            &forged_payload,
            &oracle_signature.to_bytes()
        )
        .is_err());
    }

    #[test]
    fn the_application_domain_is_distinct_from_every_internal_one() {
        let domain = String::from_utf8_lossy(APPLICATION_DOMAIN).to_string();
        for internal in [
            "otwono-agreement-binding-v1:",
            "otwono-sharing-binding-v1:",
            "otwono-succession-v1:",
            "otwono-session-v1:",
            "otwono-shared-key-seal-v1",
        ] {
            assert_ne!(domain, internal);
            assert!(
                !internal.starts_with(&domain),
                "{internal} must not extend the app domain"
            );
            assert!(
                !domain.starts_with(internal),
                "the app domain must not extend {internal}"
            );
        }
    }

    #[test]
    fn the_signing_oracle_cannot_forge_a_sharing_binding() {
        // The same attack as above aimed at ADR-0019: a caller with id.sign asks for a
        // signature over the bytes a sharing binding signs, then presents the result as
        // this node's binding — which would name a key the attacker holds as the one to
        // seal to. Domain separation is the only thing standing in the way.
        let identity = NodeIdentity::generate().unwrap();
        let attacker_key = [9u8; 32];
        let forged_payload = otwono_identity::sharing_binding_message(&attacker_key);
        let oracle_signature = identity.sign(&domain_separated(&forged_payload));

        let forged = otwono_identity::SharingBinding {
            node_id: *identity.node_id(),
            public_key: data_encoding::BASE64.encode(&identity.public_key_bytes()),
            sharing_public_key: data_encoding::BASE64.encode(&attacker_key),
            signature: data_encoding::BASE64.encode(&oracle_signature.to_bytes()),
        };
        assert!(
            forged.verify().is_err(),
            "an application signature satisfied a sharing binding"
        );
    }

    #[test]
    fn domain_separation_preserves_the_payload() {
        assert_eq!(&domain_separated(b"hi")[APPLICATION_DOMAIN.len()..], b"hi");
    }
}

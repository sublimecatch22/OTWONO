//! OTWONO identity daemon.
//!
//! Publishes the node's public identity and signs on behalf of authorized callers, so that
//! no other process needs to read the private key. Concentrating key use in one small
//! daemon is the point: the number of processes that can touch the node key is the number
//! of processes whose compromise costs the node its identity.
//!
//! # Which methods are open
//!
//! `id.node`, `id.fingerprint`, `id.agreement_binding` and `id.succession` are
//! unauthenticated on the local socket. Everything they return is already published to any
//! peer that connects, so guarding them would be theatre. `id.sign` and `id.rotate` are
//! brokered, because they *use* the key rather than describing it.

#![forbid(unsafe_code)]

use otwono_identity::{Keystore, NodeIdentity};
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
pub const CAPABILITY_ROTATE: &str = "id.rotate";

/// Domain prefix for anything signed on a caller's behalf.
///
/// Without this, `id.sign` would be a signing oracle: a caller could ask for a signature
/// over bytes that happen to be a valid agreement binding or succession record and use the
/// result to impersonate the node's own protocol messages. Every internal message type has
/// its own distinct prefix, so a signature made here can never be replayed as one of them.
pub const APPLICATION_DOMAIN: &[u8] = b"otwono-application-v1:";

pub struct IdentityService {
    keystore: Keystore,
    identity: Mutex<Arc<NodeIdentity>>,
    perm_socket: PathBuf,
}

#[derive(Debug, Deserialize)]
struct SignParams {
    /// Base64 payload to sign.
    payload: String,
}

impl IdentityService {
    pub fn new(keystore: Keystore, identity: NodeIdentity, perm_socket: PathBuf) -> Self {
        IdentityService {
            keystore,
            identity: Mutex::new(Arc::new(identity)),
            perm_socket,
        }
    }

    fn current(&self) -> Arc<NodeIdentity> {
        Arc::clone(&self.identity.lock().expect("identity lock poisoned"))
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

    fn handle_rotate(&self, ctx: &CallContext) -> Result<Value, RpcError> {
        self.authorize(ctx, CAPABILITY_ROTATE)?;
        let (new, record) = self
            .keystore
            .rotate(otwono_identity::now_unix_ms())
            .map_err(|e| RpcError::internal(format!("rotation failed: {e}")))?;
        let response = json!({
            "node_id": new.node_id().to_text(),
            "fingerprint": new.node_id().fingerprint(),
            "succession": record,
        });
        *self.identity.lock().expect("identity lock poisoned") = Arc::new(new);
        Ok(response)
    }
}

/// Prefix a caller's payload so its signature cannot be reused as a protocol message.
pub fn domain_separated(payload: &[u8]) -> Vec<u8> {
    let mut m = APPLICATION_DOMAIN.to_vec();
    m.extend_from_slice(payload);
    m
}

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
                    "id.agreement_binding",
                    "The signed binding between this NodeID and its X25519 agreement key",
                ),
                MethodDescription::open("id.succession", "Signed key-rotation history"),
                MethodDescription::guarded(
                    "id.sign",
                    "Sign a base64 payload with the node key, under the application domain",
                    CAPABILITY_SIGN,
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
            "id.node" => serde_json::to_value(self.current().to_public())
                .map_err(|e| RpcError::internal(e.to_string())),
            "id.fingerprint" => {
                let identity = self.current();
                Ok(json!({
                    "node_id": identity.node_id().to_text(),
                    "fingerprint": identity.node_id().fingerprint(),
                }))
            }
            "id.agreement_binding" => serde_json::to_value(self.current().agreement_binding())
                .map_err(|e| RpcError::internal(e.to_string())),
            "id.succession" => {
                let records = self
                    .keystore
                    .succession_records()
                    .map_err(|e| RpcError::internal(format!("cannot read succession: {e}")))?;
                serde_json::to_value(json!({ "records": records }))
                    .map_err(|e| RpcError::internal(e.to_string()))
            }
            "id.sign" => self.handle_sign(ctx, params),
            "id.rotate" => self.handle_rotate(ctx),
            other => Err(unknown_method(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otwono_identity::verify_signature;

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
        for internal in ["otwono-agreement-binding-v1:", "otwono-succession-v1:"] {
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
    fn domain_separation_preserves_the_payload() {
        assert_eq!(&domain_separated(b"hi")[APPLICATION_DOMAIN.len()..], b"hi");
    }
}

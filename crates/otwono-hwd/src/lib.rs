//! OTWONO hardware daemon.
//!
//! Publishes the capability profile on the Local Control Plane. This is the first
//! subsystem to sit behind the permission broker, and it is deliberately the simplest one:
//! it reads hardware and returns JSON. If the authorization path is wrong here, it is
//! wrong everywhere, so it is worth getting right on something with no other complexity.
//!
//! `hwd` does not decide anything about authorization itself. It asks `permd`, over the
//! same control plane, whether the presented token authorizes the action for this caller.
//! Keeping the decision in one process is what makes the policy auditable — the
//! alternative, every service interpreting policy for itself, produces exactly the
//! inconsistency the architecture forbids (CLAUDE.md Section 2.6).

#![forbid(unsafe_code)]

use otwono_capability::{classify_with_overrides, CapabilityOverrides};
use otwono_hal::SystemProbe;
use otwono_proto::{
    unknown_method, CallContext, Client, MethodDescription, RpcError, Service, ServiceDescription,
};
use serde_json::{json, Value};
use std::path::PathBuf;

pub const SERVICE_NAME: &str = "otwono-hwd";
pub const DESCRIBE_SCHEMA_VERSION: &str = "1.0.0";
/// Capability a caller must hold to read the profile.
pub const CAPABILITY_HW_READ: &str = "hw.read";

pub struct HwService {
    /// Probe root. `/` in production, a fixture directory in tests.
    root: PathBuf,
    /// Where the permission broker listens.
    perm_socket: PathBuf,
    overrides: CapabilityOverrides,
}

impl HwService {
    pub fn new(root: PathBuf, perm_socket: PathBuf, overrides: CapabilityOverrides) -> Self {
        HwService {
            root,
            perm_socket,
            overrides,
        }
    }

    fn profile(&self) -> Result<Value, RpcError> {
        let report = SystemProbe::from_root(&self.root).probe();
        let profile = classify_with_overrides(&report, &self.overrides);
        serde_json::to_value(profile)
            .map_err(|e| RpcError::internal(format!("cannot serialise the profile: {e}")))
    }

    /// Ask the broker whether this token authorizes this action for this caller.
    ///
    /// A fresh connection per check. At control-plane rates that costs microseconds, and
    /// it avoids sharing a mutable client across worker threads. If profiling ever shows
    /// this matters, a pool is the fix — not caching the decision, which would let a
    /// revoked token keep working.
    fn authorize(&self, ctx: &CallContext, action: &str) -> Result<(), RpcError> {
        let token = ctx.capability.as_deref().ok_or_else(|| {
            RpcError::unauthorized(format!(
                "{action} requires a capability token; request one from otwono-permd"
            ))
        })?;

        let mut client = Client::connect(&self.perm_socket).map_err(|e| {
            // Fail closed: if the broker is unreachable we refuse, we do not assume yes.
            RpcError::unavailable(format!(
                "cannot reach the permission broker at {}: {e}",
                self.perm_socket.display()
            ))
        })?;

        let response = client
            .call(
                "perm.verify",
                json!({
                    "token": token,
                    "action": action,
                    "subject": ctx.peer.subject(),
                }),
            )
            .map_err(|e| RpcError::unavailable(format!("permission broker call failed: {e}")))?;

        response.map(|_| ())
    }
}

impl Service for HwService {
    fn describe(&self) -> ServiceDescription {
        ServiceDescription {
            schema_version: DESCRIBE_SCHEMA_VERSION.to_string(),
            service: SERVICE_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            methods: vec![
                MethodDescription::guarded(
                    "hw.profile",
                    "The full capability profile: hardware report, axes, tier and feature gates",
                    CAPABILITY_HW_READ,
                ),
                MethodDescription::guarded(
                    "hw.tier",
                    "Just the tier identifier and its limiting axis",
                    CAPABILITY_HW_READ,
                ),
            ],
        }
    }

    fn call(&self, ctx: &CallContext, method: &str, _params: Value) -> Result<Value, RpcError> {
        match method {
            "hw.profile" => {
                self.authorize(ctx, CAPABILITY_HW_READ)?;
                self.profile()
            }
            "hw.tier" => {
                self.authorize(ctx, CAPABILITY_HW_READ)?;
                let profile = self.profile()?;
                Ok(json!({
                    "tier": profile.get("tier").cloned().unwrap_or(Value::Null),
                    "limiting_factor": profile.get("limiting_factor").cloned().unwrap_or(Value::Null),
                }))
            }
            other => Err(unknown_method(other)),
        }
    }
}

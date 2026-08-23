//! OTWONO AI daemon.
//!
//! Answers three questions on the control plane: what can this node do, what models does
//! it have, and would this model load. It does **not** run inference — see below.
//!
//! # STATUS
//!
//! `ai.capabilities`, `ai.models.list` and `ai.admit` are implemented and tested, as is
//! model signature verification against a configured publisher trust store, and the
//! out-of-process backend supervisor the engine will eventually run behind.
//! **`ai.infer` is not implemented.** No inference engine is linked into this build, and
//! the method says so with `NoBackendAvailable` rather than returning a plausible-looking
//! answer. That is deliberate: a stubbed `ai.infer` would be a mock on the default code
//! path of a shipped binary, which CLAUDE.md §2.2 forbids, and every caller built against
//! it would be built against a lie.
//!
//! # Why admission before inference
//!
//! `docs/ai/AI-RUNTIME.md` §4: the common failure of local AI on small hardware is a
//! confident load followed by the OOM killer. That decision is pure logic over a manifest
//! and a capability profile, so it can be got right — and tested against every fixture
//! machine — before any engine exists. Wiring an engine in first would mean the refusal
//! path is whatever the engine happens to do when it runs out of memory.

#![forbid(unsafe_code)]

use otwono_ai::{
    admission::{largest_admissible_context, MemoryPool},
    admit, installed_backends, AdmissionRequest, Catalog, PublisherTrust,
};
use otwono_capability::CapabilityProfile;
use otwono_proto::{
    unknown_method, CallContext, Client, MethodDescription, RpcError, Service, ServiceDescription,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;

pub const SERVICE_NAME: &str = "otwono-aid";
pub const DESCRIBE_SCHEMA_VERSION: &str = "1.0.0";
pub const CAPABILITY_READ: &str = "ai.read";
pub const CAPABILITY_INFER: &str = "ai.infer";

pub struct AiService {
    catalog: Catalog,
    profile: CapabilityProfile,
    trust: PublisherTrust,
    perm_socket: PathBuf,
}

#[derive(Debug, Deserialize)]
struct AdmitParams {
    model_id: String,
    #[serde(default)]
    context_tokens: Option<u32>,
    #[serde(default)]
    sequences: Option<u32>,
    #[serde(default)]
    allow_unsigned: bool,
}

impl AiService {
    pub fn new(
        catalog: Catalog,
        profile: CapabilityProfile,
        trust: PublisherTrust,
        perm_socket: PathBuf,
    ) -> Self {
        AiService {
            catalog,
            profile,
            trust,
            perm_socket,
        }
    }

    fn authorize(&self, ctx: &CallContext, action: &str) -> Result<(), RpcError> {
        let token = ctx
            .capability
            .as_deref()
            .ok_or_else(|| RpcError::unauthorized(format!("{action} requires a capability token")))?;
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
            .map_err(|e| RpcError::unavailable(format!("broker call failed: {e}")))?
            .map(|_| ())
    }

    /// What this node can do right now. Open: it describes the machine, not its contents.
    fn capabilities(&self) -> Value {
        let backends = installed_backends();
        json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "tier": self.profile.tier.as_str(),
            "limiting_factor": self.profile.limiting_factor,
            "accelerator": self.profile.axes.accelerator.as_str(),
            "installed_backends": backends.iter().map(|b| b.as_str()).collect::<Vec<_>>(),
            "trusted_publishers": self.trust.len(),
            // The honest headline. Anything reading this to decide whether to offer a
            // local assistant needs one boolean, and it must not be optimistic.
            "local_inference_available": !backends.is_empty(),
            "notes": if backends.is_empty() {
                vec!["no inference backend is linked into this build; ai.infer will refuse"]
            } else {
                vec![]
            },
        })
    }

    fn models_list(&self) -> Result<Value, RpcError> {
        let (entries, problems) = self
            .catalog
            .list()
            .map_err(|e| RpcError::internal(format!("cannot read the model catalog: {e}")))?;
        let backends = installed_backends();

        let models: Vec<Value> = entries
            .iter()
            .map(|entry| {
                // Every model is listed, admissible or not, with the reason. A catalog that
                // hides what a machine cannot run leaves the user wondering where it went.
                let verdict = admit(
                    &entry.manifest,
                    &self.profile,
                    &AdmissionRequest::default(),
                    &backends,
                    &self.trust,
                );
                json!({
                    "id": entry.manifest.id,
                    "family": entry.manifest.family,
                    "quantization": entry.manifest.quantization,
                    "size_bytes": entry.manifest.size_bytes,
                    "min_tier": entry.manifest.min_tier.as_str(),
                    "max_context": entry.manifest.max_context,
                    "signed": entry.manifest.is_signed(),
                    "provenance": match entry.manifest.verify_signature(&self.trust) {
                        Ok(otwono_ai::SignatureStatus::Trusted { name, .. }) => json!({
                            "status": "trusted", "publisher": name
                        }),
                        Ok(otwono_ai::SignatureStatus::Unsigned) => json!({ "status": "unsigned" }),
                        Err(otwono_ai::SignatureError::UntrustedPublisher { .. }) => {
                            json!({ "status": "untrusted_publisher" })
                        }
                        // A broken signature is worth surfacing in a listing, not only at
                        // load time: it means the manifest was altered after signing.
                        Err(e) => json!({ "status": "bad_signature", "reason": e.to_string() }),
                    },
                    "weights_present": entry.weights_present,
                    "admissible": verdict.is_ok(),
                    "reason": verdict.as_ref().err().map(|e| e.to_string()),
                })
            })
            .collect();

        Ok(json!({
            "models": models,
            "problems": problems
                .iter()
                .map(|p| json!({ "path": p.path.display().to_string(), "reason": p.reason }))
                .collect::<Vec<_>>(),
        }))
    }

    /// Would this model load? A dry run of the decision, with the numbers behind it.
    fn admit_model(&self, params: Value) -> Result<Value, RpcError> {
        let p: AdmitParams =
            serde_json::from_value(params).map_err(|e| RpcError::invalid_params(format!("ai.admit: {e}")))?;
        let entry = self
            .catalog
            .get(&p.model_id)
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;

        let request = AdmissionRequest {
            context_tokens: p.context_tokens,
            sequences: p.sequences.unwrap_or(1),
            allow_unsigned: p.allow_unsigned,
        };
        let backends = installed_backends();

        match admit(&entry.manifest, &self.profile, &request, &backends, &self.trust) {
            Ok(a) => Ok(json!({
                "admissible": true,
                "model_id": a.model_id,
                "backend": a.selection.backend.as_str(),
                "pool": match a.pool {
                    MemoryPool::SystemRam => "system_ram",
                    MemoryPool::AcceleratorVram => "accelerator_vram",
                },
                "context_tokens": a.context_tokens,
                "sequences": a.sequences,
                "required_bytes": a.required_bytes,
                "budget_bytes": a.budget_bytes,
                "reserve_bytes": a.reserve_bytes,
                "warnings": a.warnings,
            })),
            // A refusal is a normal result, so it is a successful call reporting
            // `admissible: false` — not an RPC error. Callers browsing a catalog should not
            // have to treat "too big for this machine" as an exception.
            Err(e) => Ok(json!({
                "admissible": false,
                "model_id": entry.manifest.id,
                "reason": e.to_string(),
                "largest_admissible_context": largest_admissible_context(
                    &entry.manifest,
                    &self.profile,
                    &request,
                    &backends,
                    &self.trust,
                ),
            })),
        }
    }
}

impl Service for AiService {
    fn describe(&self) -> ServiceDescription {
        ServiceDescription {
            schema_version: DESCRIBE_SCHEMA_VERSION.to_string(),
            service: SERVICE_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            methods: vec![
                MethodDescription::open(
                    "ai.capabilities",
                    "What this node can do: tier, accelerator, and which backends are linked",
                ),
                MethodDescription::guarded(
                    "ai.models.list",
                    "Models in the catalog, each with whether this machine can run it and why not",
                    CAPABILITY_READ,
                ),
                MethodDescription::guarded(
                    "ai.admit",
                    "Dry run: would this model load, at what cost, and if not what would fit",
                    CAPABILITY_READ,
                ),
                MethodDescription::guarded(
                    "ai.infer",
                    "NOT IMPLEMENTED: no inference backend is linked into this build",
                    CAPABILITY_INFER,
                ),
            ],
        }
    }

    fn call(&self, ctx: &CallContext, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "ai.capabilities" => Ok(self.capabilities()),
            "ai.models.list" => {
                self.authorize(ctx, CAPABILITY_READ)?;
                self.models_list()
            }
            "ai.admit" => {
                self.authorize(ctx, CAPABILITY_READ)?;
                self.admit_model(params)
            }
            "ai.infer" => {
                // Authorize first so the refusal is about the missing engine and not a
                // way to probe the method without a capability.
                self.authorize(ctx, CAPABILITY_INFER)?;
                Err(RpcError::unavailable(
                    "ai.infer is not implemented: no inference backend is linked into this build. \
                     Use ai.capabilities to see what this node can do.",
                ))
            }
            other => Err(unknown_method(other)),
        }
    }
}

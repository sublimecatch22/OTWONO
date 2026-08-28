//! OTWONO AI daemon.
//!
//! Answers, on the control plane: what can this node do, what models does it have, would
//! this model load, are its weights still the weights its manifest describes, and — when a
//! backend is installed — what does the model say.
//!
//! # STATUS
//!
//! `ai.capabilities`, `ai.models.list`, `ai.admit` and `ai.infer` are implemented and
//! tested. `ai.infer` runs a real model through a real engine, or refuses; what it never
//! does is return a plausible-looking answer from no model at all.
//!
//! Whether it can run anything depends on the *machine*, not on this build: backends are
//! discovered on disk (`otwono_ai::discovery`), so a node with no engine installed still
//! reports `local_inference_available: false` and refuses with a reason.
//!
//! # Every inference goes through admission control
//!
//! `docs/ai/AI-RUNTIME.md` §4: the common failure of local AI on small hardware is a
//! confident load followed by the OOM killer. So `ai.infer` does not load what `ai.admit`
//! would refuse, and it loads it with exactly the context window admission control charged
//! the memory budget for — not with the engine's own defaults, which know nothing about
//! this node's reserve.
//!
//! That ordering is the reason admission control was built first. Had the engine gone in
//! first, the refusal path would be whatever llama.cpp happens to do when it runs out of
//! memory, which on a Pi is to be killed by the kernel.

#![forbid(unsafe_code)]

pub mod pull;

use otwono_ai::{
    admission::{largest_admissible_context, MemoryPool},
    admit, install, verify_installed, Admission, AdmissionRequest, BackendId, BackendInstall, BackendProcess,
    Catalog, InstallRequest, ModelManifest, Provenance, PublisherTrust,
};
use otwono_capability::CapabilityProfile;
use otwono_proto::message::Request;
use otwono_proto::{
    unknown_method, CallContext, Client, MethodDescription, RpcError, Service, ServiceDescription,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub const SERVICE_NAME: &str = "otwono-aid";
pub const DESCRIBE_SCHEMA_VERSION: &str = "1.0.0";
pub const CAPABILITY_READ: &str = "ai.read";
pub const CAPABILITY_INFER: &str = "ai.infer";
pub const CAPABILITY_ADMIN: &str = "ai.admin";

/// Where a backend adapter creates its engine socket.
pub const DEFAULT_BACKEND_RUNTIME_DIR: &str = "/run/otwono/ai";

/// How long an adapter has to answer `hello`.
///
/// Short on purpose: the adapter says hello *before* loading a model, so this covers
/// process startup only. If it needed to cover a model load it would have to be minutes,
/// and a minutes-long hello timeout cannot tell a slow node from a dead adapter.
const HELLO_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a model may take to load.
const LOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// How long one completion may take.
const INFER_TIMEOUT: Duration = Duration::from_secs(900);

pub struct AiService {
    catalog: Catalog,
    profile: CapabilityProfile,
    trust: PublisherTrust,
    perm_socket: PathBuf,
    /// Backends found on disk at startup.
    ///
    /// Read once rather than on every call: the answer changes only when packages are
    /// installed, and a daemon that re-probed per request would report a half-installed
    /// backend the moment a file appeared, mid-upgrade.
    installs: Vec<BackendInstall>,
    backend_runtime_dir: PathBuf,
    /// Whether a backend adapter can actually confine itself here, measured once at
    /// startup by running one with `--probe`.
    ///
    /// Measured rather than assumed, and measured *by this process*, because the answer
    /// depends on this daemon's own sandbox: the landlock syscalls are not in systemd's
    /// `@system-service` set, so a unit that filters to it gets EPERM and an adapter reads
    /// that as "no Landlock". A probe run from anywhere else — a boot script under a
    /// lighter unit, say — answers a different question and would report confinement that
    /// the real backend does not get. That happened, on a booted node, and is why this
    /// lives here.
    backend_confinement: String,
    /// Let backends run without kernel confinement.
    ///
    /// Off by default, and it has to be reachable from *here* rather than only from the
    /// adapter's command line: on a kernel without Landlock the adapter fails closed
    /// (ADR-0012), and if the daemon could not pass the override there would be no way for
    /// an operator to run inference on such a kernel at all — which is a decision for them,
    /// made in a unit file, not one for us to make by omission.
    allow_unconfined_backends: bool,
    /// Where `otwono-fetchd` listens, when this node is allowed to download at all.
    ///
    /// `None` means `ai.models.pull` is unavailable and says so, which is the shipped
    /// default: a node fetches nothing until an operator configures a source and grants
    /// `net.fetch` (ADR-0014).
    fetcher: Option<pull::Fetcher>,
    /// The one loaded model, if any.
    ///
    /// One at a time, and the mutex serializes inference. That is not a placeholder for
    /// concurrency to be added later: the memory budget admission control computed is for
    /// **one** model, and a second concurrent load would spend it twice. Multiple
    /// simultaneous requests are what a backend's own sequence slots are for.
    session: Mutex<Option<Session>>,
    next_request_id: AtomicI64,
}

/// A running backend adapter with a model loaded.
struct Session {
    process: BackendProcess,
    model_id: String,
    backend: BackendId,
    context_tokens: u32,
    sequences: u32,
}

#[derive(Debug, Deserialize)]
struct InstallParams {
    /// A manifest JSON file on local disk.
    manifest_path: String,
    /// The weights the manifest describes.
    blob_path: String,
    #[serde(default)]
    allow_unsigned: bool,
}

#[derive(Debug, Deserialize)]
struct PullParams {
    /// A source id from `otwono-fetchd`'s allow-list. Never a URL — the caller cannot name
    /// a host, which is the whole point of ADR-0014.
    source: String,
    /// Path suffix of the manifest under that source's prefix.
    manifest_path: String,
    /// Path suffix of the weights.
    blob_path: String,
    #[serde(default)]
    allow_unsigned: bool,
    /// Download a model this node could not load. Off by default: spending an hour of a
    /// rural link on weights that admission control will refuse is a waste the node can
    /// see coming.
    #[serde(default)]
    allow_unadmittable: bool,
}

#[derive(Debug, Deserialize)]
struct VerifyParams {
    model_id: String,
}

#[derive(Debug, Deserialize)]
struct InferParams {
    model_id: String,
    prompt: String,
    /// Upper bound on generated tokens. Required, because an unbounded request occupies
    /// the node's only engine for as long as the model keeps talking.
    max_tokens: u32,
    #[serde(default)]
    context_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    top_p: Option<f32>,
    #[serde(default)]
    top_k: Option<u32>,
    #[serde(default)]
    seed: Option<u64>,
    #[serde(default)]
    stop: Vec<String>,
    #[serde(default)]
    allow_unsigned: bool,
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
        installs: Vec<BackendInstall>,
    ) -> Self {
        let backend_confinement = probe_backend_confinement(&installs);
        AiService {
            catalog,
            profile,
            trust,
            perm_socket,
            installs,
            backend_runtime_dir: PathBuf::from(DEFAULT_BACKEND_RUNTIME_DIR),
            backend_confinement,
            allow_unconfined_backends: false,
            fetcher: None,
            session: Mutex::new(None),
            next_request_id: AtomicI64::new(1),
        }
    }

    /// Point this daemon at a fetch daemon, enabling `ai.models.pull`.
    pub fn with_fetch_socket(mut self, fetch_socket: impl AsRef<Path>) -> Self {
        self.fetcher = Some(pull::Fetcher {
            fetch_socket: fetch_socket.as_ref().to_path_buf(),
            perm_socket: self.perm_socket.clone(),
        });
        self
    }

    /// Override where backend adapters put their engine sockets. For tests, which cannot
    /// write to `/run`.
    pub fn with_backend_runtime_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.backend_runtime_dir = dir.as_ref().to_path_buf();
        self
    }

    /// Permit backends to run unconfined on a kernel that will not enforce Landlock.
    ///
    /// Deliberately a builder call and not a default: turning it on is a decision, and the
    /// daemon logs it at every backend start.
    pub fn with_unconfined_backends(mut self, allow: bool) -> Self {
        self.allow_unconfined_backends = allow;
        self
    }

    fn installed(&self) -> Vec<BackendId> {
        self.installs.iter().map(|i| i.backend).collect()
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
        let backends = self.installed();
        json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "tier": self.profile.tier.as_str(),
            "limiting_factor": self.profile.limiting_factor,
            "accelerator": self.profile.axes.accelerator.as_str(),
            "installed_backends": backends.iter().map(|b| b.as_str()).collect::<Vec<_>>(),
            "trusted_publishers": self.trust.len(),
            // Measured in this process, so it is the answer the real backend will get.
            "backend_confinement": self.backend_confinement,
            // The honest headline. Anything reading this to decide whether to offer a
            // local assistant needs one boolean, and it must not be optimistic.
            "local_inference_available": !backends.is_empty(),
            "notes": if backends.is_empty() {
                vec!["no inference backend is installed on this node; ai.infer will refuse"]
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
        let backends = self.installed();

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
        let backends = self.installed();

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

    /// Put a model into the catalog, having proved it is the model its manifest describes.
    ///
    /// Local paths only. Downloading is a different problem with a different threat model:
    /// it needs network egress, and this daemon runs with `PrivateNetwork=yes` precisely so
    /// that it has none. Fetching therefore belongs in a separate brokered component, and
    /// splitting it out means everything that decides whether to *trust* a model is tested
    /// without a network anywhere near it (`docs/ai/AI-RUNTIME.md` §5).
    fn install_model(&self, params: Value) -> Result<Value, RpcError> {
        let p: InstallParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("ai.models.install: {e}")))?;

        let text = std::fs::read_to_string(&p.manifest_path)
            .map_err(|e| RpcError::invalid_params(format!("cannot read {}: {e}", p.manifest_path)))?;
        let manifest: ModelManifest = serde_json::from_str(&text).map_err(|e| {
            RpcError::invalid_params(format!("{} is not a model manifest: {e}", p.manifest_path))
        })?;

        let installed = install(
            &self.catalog,
            &manifest,
            Path::new(&p.blob_path),
            &self.trust,
            &InstallRequest {
                allow_unsigned: p.allow_unsigned,
            },
        )
        // A refusal here is the caller's problem — a bad digest, an unsigned manifest they
        // did not opt into — not a node fault, so it is invalid_params and carries the
        // reason verbatim.
        .map_err(|e| RpcError::invalid_params(format!("ai.models.install refused: {e}")))?;

        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "model_id": installed.model_id,
            "blake3": installed.blake3,
            "size_bytes": installed.size_bytes,
            "already_present": installed.already_present,
            "provenance": provenance_json(&installed.provenance),
        }))
    }

    /// Download a model and install it.
    ///
    /// The ordering is the design. Each step is cheaper than the one after it, and each can
    /// refuse, so the expensive one only runs when the cheap ones have already agreed:
    ///
    /// 1. Fetch the **manifest** — kilobytes.
    /// 2. Check **provenance**. A manifest signed by nobody we trust is refused here, before
    ///    a single byte of weights moves. `install` already applies this reasoning to
    ///    hashing; applied to downloading it saves an hour rather than a minute.
    /// 3. Check **admission**. Weights this node could never load are not worth a rural
    ///    link's afternoon. Overridable, because downloading for another machine is a real
    ///    thing to want.
    /// 4. Fetch the **weights**, resumably.
    /// 5. **Install**, which re-hashes them against the manifest with the code that was
    ///    already tested. This call adds no new trust decision (ADR-0014).
    /// 6. **Discard the spool copy**, because `install` copies rather than moves.
    fn pull_model(&self, params: Value) -> Result<Value, RpcError> {
        let p: PullParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("ai.models.pull: {e}")))?;
        let fetcher = self.fetcher.as_ref().ok_or_else(|| {
            RpcError::unavailable(
                "this node has no fetch daemon configured, so it cannot download models;                  install from a local path instead",
            )
        })?;

        // 1. The manifest.
        let manifest_file = fetcher
            .fetch(&p.source, &p.manifest_path)
            .map_err(rpc_from_pull)?;
        let text = std::fs::read_to_string(&manifest_file.path)
            .map_err(|e| RpcError::internal(format!("cannot read {}: {e}", manifest_file.path.display())))?;
        let manifest: ModelManifest = serde_json::from_str(&text).map_err(|e| {
            RpcError::invalid_params(format!("{} is not a model manifest: {e}", p.manifest_path))
        })?;
        manifest
            .validate()
            .map_err(|e| RpcError::invalid_params(format!("ai.models.pull refused: {e}")))?;

        // 2. Provenance, before the weights.
        let request = InstallRequest {
            allow_unsigned: p.allow_unsigned,
        };
        let provenance = otwono_ai::check_provenance(&manifest, &self.trust, &request)
            .map_err(|e| RpcError::invalid_params(format!("ai.models.pull refused: {e}")))?;

        // 3. Admission, also before the weights — but a *narrower* question than the one
        // `ai.infer` asks. What matters before spending a download is whether this machine
        // could ever hold the model, not whether it can run it this minute. A node with no
        // engine installed yet is exactly the node that wants to download one, and the
        // engine is opt-in in the build, so the two legitimately arrive in either order.
        // Refusing "no backend installed" here would break setup on a fresh node.
        if !p.allow_unadmittable {
            if let Err(reason) = otwono_ai::fits_this_machine(&manifest, &self.profile) {
                return Err(RpcError::invalid_params(format!(
                    "ai.models.pull refused before downloading: {reason}. \
                     Pass allow_unadmittable to download it anyway."
                )));
            }
        }

        // 4. The weights.
        let blob = fetcher.fetch(&p.source, &p.blob_path).map_err(rpc_from_pull)?;

        // 5. Install, which re-hashes. The fetcher's word is not taken for anything.
        let installed = install(&self.catalog, &manifest, &blob.path, &self.trust, &request)
            .map_err(|e| RpcError::invalid_params(format!("ai.models.pull refused: {e}")))?;

        // 6. Reclaim the spool. A failure here is worth reporting and is not worth undoing
        // a good install for, so it is a warning in the reply rather than an error.
        let reclaimed = fetcher.discard(&p.source, &p.blob_path).is_ok()
            && fetcher.discard(&p.source, &p.manifest_path).is_ok();

        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "model_id": installed.model_id,
            "blake3": installed.blake3,
            "size_bytes": installed.size_bytes,
            "already_present": installed.already_present,
            "bytes_fetched": blob.bytes,
            "fetch_calls": blob.calls,
            "spool_reclaimed": reclaimed,
            "provenance": provenance_json(&provenance),
        }))
    }

    /// Re-hash an installed model's weights.
    ///
    /// A successful call reporting `digest_matches: false` is the interesting answer, and
    /// it is a result rather than an error: a caller auditing a catalog should not have to
    /// handle an exception per corrupt model.
    fn verify_model(&self, params: Value) -> Result<Value, RpcError> {
        let p: VerifyParams = serde_json::from_value(params)
            .map_err(|e| RpcError::invalid_params(format!("ai.models.verify: {e}")))?;
        let v = verify_installed(&self.catalog, &p.model_id)
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "model_id": v.model_id,
            "weights_present": v.weights_present,
            "digest_matches": v.digest_matches,
            "blake3": v.actual,
        }))
    }

    /// Run a prompt through a locally installed model.
    ///
    /// The order of operations is the whole design:
    ///
    /// 1. **Admission control decides**, exactly as `ai.admit` would, including the
    ///    signature check. A model that would be refused as a dry run is refused here too —
    ///    otherwise `ai.admit` would be advice the system itself ignores.
    /// 2. **The weights must be present.** A manifest without its blob is a catalog entry,
    ///    not a model, and "file not found" from inside an engine is a poor way to learn it.
    /// 3. **The engine is started with admission control's numbers**, not its own defaults.
    /// 4. **An already-loaded model is reused.** Reloading per request would dominate the
    ///    wall clock on exactly the hardware this project exists for.
    fn infer(&self, params: Value) -> Result<Value, RpcError> {
        let p: InferParams =
            serde_json::from_value(params).map_err(|e| RpcError::invalid_params(format!("ai.infer: {e}")))?;
        if p.max_tokens == 0 {
            return Err(RpcError::invalid_params("max_tokens must be at least 1"));
        }

        let entry = self
            .catalog
            .get(&p.model_id)
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;

        let request = AdmissionRequest {
            context_tokens: p.context_tokens,
            sequences: 1,
            allow_unsigned: p.allow_unsigned,
        };
        let backends = self.installed();
        let admission =
            admit(&entry.manifest, &self.profile, &request, &backends, &self.trust).map_err(|e| {
                // A refusal carries what would fit, so a caller can retry with a smaller
                // context instead of guessing. `ai.admit` returns the same figure.
                RpcError::unavailable(format!("ai.infer refused: {e}")).with_data(json!({
                    "model_id": entry.manifest.id,
                    "largest_admissible_context": largest_admissible_context(
                        &entry.manifest, &self.profile, &request, &backends, &self.trust,
                    ),
                }))
            })?;

        if !entry.weights_present {
            return Err(RpcError::unavailable(format!(
                "the manifest for {} is in the catalog but its weights are not; \
                 nothing has been downloaded yet",
                entry.manifest.id
            )));
        }
        let weights = self.catalog.blob_path(&entry.manifest.blake3);

        let install = self
            .installs
            .iter()
            .find(|i| i.backend == admission.selection.backend)
            .ok_or_else(|| {
                // Should be unreachable: admission chose from exactly this set. Reported
                // rather than unwrapped, because an unreachable state that panics takes
                // the daemon down with it.
                RpcError::internal(format!(
                    "admission chose {} but it is not installed",
                    admission.selection.backend.as_str()
                ))
            })?
            .clone();

        let mut guard = self
            .session
            .lock()
            .map_err(|_| RpcError::internal("the inference lock was poisoned by an earlier panic"))?;

        self.ensure_loaded(&mut guard, &install, &admission, &weights)?;
        let session = guard.as_mut().expect("ensure_loaded leaves a session");

        let mut infer_params = json!({
            "prompt": p.prompt,
            "max_tokens": p.max_tokens,
        });
        let map = infer_params.as_object_mut().expect("object literal");
        for (key, value) in [
            ("temperature", p.temperature.map(|v| json!(v))),
            ("top_p", p.top_p.map(|v| json!(v))),
            ("top_k", p.top_k.map(|v| json!(v))),
            ("seed", p.seed.map(|v| json!(v))),
        ] {
            if let Some(value) = value {
                map.insert(key.to_string(), value);
            }
        }
        if !p.stop.is_empty() {
            map.insert("stop".to_string(), json!(p.stop));
        }

        let result = self
            .backend_call(session, "backend.infer", infer_params, INFER_TIMEOUT)
            .inspect_err(|_| {
                // A backend that failed mid-inference is not trustworthy for the next
                // request: it may be dead, or holding a half-finished response that would
                // be read as the answer to whatever comes next. Drop it and reload.
                *guard = None;
            })?;

        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "model_id": entry.manifest.id,
            "backend": admission.selection.backend.as_str(),
            "context_tokens": admission.context_tokens,
            "text": result["text"],
            "tokens_predicted": result["tokens_predicted"],
            "tokens_evaluated": result["tokens_evaluated"],
            "stop_reason": result["stop_reason"],
            "prompt_truncated": result["prompt_truncated"],
            "timings": result["timings"],
        }))
    }

    /// Make sure the right model is loaded in the right backend, reusing what is there.
    fn ensure_loaded(
        &self,
        guard: &mut Option<Session>,
        install: &BackendInstall,
        admission: &Admission,
        weights: &Path,
    ) -> Result<(), RpcError> {
        if let Some(session) = guard.as_ref() {
            // Context is part of the identity of a load: the same model at a larger context
            // is a different memory reservation, and reusing the smaller one would silently
            // give the caller less room than admission control granted.
            if session.model_id == admission.model_id
                && session.backend == install.backend
                && session.context_tokens == admission.context_tokens
                && session.sequences == admission.sequences
            {
                return Ok(());
            }
        }
        // Stop the old one before starting the new one: two resident models would need
        // twice the memory that was budgeted.
        *guard = None;

        std::fs::create_dir_all(&self.backend_runtime_dir).map_err(|e| {
            RpcError::internal(format!(
                "cannot create {}: {e}",
                self.backend_runtime_dir.display()
            ))
        })?;

        let mut command = std::process::Command::new(&install.adapter);
        command
            .arg("--engine")
            .arg(&install.engine)
            // The blob store, and nothing above it. This is the boundary the adapter
            // confines itself to with Landlock (ADR-0012), so passing the catalog root
            // instead would hand the engine the manifests as well for no reason.
            .arg("--model-dir")
            .arg(self.catalog.blob_dir())
            .arg("--runtime-dir")
            .arg(&self.backend_runtime_dir);
        if self.allow_unconfined_backends {
            eprintln!(
                "otwono-aid: starting the {} backend unconfined; the operator asked for this",
                install.backend.as_str()
            );
            command.arg("--allow-unconfined");
        }
        let process =
            BackendProcess::spawn(install.backend.as_str(), &mut command, HELLO_TIMEOUT).map_err(|e| {
                RpcError::unavailable(format!(
                    "cannot start the {} backend: {e}",
                    install.backend.as_str()
                ))
            })?;

        let mut session = Session {
            process,
            model_id: admission.model_id.clone(),
            backend: install.backend,
            context_tokens: admission.context_tokens,
            sequences: admission.sequences,
        };
        self.backend_call(
            &mut session,
            "backend.load",
            json!({
                "model_path": weights.display().to_string(),
                "context_tokens": admission.context_tokens,
                "sequences": admission.sequences,
            }),
            LOAD_TIMEOUT,
        )?;
        *guard = Some(session);
        Ok(())
    }

    /// One JSON-RPC round trip to a backend adapter.
    fn backend_call(
        &self,
        session: &mut Session,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let request = serde_json::to_value(Request::new(id, method, params))
            .map_err(|e| RpcError::internal(format!("cannot encode {method}: {e}")))?;
        let response = session.process.request(&request, timeout).map_err(|e| {
            RpcError::unavailable(format!("the {} backend failed: {e}", session.backend.as_str()))
        })?;

        if let Some(error) = response.get("error") {
            let message = error["message"].as_str().unwrap_or("no message");
            // Pass the backend's own words through. It is the only part of the system that
            // knows why a particular GGUF would not load.
            return Err(RpcError::unavailable(format!(
                "the {} backend refused {method}: {message}",
                session.backend.as_str()
            )));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| RpcError::internal(format!("{method} returned neither a result nor an error")))
    }
}

/// Ask an installed adapter whether it can confine itself here.
///
/// One short-lived subprocess at startup. It has to be a subprocess: finding out means
/// applying a Landlock ruleset, which is irreversible, and a daemon that confined itself to
/// answer a status question would have answered it by breaking.
///
/// `"unknown"` when there is no adapter to ask, which is not a failure — a node with no
/// backend has nothing to confine.
fn probe_backend_confinement(installs: &[BackendInstall]) -> String {
    let Some(install) = installs.first() else {
        return "unknown".to_string();
    };
    match std::process::Command::new(&install.adapter)
        .arg("--probe")
        .output()
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|l| l.trim().strip_prefix("landlock="))
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        Err(_) => "unknown".to_string(),
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
                    "ai.models.install",
                    "Install a model from a local manifest and weights, verifying both first",
                    CAPABILITY_ADMIN,
                ),
                MethodDescription::guarded(
                    "ai.models.pull",
                    "Download a model from an allow-listed source and install it",
                    CAPABILITY_ADMIN,
                ),
                MethodDescription::guarded(
                    "ai.models.verify",
                    "Re-hash an installed model's weights and compare them to its manifest",
                    CAPABILITY_READ,
                ),
                MethodDescription::guarded(
                    "ai.infer",
                    "Run a prompt through a locally installed model, subject to admission control",
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
            "ai.models.install" => {
                self.authorize(ctx, CAPABILITY_ADMIN)?;
                self.install_model(params)
            }
            // ai.admin, the same as a local install: what makes this powerful is that it
            // changes what the node will run, and where the bytes came from does not
            // change that. The fetch itself needs net.fetch, which this daemon requests
            // from the broker on the caller's behalf and scoped to the named source.
            "ai.models.pull" => {
                self.authorize(ctx, CAPABILITY_ADMIN)?;
                self.pull_model(params)
            }
            "ai.models.verify" => {
                self.authorize(ctx, CAPABILITY_READ)?;
                self.verify_model(params)
            }
            "ai.infer" => {
                // Authorize before anything else, so that a node with no backend refuses
                // unauthenticated callers on the capability rather than leaking that it
                // has nothing installed.
                self.authorize(ctx, CAPABILITY_INFER)?;
                if self.installs.is_empty() {
                    return Err(RpcError::unavailable(
                        "no inference backend is installed on this node, so ai.infer cannot run \
                         anything. Use ai.capabilities to see what this node can do.",
                    ));
                }
                self.infer(params)
            }
            other => Err(unknown_method(other)),
        }
    }
}

fn provenance_json(p: &Provenance) -> Value {
    match p {
        Provenance::Trusted { publisher } => json!({ "status": "trusted", "publisher": publisher }),
        Provenance::Unsigned => json!({ "status": "unsigned" }),
        Provenance::UntrustedPublisher => json!({ "status": "untrusted_publisher" }),
    }
}

/// A fetch failure is the node's or the network's, not the caller's — except when the
/// caller named something the allow-list does not have, which the fetcher reports as a
/// refusal and which the caller can act on.
fn rpc_from_pull(e: pull::PullError) -> RpcError {
    match e {
        pull::PullError::Refused(m) => RpcError::invalid_params(format!("ai.models.pull: {m}")),
        other => RpcError::unavailable(format!("ai.models.pull: {other}")),
    }
}

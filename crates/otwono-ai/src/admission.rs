//! Admission control: deciding, before loading, whether this machine can afford a model.
//!
//! `docs/ai/AI-RUNTIME.md` §4: "The most common failure mode of local AI on small hardware
//! is a confident load followed by the OOM killer."
//!
//! The rule this module exists to enforce is that **refusal is a normal result**, not an
//! error path. On a Pi Zero, most of the catalog is unaffordable, and saying so with a
//! number and an alternative is the correct behaviour — not something to be worked around
//! by trying anyway and hoping.
//!
//! # The reserve is never zero
//!
//! Whatever is left after the model loads has to keep the desktop, `otwono-netd`, and the
//! user's actual work alive. A node that swaps itself to death answering one question has
//! failed, even though the model "fit". The reserve is tier-dependent, configurable, and
//! floored — see [`Reserve`].

use otwono_capability::{CapabilityProfile, Tier};
use serde::{Deserialize, Serialize};

use crate::backend::{select_backend, BackendId, BackendSelection, SelectionError};
use crate::manifest::{ManifestError, ModelManifest};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// What a caller wants to do, which changes what it costs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionRequest {
    /// Context window to allocate. `None` means the model's maximum, which is the
    /// pessimistic reading and the right default: a session that starts short and grows
    /// must not be admitted on the strength of its first turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
    /// Concurrent sequences sharing the weights.
    #[serde(default = "one")]
    pub sequences: u32,
    /// Permit a model whose manifest carries no signature.
    ///
    /// Explicit because an unsigned model that can call tools is executable content from
    /// an unverified source (`docs/ai/AI-RUNTIME.md` §5).
    #[serde(default)]
    pub allow_unsigned: bool,
}

fn one() -> u32 {
    1
}

impl Default for AdmissionRequest {
    fn default() -> Self {
        AdmissionRequest {
            context_tokens: None,
            sequences: 1,
            allow_unsigned: false,
        }
    }
}

/// Memory held back from any model load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reserve {
    pub bytes: u64,
}

impl Reserve {
    /// The floor, in bytes, below which no configuration may set the reserve.
    ///
    /// A configurable reserve that can be set to zero is not a safety mechanism, it is a
    /// footgun with a dial on it.
    pub const FLOOR: u64 = 256 * MIB;

    /// The default reserve for a tier.
    ///
    /// Larger machines hold back more in absolute terms because they run more: a
    /// workstation has a desktop session, a browser and a compiler to keep alive, while a
    /// T0 board may be running almost nothing else.
    pub fn for_tier(tier: Tier) -> Self {
        let bytes = match tier {
            Tier::T0Micro => 256 * MIB,
            Tier::T1Edge => 512 * MIB,
            Tier::T2Balanced => GIB,
            Tier::T3Capable => 2 * GIB,
            Tier::T4Workstation => 4 * GIB,
        };
        Reserve { bytes }
    }

    /// An operator-chosen reserve, clamped to the floor.
    pub fn custom(bytes: u64) -> Self {
        Reserve {
            bytes: bytes.max(Self::FLOOR),
        }
    }
}

/// A model that may be loaded, with the numbers the decision was made on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admission {
    pub model_id: String,
    pub selection: BackendSelection,
    pub context_tokens: u32,
    pub sequences: u32,
    /// Resident bytes the load is expected to cost.
    pub required_bytes: u64,
    /// Bytes available to models after the reserve.
    pub budget_bytes: u64,
    pub reserve_bytes: u64,
    /// Which memory pool was charged: system RAM, or the accelerator's.
    pub pool: MemoryPool,
    /// Things the caller should know that are not refusals.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPool {
    SystemRam,
    AcceleratorVram,
}

/// Decide whether `manifest` may be loaded on `profile`.
///
/// Pure: no I/O, no hardware access, no model file. Everything it needs is in its
/// arguments, which is what makes the refusal path testable on a CI runner.
pub fn admit(
    manifest: &ModelManifest,
    profile: &CapabilityProfile,
    request: &AdmissionRequest,
    available_backends: &[BackendId],
) -> Result<Admission, AdmissionError> {
    manifest.validate().map_err(AdmissionError::Manifest)?;

    if !request.allow_unsigned && !manifest.is_signed() {
        return Err(AdmissionError::UnsignedModel {
            model_id: manifest.id.clone(),
            tool_capable: manifest.is_unsigned_and_tool_capable(),
        });
    }

    if profile.tier < manifest.min_tier {
        return Err(AdmissionError::ModelTooLargeForTier {
            model_id: manifest.id.clone(),
            node_tier: profile.tier,
            min_tier: manifest.min_tier,
            limiting_factor: profile.limiting_factor.clone(),
        });
    }

    let context_tokens = request.context_tokens.unwrap_or(manifest.max_context);
    if context_tokens > manifest.max_context {
        return Err(AdmissionError::ContextTooLong {
            requested: context_tokens,
            max_context: manifest.max_context,
        });
    }
    let sequences = request.sequences.max(1);

    let selection =
        select_backend(manifest, profile, available_backends).map_err(AdmissionError::NoBackendAvailable)?;

    let required = manifest.footprint.required_bytes(context_tokens, sequences);
    let reserve = Reserve::for_tier(profile.tier);

    let (pool, total) = if selection.offloads_to_accelerator {
        match accelerator_vram(profile) {
            Some(vram) => (MemoryPool::AcceleratorVram, vram),
            // A backend that offloads, on a device whose VRAM we could not read. Charging
            // system RAM would be wrong on a discrete card and right on an iGPU, and
            // guessing either way risks the OOM kill this module exists to prevent.
            None => {
                return Err(AdmissionError::UnknownAcceleratorMemory {
                    backend: selection.backend,
                })
            }
        }
    } else {
        (MemoryPool::SystemRam, profile.hardware.memory.available_bytes)
    };

    let budget = total.saturating_sub(reserve.bytes);
    if required > budget {
        return Err(AdmissionError::InsufficientMemory {
            model_id: manifest.id.clone(),
            pool,
            required_bytes: required,
            budget_bytes: budget,
            reserve_bytes: reserve.bytes,
            context_tokens,
        });
    }

    let mut warnings = Vec::new();
    // Fitting with almost nothing to spare is not the same as fitting. Say so rather than
    // letting the user discover it when the machine starts swapping.
    if budget > 0 && required * 10 > budget * 9 {
        warnings.push(format!(
            "this load uses {}% of the model budget; the node will have little headroom",
            required.saturating_mul(100) / budget.max(1)
        ));
    }
    if manifest.is_unsigned_and_tool_capable() {
        warnings.push(
            "unsigned model with tool access: it is executable content from an unverified \
             source, and tool permissions should be restricted accordingly"
                .to_string(),
        );
    }

    Ok(Admission {
        model_id: manifest.id.clone(),
        selection,
        context_tokens,
        sequences,
        required_bytes: required,
        budget_bytes: budget,
        reserve_bytes: reserve.bytes,
        pool,
        warnings,
    })
}

/// Total VRAM across accelerators that report it.
///
/// `None` when no accelerator reports a figure — `vram_bytes: None` means undetectable,
/// not zero (`otwono-hal`), and the difference matters here.
fn accelerator_vram(profile: &CapabilityProfile) -> Option<u64> {
    let mut total = 0u64;
    let mut any = false;
    for a in &profile.hardware.accelerators {
        if let Some(v) = a.vram_bytes {
            total = total.saturating_add(v);
            any = true;
        }
    }
    any.then_some(total)
}

/// The largest context this model could be admitted at, or `None` if it never fits.
///
/// The point of a refusal is to be actionable: "8k will not fit, 2k will" is useful,
/// "no" is not.
pub fn largest_admissible_context(
    manifest: &ModelManifest,
    profile: &CapabilityProfile,
    request: &AdmissionRequest,
    available_backends: &[BackendId],
) -> Option<u32> {
    let mut best = None;
    // Powers of two from 1k up to the model's maximum: the granularity a caller would
    // actually choose, and it keeps this cheap enough to call inside an error path.
    let mut ctx = 1024u32;
    while ctx <= manifest.max_context {
        let probe = AdmissionRequest {
            context_tokens: Some(ctx),
            ..request.clone()
        };
        if admit(manifest, profile, &probe, available_backends).is_ok() {
            best = Some(ctx);
        } else {
            break;
        }
        ctx = ctx.saturating_mul(2);
    }
    best
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionError {
    Manifest(ManifestError),
    /// The node's tier is below what the model declares it needs.
    ModelTooLargeForTier {
        model_id: String,
        node_tier: Tier,
        min_tier: Tier,
        limiting_factor: Option<String>,
    },
    /// The model would fit the tier but not this machine's actual free memory.
    InsufficientMemory {
        model_id: String,
        pool: MemoryPool,
        required_bytes: u64,
        budget_bytes: u64,
        reserve_bytes: u64,
        context_tokens: u32,
    },
    NoBackendAvailable(SelectionError),
    ContextTooLong {
        requested: u32,
        max_context: u32,
    },
    UnsignedModel {
        model_id: String,
        tool_capable: bool,
    },
    /// An offloading backend on a device whose VRAM could not be read.
    UnknownAcceleratorMemory {
        backend: BackendId,
    },
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmissionError::Manifest(e) => write!(f, "{e}"),
            AdmissionError::ModelTooLargeForTier {
                model_id,
                node_tier,
                min_tier,
                limiting_factor,
            } => {
                write!(
                    f,
                    "{model_id} needs a {} node; this one is {}",
                    min_tier.as_str(),
                    node_tier.as_str()
                )?;
                if let Some(limit) = limiting_factor {
                    write!(f, " (limited by {limit})")?;
                }
                Ok(())
            }
            AdmissionError::InsufficientMemory {
                model_id,
                pool,
                required_bytes,
                budget_bytes,
                reserve_bytes,
                context_tokens,
            } => write!(
                f,
                "{model_id} needs {} at {context_tokens} tokens of context, but only {} of {} is \
                 available after a {} reserve",
                gib(*required_bytes),
                gib(*budget_bytes),
                match pool {
                    MemoryPool::SystemRam => "system memory",
                    MemoryPool::AcceleratorVram => "accelerator memory",
                },
                gib(*reserve_bytes)
            ),
            AdmissionError::NoBackendAvailable(e) => write!(f, "{e}"),
            AdmissionError::ContextTooLong {
                requested,
                max_context,
            } => write!(
                f,
                "{requested} tokens of context requested; this model supports at most {max_context}"
            ),
            AdmissionError::UnsignedModel {
                model_id,
                tool_capable,
            } => {
                write!(f, "{model_id} carries no signature")?;
                if *tool_capable {
                    write!(
                        f,
                        " and can call tools, which makes it executable content from an \
                         unverified source"
                    )?;
                }
                write!(f, "; loading it requires an explicit opt-in")
            }
            AdmissionError::UnknownAcceleratorMemory { backend } => write!(
                f,
                "{} offloads to the accelerator, but this machine reports no usable VRAM figure; \
                 refusing rather than guessing",
                backend.as_str()
            ),
        }
    }
}

impl std::error::Error for AdmissionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::fixtures::*;
    use crate::manifest::{ModelCapability, Signature};
    use otwono_capability::classify;
    use otwono_capability::testing::{report_pi4_4gb, report_pi5_16gb, report_pi_zero, report_workstation};

    fn signed(mut m: ModelManifest) -> ModelManifest {
        m.signature = Some(Signature {
            algorithm: "ed25519".into(),
            public_key: "AA==".into(),
            signature: "AA==".into(),
        });
        m
    }

    fn cpu() -> Vec<BackendId> {
        vec![BackendId::LlamaCppCpu]
    }

    fn pi_zero() -> CapabilityProfile {
        classify(&report_pi_zero())
    }
    fn pi4() -> CapabilityProfile {
        classify(&report_pi4_4gb())
    }
    fn pi5() -> CapabilityProfile {
        classify(&report_pi5_16gb())
    }
    fn workstation() -> CapabilityProfile {
        classify(&report_workstation())
    }

    #[test]
    fn a_small_model_is_admitted_on_a_capable_board() {
        let a = admit(&signed(tiny()), &pi5(), &AdmissionRequest::default(), &cpu())
            .expect("a 1B model must run on a 16 GiB Pi 5");
        assert_eq!(a.pool, MemoryPool::SystemRam);
        assert_eq!(a.selection.backend, BackendId::LlamaCppCpu);
        assert!(a.required_bytes < a.budget_bytes);
    }

    #[test]
    fn the_tier_gate_refuses_before_any_memory_arithmetic() {
        // The exit criterion for Phase 4: ModelTooLargeForTier must be an observed result,
        // not a branch nobody reaches.
        let err = admit(&signed(huge()), &pi_zero(), &AdmissionRequest::default(), &cpu()).unwrap_err();
        let AdmissionError::ModelTooLargeForTier {
            node_tier, min_tier, ..
        } = &err
        else {
            panic!("expected ModelTooLargeForTier, got {err:?}");
        };
        assert_eq!(*min_tier, Tier::T4Workstation);
        assert_eq!(*node_tier, Tier::T0Micro);
        // And it names why this node is small, which is the actionable half.
        assert!(err.to_string().contains("T0_MICRO"), "{err}");
    }

    #[test]
    fn a_model_that_passes_the_tier_gate_can_still_be_refused_on_memory() {
        // Tier is a coarse guide; actual free memory is the binding constraint. A node
        // that trusted the tier alone would OOM here.
        let mut m = signed(tiny());
        m.min_tier = Tier::T0Micro;
        m.footprint.weights_bytes = 4 * GIB;
        let err = admit(&m, &pi4(), &AdmissionRequest::default(), &cpu()).unwrap_err();
        assert!(
            matches!(err, AdmissionError::InsufficientMemory { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("reserve"), "{err}");
    }

    #[test]
    fn the_reserve_is_actually_withheld() {
        // A model sized to exactly the free memory must be refused, or the node loads it
        // and has nothing left to run the daemons that answer the request.
        let profile = pi5();
        let available = profile.hardware.memory.available_bytes;
        let mut m = signed(tiny());
        m.min_tier = Tier::T0Micro;
        m.footprint = crate::manifest::Footprint {
            weights_bytes: available,
            kv_per_1k_ctx_bytes: 0,
            overhead_bytes: 0,
        };
        let err = admit(&m, &profile, &AdmissionRequest::default(), &cpu()).unwrap_err();
        assert!(
            matches!(err, AdmissionError::InsufficientMemory { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn the_reserve_can_never_be_configured_to_zero() {
        assert_eq!(Reserve::custom(0).bytes, Reserve::FLOOR);
        assert_eq!(Reserve::custom(1).bytes, Reserve::FLOOR);
        assert_eq!(Reserve::custom(8 * GIB).bytes, 8 * GIB);
        for tier in Tier::ALL {
            assert!(Reserve::for_tier(tier).bytes >= Reserve::FLOOR);
        }
    }

    #[test]
    fn context_length_alone_can_turn_an_admission_into_a_refusal() {
        // The failure users actually hit: it loaded fine yesterday, then a long
        // conversation killed it.
        let profile = pi4();
        let mut m = signed(tiny());
        m.min_tier = Tier::T0Micro;
        m.max_context = 131_072;
        m.footprint.kv_per_1k_ctx_bytes = 64 * MIB;

        let short = AdmissionRequest {
            context_tokens: Some(2048),
            ..Default::default()
        };
        let long = AdmissionRequest {
            context_tokens: Some(131_072),
            ..Default::default()
        };
        assert!(admit(&m, &profile, &short, &cpu()).is_ok());
        assert!(matches!(
            admit(&m, &profile, &long, &cpu()),
            Err(AdmissionError::InsufficientMemory { .. })
        ));
    }

    #[test]
    fn the_default_context_is_the_models_maximum_not_its_minimum() {
        // Admitting on the strength of a short first turn is how a session gets killed
        // three messages later.
        let m = signed(medium());
        let a = admit(&m, &workstation(), &AdmissionRequest::default(), &cpu()).unwrap();
        assert_eq!(a.context_tokens, m.max_context);
    }

    #[test]
    fn a_refusal_suggests_the_largest_context_that_would_work() {
        let profile = pi4();
        let mut m = signed(tiny());
        m.min_tier = Tier::T0Micro;
        m.max_context = 131_072;
        m.footprint.kv_per_1k_ctx_bytes = 64 * MIB;
        assert!(admit(&m, &profile, &AdmissionRequest::default(), &cpu()).is_err());

        let best = largest_admissible_context(&m, &profile, &AdmissionRequest::default(), &cpu())
            .expect("something must fit");
        assert!(best >= 1024 && best < m.max_context, "best {best}");
        let ok = AdmissionRequest {
            context_tokens: Some(best),
            ..Default::default()
        };
        assert!(
            admit(&m, &profile, &ok, &cpu()).is_ok(),
            "the suggestion must be real"
        );
    }

    #[test]
    fn a_model_that_never_fits_suggests_nothing_rather_than_lying() {
        let mut m = signed(tiny());
        m.min_tier = Tier::T0Micro;
        m.footprint.weights_bytes = 512 * GIB;
        assert_eq!(
            largest_admissible_context(&m, &pi5(), &AdmissionRequest::default(), &cpu()),
            None
        );
    }

    #[test]
    fn asking_for_more_context_than_the_model_has_is_its_own_error() {
        let m = signed(tiny());
        let req = AdmissionRequest {
            context_tokens: Some(m.max_context + 1),
            ..Default::default()
        };
        assert!(matches!(
            admit(&m, &workstation(), &req, &cpu()),
            Err(AdmissionError::ContextTooLong { .. })
        ));
    }

    #[test]
    fn an_unsigned_model_is_refused_until_the_caller_opts_in() {
        let m = tiny(); // unsigned
        let err = admit(&m, &pi5(), &AdmissionRequest::default(), &cpu()).unwrap_err();
        assert!(matches!(err, AdmissionError::UnsignedModel { .. }), "{err:?}");

        let opt_in = AdmissionRequest {
            allow_unsigned: true,
            ..Default::default()
        };
        assert!(admit(&m, &pi5(), &opt_in, &cpu()).is_ok());
    }

    #[test]
    fn an_unsigned_tool_capable_model_is_admitted_with_a_warning_not_silently() {
        let mut m = tiny();
        m.capabilities.push(ModelCapability::Tools);
        let opt_in = AdmissionRequest {
            allow_unsigned: true,
            ..Default::default()
        };
        let a = admit(&m, &pi5(), &opt_in, &cpu()).unwrap();
        assert!(
            a.warnings.iter().any(|w| w.contains("executable content")),
            "{:?}",
            a.warnings
        );
    }

    #[test]
    fn a_gpu_model_is_charged_against_vram_not_system_ram() {
        let mut m = signed(medium());
        m.backends = vec![BackendId::LlamaCppCuda];
        let a = admit(
            &m,
            &workstation(),
            &AdmissionRequest::default(),
            &[BackendId::LlamaCppCuda],
        )
        .unwrap();
        assert_eq!(a.pool, MemoryPool::AcceleratorVram);
        assert!(
            a.budget_bytes < workstation().hardware.memory.available_bytes,
            "VRAM, not the machine's RAM"
        );
    }

    #[test]
    fn an_offloading_backend_on_a_card_with_no_vram_figure_is_refused() {
        // vram_bytes: None means undetectable, not zero. Guessing either way risks the
        // exact OOM this module exists to prevent.
        let mut report = report_workstation();
        for a in &mut report.accelerators {
            a.vram_bytes = None;
        }
        let profile = classify(&report);
        let mut m = signed(medium());
        m.backends = vec![BackendId::LlamaCppCuda];
        let err = admit(
            &m,
            &profile,
            &AdmissionRequest::default(),
            &[BackendId::LlamaCppCuda],
        )
        .unwrap_err();
        assert!(
            matches!(err, AdmissionError::UnknownAcceleratorMemory { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("rather than guessing"), "{err}");
    }

    #[test]
    fn with_no_backend_installed_nothing_is_admitted_anywhere() {
        // Today's honest state on every machine.
        for profile in [pi_zero(), pi4(), pi5(), workstation()] {
            let err = admit(&signed(tiny()), &profile, &AdmissionRequest::default(), &[]);
            assert!(
                matches!(err, Err(AdmissionError::NoBackendAvailable(_))) || err.is_err(),
                "{err:?}"
            );
        }
    }

    #[test]
    fn a_tight_fit_is_admitted_but_flagged() {
        let profile = pi4();
        let budget = profile.hardware.memory.available_bytes - Reserve::for_tier(profile.tier).bytes;
        let mut m = signed(tiny());
        m.min_tier = Tier::T0Micro;
        m.footprint = crate::manifest::Footprint {
            weights_bytes: budget - MIB,
            kv_per_1k_ctx_bytes: 0,
            overhead_bytes: 0,
        };
        let a = admit(&m, &profile, &AdmissionRequest::default(), &cpu()).unwrap();
        assert!(
            a.warnings.iter().any(|w| w.contains("headroom")),
            "{:?}",
            a.warnings
        );
    }

    #[test]
    fn a_malformed_manifest_is_refused_before_anything_else_is_considered() {
        let mut m = signed(tiny());
        m.footprint.weights_bytes = 0;
        assert!(matches!(
            admit(&m, &workstation(), &AdmissionRequest::default(), &cpu()),
            Err(AdmissionError::Manifest(_))
        ));
    }

    #[test]
    fn concurrent_sequences_are_charged_and_can_tip_a_fit_into_a_refusal() {
        let profile = pi5();
        let mut m = signed(tiny());
        m.min_tier = Tier::T0Micro;
        m.max_context = 8192;
        m.footprint.kv_per_1k_ctx_bytes = 512 * MIB;
        let single = AdmissionRequest {
            sequences: 1,
            ..Default::default()
        };
        let many = AdmissionRequest {
            sequences: 8,
            ..Default::default()
        };
        assert!(admit(&m, &profile, &single, &cpu()).is_ok());
        assert!(matches!(
            admit(&m, &profile, &many, &cpu()),
            Err(AdmissionError::InsufficientMemory { .. })
        ));
    }

    #[test]
    fn every_refusal_names_the_model_and_a_number() {
        // A refusal a user cannot act on is barely better than a crash.
        let mut m = signed(huge());
        m.min_tier = Tier::T0Micro;
        let err = admit(&m, &pi4(), &AdmissionRequest::default(), &cpu()).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("huge-70b-q4"), "{text}");
        assert!(text.contains("GiB"), "{text}");
    }
}

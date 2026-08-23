//! Which backend, if any, could run this model on this machine.
//!
//! A pure function of the manifest and the capability profile, per
//! `docs/ai/AI-RUNTIME.md` §3: "Backend selection comes from the capability profile plus
//! the model manifest. It is a pure function, and it is unit-testable against fixture
//! profiles with no hardware present."
//!
//! # Selection is not availability
//!
//! What this module decides is *which* backend should run a model. Whether any backend is
//! installed at all is a property of the filesystem, and lives in [`crate::discovery`].
//! Keeping them apart is what lets the interesting decision — a Pi choosing NEON CPU
//! inference over an RK3588's NPU — be unit-tested on a machine that has neither, by
//! passing the available set in explicitly.

use otwono_capability::axes::AcceleratorClass;
use otwono_capability::CapabilityProfile;
use serde::{Deserialize, Serialize};

use crate::manifest::{ModelFormat, ModelManifest};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendId {
    LlamaCppCpu,
    LlamaCppVulkan,
    LlamaCppCuda,
    LlamaCppRocm,
    OnnxRuntime,
    WhisperCpp,
    Piper,
    Vllm,
}

impl BackendId {
    pub fn as_str(&self) -> &'static str {
        match self {
            BackendId::LlamaCppCpu => "llama-cpp-cpu",
            BackendId::LlamaCppVulkan => "llama-cpp-vulkan",
            BackendId::LlamaCppCuda => "llama-cpp-cuda",
            BackendId::LlamaCppRocm => "llama-cpp-rocm",
            BackendId::OnnxRuntime => "onnx-runtime",
            BackendId::WhisperCpp => "whisper-cpp",
            BackendId::Piper => "piper",
            BackendId::Vllm => "vllm",
        }
    }

    /// Formats this backend can execute at all.
    pub fn accepts(&self, format: ModelFormat) -> bool {
        match self {
            BackendId::LlamaCppCpu
            | BackendId::LlamaCppVulkan
            | BackendId::LlamaCppCuda
            | BackendId::LlamaCppRocm
            | BackendId::WhisperCpp => format == ModelFormat::Gguf,
            BackendId::OnnxRuntime => matches!(format, ModelFormat::Onnx | ModelFormat::Rknn),
            BackendId::Piper => format == ModelFormat::Onnx,
            BackendId::Vllm => matches!(format, ModelFormat::Safetensors | ModelFormat::Gguf),
        }
    }

    /// Whether the model's weights live in accelerator memory rather than system RAM.
    ///
    /// This changes which pool admission control has to charge, so it is a property of the
    /// backend and not a hint.
    pub fn offloads_to_accelerator(&self) -> bool {
        matches!(
            self,
            BackendId::LlamaCppVulkan | BackendId::LlamaCppCuda | BackendId::LlamaCppRocm | BackendId::Vllm
        )
    }
}

/// The backend chosen for a model, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendSelection {
    pub backend: BackendId,
    pub offloads_to_accelerator: bool,
    /// Why this one and not the others. Kept because "why is it slow" is the question
    /// users actually ask.
    pub reason: String,
    /// Backends the manifest declared that this machine ruled out, with the reason.
    pub rejected: Vec<(BackendId, String)>,
}

/// Choose a backend for `manifest` on `profile`, from `available`.
///
/// `available` is a parameter rather than a call to [`crate::installed_backends`] so the
/// decision can be exercised for hardware and installs that are not the one running the
/// test.
pub fn select_backend(
    manifest: &ModelManifest,
    profile: &CapabilityProfile,
    available: &[BackendId],
) -> Result<BackendSelection, SelectionError> {
    if available.is_empty() {
        return Err(SelectionError::NoBackendInstalled);
    }

    let mut rejected = Vec::new();
    // Manifest order is the publisher's preference, most preferred first.
    for &candidate in &manifest.backends {
        if !available.contains(&candidate) {
            rejected.push((candidate, "not installed in this build".to_string()));
            continue;
        }
        if !candidate.accepts(manifest.format) {
            rejected.push((candidate, format!("cannot execute {:?} weights", manifest.format)));
            continue;
        }
        if let Err(why) = accelerator_supports(candidate, profile) {
            rejected.push((candidate, why));
            continue;
        }
        return Ok(BackendSelection {
            backend: candidate,
            offloads_to_accelerator: candidate.offloads_to_accelerator(),
            reason: format!(
                "highest-preference backend this machine can run ({})",
                candidate.as_str()
            ),
            rejected,
        });
    }

    Err(SelectionError::NoUsableBackend { rejected })
}

/// Does this machine have the accelerator the backend needs?
fn accelerator_supports(backend: BackendId, profile: &CapabilityProfile) -> Result<(), String> {
    let accel = profile.axes.accelerator;
    match backend {
        // CPU backends run anywhere.
        BackendId::LlamaCppCpu | BackendId::WhisperCpp | BackendId::Piper => Ok(()),
        BackendId::LlamaCppVulkan => match accel {
            AcceleratorClass::None => Err("no GPU detected".into()),
            _ => Ok(()),
        },
        BackendId::LlamaCppCuda => require_vendor(profile, "nvidia", "CUDA"),
        BackendId::LlamaCppRocm => require_vendor(profile, "amd", "ROCm"),
        BackendId::OnnxRuntime => Ok(()),
        BackendId::Vllm => match accel {
            AcceleratorClass::GpuSmall | AcceleratorClass::GpuLarge | AcceleratorClass::GpuMulti => Ok(()),
            _ => Err("vLLM needs a discrete GPU".into()),
        },
    }
}

fn require_vendor(profile: &CapabilityProfile, vendor: &str, label: &str) -> Result<(), String> {
    let present = profile
        .hardware
        .accelerators
        .iter()
        .any(|a| a.vendor.eq_ignore_ascii_case(vendor));
    if present {
        Ok(())
    } else {
        Err(format!("no {vendor} device, so {label} is unavailable"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionError {
    /// No inference engine is installed on this machine at all.
    NoBackendInstalled,
    /// Engines exist, but none of the ones this model declares can run here.
    NoUsableBackend { rejected: Vec<(BackendId, String)> },
}

impl std::fmt::Display for SelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelectionError::NoBackendInstalled => write!(
                f,
                "no inference backend is installed on this node, so no model can be run locally"
            ),
            SelectionError::NoUsableBackend { rejected } => {
                write!(f, "no usable backend for this model:")?;
                for (backend, why) in rejected {
                    write!(f, " {}: {why};", backend.as_str())?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for SelectionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::fixtures::*;
    use otwono_capability::classify;
    use otwono_capability::testing::{report_pi5_16gb, report_workstation};

    fn pi5() -> CapabilityProfile {
        classify(&report_pi5_16gb())
    }

    fn workstation() -> CapabilityProfile {
        classify(&report_workstation())
    }

    #[test]
    fn a_machine_with_nothing_installed_is_told_so_rather_than_offered_a_backend() {
        assert_eq!(
            select_backend(&tiny(), &pi5(), &[]),
            Err(SelectionError::NoBackendInstalled)
        );
    }

    #[test]
    fn cpu_inference_is_selected_on_a_machine_with_no_gpu() {
        let mut m = tiny();
        m.backends = vec![BackendId::LlamaCppCuda, BackendId::LlamaCppCpu];
        let choice = select_backend(&m, &pi5(), &[BackendId::LlamaCppCpu, BackendId::LlamaCppCuda]).unwrap();
        assert_eq!(choice.backend, BackendId::LlamaCppCpu);
        assert!(!choice.offloads_to_accelerator);
        // And it says why CUDA lost, because "why is this slow" is the real question.
        assert!(
            choice
                .rejected
                .iter()
                .any(|(b, why)| *b == BackendId::LlamaCppCuda && why.contains("nvidia")),
            "{:?}",
            choice.rejected
        );
    }

    #[test]
    fn a_discrete_nvidia_machine_prefers_cuda_when_the_manifest_does() {
        let mut m = medium();
        m.backends = vec![BackendId::LlamaCppCuda, BackendId::LlamaCppCpu];
        let choice = select_backend(
            &m,
            &workstation(),
            &[BackendId::LlamaCppCpu, BackendId::LlamaCppCuda],
        )
        .unwrap();
        assert_eq!(choice.backend, BackendId::LlamaCppCuda);
        assert!(choice.offloads_to_accelerator, "CUDA weights live in VRAM");
    }

    #[test]
    fn manifest_order_is_the_publisher_preference_and_is_respected() {
        let mut m = medium();
        m.backends = vec![BackendId::LlamaCppCpu, BackendId::LlamaCppCuda];
        let choice = select_backend(
            &m,
            &workstation(),
            &[BackendId::LlamaCppCpu, BackendId::LlamaCppCuda],
        )
        .unwrap();
        assert_eq!(
            choice.backend,
            BackendId::LlamaCppCpu,
            "the manifest asked for CPU first"
        );
    }

    #[test]
    fn a_backend_that_cannot_read_the_format_is_rejected() {
        let mut m = tiny();
        m.format = ModelFormat::Onnx;
        m.backends = vec![BackendId::LlamaCppCpu, BackendId::OnnxRuntime];
        let choice = select_backend(&m, &pi5(), &[BackendId::LlamaCppCpu, BackendId::OnnxRuntime]).unwrap();
        assert_eq!(choice.backend, BackendId::OnnxRuntime);
        assert!(choice.rejected.iter().any(|(b, _)| *b == BackendId::LlamaCppCpu));
    }

    #[test]
    fn no_usable_backend_explains_every_rejection() {
        // A user whose GPU model will not load needs the list, not a bare "no".
        let mut m = medium();
        m.backends = vec![BackendId::LlamaCppCuda, BackendId::Vllm];
        let err = select_backend(&m, &pi5(), &[BackendId::LlamaCppCuda, BackendId::Vllm]).unwrap_err();
        let SelectionError::NoUsableBackend { rejected } = &err else {
            panic!("expected NoUsableBackend, got {err:?}");
        };
        assert_eq!(rejected.len(), 2);
        assert!(err.to_string().contains("nvidia"), "{err}");
        assert!(err.to_string().contains("discrete GPU"), "{err}");
    }

    #[test]
    fn vllm_is_refused_without_a_discrete_gpu() {
        let mut m = huge();
        m.backends = vec![BackendId::Vllm];
        assert!(select_backend(&m, &pi5(), &[BackendId::Vllm]).is_err());
    }

    #[test]
    fn backend_ids_round_trip_through_their_wire_names() {
        for b in [
            BackendId::LlamaCppCpu,
            BackendId::LlamaCppVulkan,
            BackendId::LlamaCppCuda,
            BackendId::LlamaCppRocm,
            BackendId::OnnxRuntime,
            BackendId::WhisperCpp,
            BackendId::Piper,
            BackendId::Vllm,
        ] {
            let json = serde_json::to_string(&b).unwrap();
            assert_eq!(json, format!("\"{}\"", b.as_str()));
            assert_eq!(serde_json::from_str::<BackendId>(&json).unwrap(), b);
        }
    }
}

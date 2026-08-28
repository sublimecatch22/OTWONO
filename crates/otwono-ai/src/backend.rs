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

    /// Installing a GPU build must not change what a GPU-less machine runs.
    ///
    /// This is the one property that makes shipping a Vulkan variant safe, and until now it
    /// was only *implied*: `discovery` was tested for what it finds and `select_backend` for
    /// what it picks from a hand-written list, and nothing joined the two. So "adding a
    /// variant to an image cannot disturb the CPU path" was an argument about two modules
    /// rather than a fact about their composition.
    ///
    /// Here the available set comes from a filesystem, as it does in the daemon.
    #[test]
    fn adding_a_gpu_variant_to_the_image_does_not_change_a_gpu_less_machine() {
        let tree = temptree("variant-safety");
        adapter(&tree);
        engine(&tree, "cpu");

        let mut m = tiny();
        // The publisher prefers the GPU. That is the interesting case: if preference alone
        // decided, this machine would pick a backend it cannot run.
        m.backends = vec![BackendId::LlamaCppVulkan, BackendId::LlamaCppCpu];

        let cpu_only = crate::installed_backends_in(&tree);
        assert_eq!(cpu_only, vec![BackendId::LlamaCppCpu]);
        let before = select_backend(&m, &pi5(), &cpu_only).expect("cpu-only image runs it");
        assert_eq!(before.backend, BackendId::LlamaCppCpu);

        // Now ship the Vulkan build alongside, exactly as stage 35 does with
        // AI_ENGINE_VARIANTS="cpu vulkan". Nothing else about the machine changes.
        engine(&tree, "vulkan");
        let both = crate::installed_backends_in(&tree);
        assert_eq!(
            both,
            vec![BackendId::LlamaCppCpu, BackendId::LlamaCppVulkan],
            "the variant must be discovered, or this test proves nothing"
        );

        let after = select_backend(&m, &pi5(), &both).expect("still runnable");
        assert_eq!(
            after.backend, before.backend,
            "installing a GPU engine changed what a machine with no GPU runs"
        );
        assert!(!after.offloads_to_accelerator);
        assert!(
            after
                .rejected
                .iter()
                .any(|(b, why)| *b == BackendId::LlamaCppVulkan && why.contains("no GPU")),
            "Vulkan must be rejected for the machine's sake, not silently skipped: {:?}",
            after.rejected
        );
        let _ = std::fs::remove_dir_all(&tree);
    }

    /// ...and the same image *does* use the GPU where there is one.
    ///
    /// The other half of the pair. Without it, "adding a variant is safe" would also be
    /// satisfied by a variant that is never selected anywhere, which is not a feature.
    #[test]
    fn the_same_image_reaches_for_the_gpu_on_a_machine_that_has_one() {
        let tree = temptree("variant-useful");
        adapter(&tree);
        engine(&tree, "cpu");
        engine(&tree, "vulkan");

        let mut m = medium();
        m.backends = vec![BackendId::LlamaCppVulkan, BackendId::LlamaCppCpu];

        let choice = select_backend(&m, &workstation(), &crate::installed_backends_in(&tree))
            .expect("a workstation runs it");
        assert_eq!(choice.backend, BackendId::LlamaCppVulkan);
        assert!(
            choice.offloads_to_accelerator,
            "admission control charges the wrong pool if this is wrong"
        );
        let _ = std::fs::remove_dir_all(&tree);
    }

    /// A variant on disk that the running machine cannot use is still *reported* as
    /// installed, and that is deliberate.
    ///
    /// `installed_backends` answers "what is on this disk", not "what will run here" — the
    /// second question needs a model, because it is the manifest that says which backends
    /// are even candidates. Collapsing the two would make `ai.capabilities` unable to
    /// distinguish "this image has no Vulkan build" from "this machine has no GPU", which
    /// are different problems with different fixes.
    #[test]
    fn discovery_reports_what_is_installed_not_what_is_usable_here() {
        let tree = temptree("variant-honest");
        adapter(&tree);
        engine(&tree, "cpu");
        engine(&tree, "vulkan");
        assert!(crate::installed_backends_in(&tree).contains(&BackendId::LlamaCppVulkan));
        let _ = std::fs::remove_dir_all(&tree);
    }

    fn temptree(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("otwono-variant-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn adapter(root: &std::path::Path) {
        write_executable(
            &root
                .join(crate::discovery::ADAPTER_DIR)
                .join(crate::discovery::LLAMA_ADAPTER),
        );
    }

    fn engine(root: &std::path::Path, variant: &str) {
        write_executable(
            &root
                .join(crate::discovery::ENGINE_DIR)
                .join("llama.cpp")
                .join(variant)
                .join("bin")
                .join("llama-server"),
        );
    }

    fn write_executable(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
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

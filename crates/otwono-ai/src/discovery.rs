//! Which backends are actually installed on this filesystem.
//!
//! Until now [`installed_backends`] returned an empty list because nothing was integrated.
//! It now looks, and the looking is a **path probe, not a link-time fact**: a backend is
//! present when its adapter binary and its engine binary are both on disk and executable.
//!
//! # Why on disk and not a Cargo feature
//!
//! One OTWONO build produces one set of binaries, and the same binaries are installed on a
//! Pi with a CPU-only engine and on a workstation that also has CUDA. Deciding backend
//! availability at compile time would mean a separate daemon build per hardware shape,
//! which is exactly the "capability tier" mistake CLAUDE.md §2.6 exists to prevent — and it
//! would make `ai.capabilities` a claim about the build rather than about the machine.
//!
//! # Why the engine is not linked
//!
//! `otwono-ai` decides *whether* a model may load. It must stay buildable and testable on a
//! machine with no inference engine anywhere on it, including a CI runner, so it never
//! depends on `otwono-llama` — only on the paths where an adapter would be. The dependency
//! points the other way.
//!
//! Everything here takes a filesystem root, per CLAUDE.md §6: the tests run against fixture
//! trees, never against the real `/`.

use std::path::{Path, PathBuf};

use crate::backend::BackendId;

/// Adapter binaries, relative to the filesystem root.
pub const ADAPTER_DIR: &str = "usr/libexec/otwono/ai-backends";

/// Engine builds, relative to the filesystem root.
pub const ENGINE_DIR: &str = "usr/lib/otwono/ai";

/// The adapter that speaks for every llama.cpp build.
pub const LLAMA_ADAPTER: &str = "otwono-llama-backend";

/// An installed backend: the adapter to run, and the engine to hand it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendInstall {
    pub backend: BackendId,
    /// The adapter binary `otwono-aid` spawns under `supervisor::BackendProcess`.
    pub adapter: PathBuf,
    /// The engine binary the adapter drives, passed to it as `--engine`.
    pub engine: PathBuf,
}

/// llama.cpp build variants, in the order they are reported.
///
/// Order is preference-neutral here: which one is *used* comes from the model manifest and
/// [`crate::select_backend`]. This order only makes the output deterministic, which matters
/// because it ends up in `ai.capabilities` and in a boot log.
const LLAMA_VARIANTS: &[(&str, BackendId)] = &[
    ("cpu", BackendId::LlamaCppCpu),
    ("vulkan", BackendId::LlamaCppVulkan),
    ("cuda", BackendId::LlamaCppCuda),
    ("rocm", BackendId::LlamaCppRocm),
];

/// Find every backend installed under `root`.
pub fn discover(root: &Path) -> Vec<BackendInstall> {
    let adapter = root.join(ADAPTER_DIR).join(LLAMA_ADAPTER);
    if !is_executable(&adapter) {
        // No adapter means no llama.cpp backend, however many engine builds are lying
        // around. Reporting an engine we have no way to drive would make
        // `local_inference_available` true on a node where every load fails.
        return Vec::new();
    }
    LLAMA_VARIANTS
        .iter()
        .filter_map(|(dir, backend)| {
            let engine = root
                .join(ENGINE_DIR)
                .join("llama.cpp")
                .join(dir)
                .join("bin")
                .join("llama-server");
            is_executable(&engine).then(|| BackendInstall {
                backend: *backend,
                adapter: adapter.clone(),
                engine,
            })
        })
        .collect()
}

/// Backend ids installed under `root`.
pub fn installed_backends_in(root: &Path) -> Vec<BackendId> {
    discover(root).into_iter().map(|i| i.backend).collect()
}

/// Backends this machine can actually execute.
///
/// The production entry point: [`installed_backends_in`] over `/`.
pub fn installed_backends() -> Vec<BackendId> {
    installed_backends_in(Path::new("/"))
}

/// A regular file with an execute bit set.
///
/// The execute bit is checked rather than mere existence because a half-finished install —
/// a file copied without its mode — should report "no backend" rather than fail later with
/// a permission error from inside the supervisor.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Tree {
            let path = std::env::temp_dir().join(format!("otwono-discovery-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Tree(path)
        }

        fn put(&self, relative: &str, executable: bool) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"#!/bin/sh\n").unwrap();
            let mode = if executable { 0o755 } else { 0o644 };
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
            path
        }

        fn adapter(&self) -> PathBuf {
            self.put(&format!("{ADAPTER_DIR}/{LLAMA_ADAPTER}"), true)
        }

        fn engine(&self, variant: &str, executable: bool) -> PathBuf {
            self.put(
                &format!("{ENGINE_DIR}/llama.cpp/{variant}/bin/llama-server"),
                executable,
            )
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_empty_filesystem_has_no_backends() {
        let tree = Tree::new("empty");
        assert!(discover(&tree.0).is_empty());
    }

    #[test]
    fn a_cpu_engine_and_its_adapter_are_discovered_together() {
        let tree = Tree::new("cpu");
        tree.adapter();
        let engine = tree.engine("cpu", true);

        let found = discover(&tree.0);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].backend, BackendId::LlamaCppCpu);
        assert_eq!(found[0].engine, engine);
        assert_eq!(installed_backends_in(&tree.0), vec![BackendId::LlamaCppCpu]);
    }

    #[test]
    fn an_engine_with_no_adapter_is_not_a_usable_backend() {
        // Otherwise ai.capabilities would promise local inference on a node where every
        // single load fails, which is worse than promising nothing.
        let tree = Tree::new("noadapter");
        tree.engine("cpu", true);
        assert!(discover(&tree.0).is_empty());
    }

    #[test]
    fn an_adapter_with_no_engine_is_not_a_usable_backend() {
        let tree = Tree::new("noengine");
        tree.adapter();
        assert!(discover(&tree.0).is_empty());
    }

    #[test]
    fn a_binary_installed_without_its_execute_bit_does_not_count() {
        // A real failure mode of copying files around, and one that would otherwise
        // surface much later as a permission error from inside the supervisor.
        let tree = Tree::new("nomode");
        tree.adapter();
        tree.engine("cpu", false);
        assert!(discover(&tree.0).is_empty());
    }

    #[test]
    fn several_engine_variants_are_all_reported_in_a_fixed_order() {
        let tree = Tree::new("many");
        tree.adapter();
        tree.engine("cuda", true);
        tree.engine("cpu", true);
        tree.engine("vulkan", true);
        assert_eq!(
            installed_backends_in(&tree.0),
            vec![
                BackendId::LlamaCppCpu,
                BackendId::LlamaCppVulkan,
                BackendId::LlamaCppCuda
            ],
            "order must not depend on directory iteration"
        );
    }

    #[test]
    fn a_directory_where_the_engine_should_be_is_not_an_engine() {
        let tree = Tree::new("dir");
        tree.adapter();
        std::fs::create_dir_all(tree.0.join(ENGINE_DIR).join("llama.cpp/cpu/bin/llama-server")).unwrap();
        assert!(discover(&tree.0).is_empty());
    }

    #[test]
    fn discovery_on_a_root_that_does_not_exist_is_empty_not_an_error() {
        assert!(discover(Path::new("/nonexistent-otwono-root")).is_empty());
    }
}

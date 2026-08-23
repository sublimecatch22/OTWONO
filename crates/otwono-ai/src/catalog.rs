//! The on-disk model catalog.
//!
//! A directory of manifests, and separately a content-addressed store of weights. The two
//! are deliberately independent: a node can hold a manifest for a model it has not
//! downloaded, which is what lets a catalog show "this exists, here is what it would cost,
//! and here is why this machine cannot run it" without fetching gigabytes first.
//!
//! ```text
//! /var/lib/otwono/models/
//!   manifests/<id>.json      the contract
//!   blobs/<blake3>           the weights, content-addressed
//! ```
//!
//! Reading is injectable via the root path, like every other OTWONO probe (CLAUDE.md §6),
//! so the tests below run against fixture directories and never touch `/var`.

use std::path::{Path, PathBuf};

use crate::manifest::{ManifestError, ModelManifest};

pub const DEFAULT_MODEL_DIR: &str = "/var/lib/otwono/models";
const MANIFEST_DIR: &str = "manifests";
const BLOB_DIR: &str = "blobs";

pub struct Catalog {
    root: PathBuf,
}

/// A catalog entry: the manifest, and whether the weights are actually here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub manifest: ModelManifest,
    /// False when only the manifest is present. Not an error — see the module docs.
    pub weights_present: bool,
}

impl Catalog {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Catalog {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub fn manifest_dir(&self) -> PathBuf {
        self.root.join(MANIFEST_DIR)
    }

    pub fn blob_dir(&self) -> PathBuf {
        self.root.join(BLOB_DIR)
    }

    pub fn blob_path(&self, blake3: &str) -> PathBuf {
        self.blob_dir().join(blake3)
    }

    /// Create the catalog layout if it is not there.
    ///
    /// Called by the daemon at startup rather than baked into the image: `/var/lib/otwono`
    /// is a separate partition, mounted over whatever the root filesystem had at that
    /// path, so directories created at build time are shadowed the moment it mounts. The
    /// keystore already learned this; the catalog owns its layout the same way.
    ///
    /// Manifests and blobs are world-readable: a manifest is public metadata and a blob is
    /// content-addressed weights, neither of which is a secret. Nothing here is 0600.
    pub fn ensure_layout(&self) -> Result<(), CatalogError> {
        for dir in [self.manifest_dir(), self.blob_dir()] {
            std::fs::create_dir_all(&dir).map_err(|e| CatalogError::Io(format!("{}: {e}", dir.display())))?;
        }
        Ok(())
    }

    /// Every valid manifest in the catalog, sorted by id.
    ///
    /// Invalid manifests are reported alongside the good ones rather than failing the
    /// whole listing: one corrupt file must not hide the rest of a user's models.
    pub fn list(&self) -> Result<(Vec<CatalogEntry>, Vec<CatalogProblem>), CatalogError> {
        let dir = self.manifest_dir();
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            // A node with no models yet is normal, not broken.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
            Err(e) => return Err(CatalogError::Io(format!("{}: {e}", dir.display()))),
        };

        let mut entries = Vec::new();
        let mut problems = Vec::new();
        for item in read.flatten() {
            let path = item.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            match self.load_file(&path) {
                Ok(entry) => entries.push(entry),
                Err(problem) => problems.push(problem),
            }
        }
        entries.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
        problems.sort_by(|a, b| a.path.cmp(&b.path));
        Ok((entries, problems))
    }

    pub fn get(&self, id: &str) -> Result<CatalogEntry, CatalogError> {
        let (entries, _) = self.list()?;
        entries
            .into_iter()
            .find(|e| e.manifest.id == id)
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))
    }

    fn load_file(&self, path: &Path) -> Result<CatalogEntry, CatalogProblem> {
        let text = std::fs::read_to_string(path).map_err(|e| CatalogProblem {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        let manifest: ModelManifest = serde_json::from_str(&text).map_err(|e| CatalogProblem {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        manifest.validate().map_err(|e: ManifestError| CatalogProblem {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        let weights_present = self.blob_path(&manifest.blake3).exists();
        Ok(CatalogEntry {
            manifest,
            weights_present,
        })
    }
}

/// A manifest file that could not be used, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogProblem {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug)]
pub enum CatalogError {
    Io(String),
    NotFound(String),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogError::Io(e) => write!(f, "{e}"),
            CatalogError::NotFound(id) => write!(f, "no model {id:?} in the catalog"),
        }
    }
}

impl std::error::Error for CatalogError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::fixtures::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("otwono-cat-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(MANIFEST_DIR)).unwrap();
        d
    }

    fn write(root: &Path, m: &ModelManifest) {
        std::fs::write(
            root.join(MANIFEST_DIR).join(format!("{}.json", m.id)),
            serde_json::to_string_pretty(m).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn an_empty_catalog_is_not_an_error() {
        // A node with no models yet is the normal first-boot state.
        let c = Catalog::new(std::env::temp_dir().join("otwono-cat-does-not-exist"));
        let (entries, problems) = c.list().unwrap();
        assert!(entries.is_empty());
        assert!(problems.is_empty());
    }

    #[test]
    fn manifests_are_listed_sorted_and_report_whether_weights_are_here() {
        let root = scratch("list");
        write(&root, &medium());
        write(&root, &tiny());
        let c = Catalog::new(&root);

        let (entries, problems) = c.list().unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            entries.iter().map(|e| e.manifest.id.as_str()).collect::<Vec<_>>(),
            vec!["medium-8b-q4", "tiny-1b-q4"]
        );
        assert!(
            entries.iter().all(|e| !e.weights_present),
            "no blobs were written"
        );

        // Drop the blob in and the same manifest now reports it present.
        std::fs::create_dir_all(c.blob_dir()).unwrap();
        std::fs::write(c.blob_path(&tiny().blake3), b"not really weights").unwrap();
        let (entries, _) = c.list().unwrap();
        let t = entries.iter().find(|e| e.manifest.id == "tiny-1b-q4").unwrap();
        assert!(t.weights_present);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn one_corrupt_manifest_does_not_hide_the_rest() {
        // Otherwise a single bad file makes every other model on the machine disappear.
        let root = scratch("corrupt");
        write(&root, &tiny());
        std::fs::write(root.join(MANIFEST_DIR).join("broken.json"), "{not json").unwrap();

        let (entries, problems) = Catalog::new(&root).list().unwrap();
        assert_eq!(entries.len(), 1, "the good one must still be listed");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].path.ends_with("broken.json"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_manifest_that_fails_validation_is_a_problem_not_an_entry() {
        // A zero-weight manifest would otherwise be admitted on any machine.
        let root = scratch("invalid");
        let mut bad = tiny();
        bad.id = "zero-weight".into();
        bad.footprint.weights_bytes = 0;
        write(&root, &bad);

        let (entries, problems) = Catalog::new(&root).list().unwrap();
        assert!(entries.is_empty());
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].reason.contains("weights_bytes is zero"),
            "{problems:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn non_json_files_are_ignored_rather_than_reported() {
        let root = scratch("junk");
        write(&root, &tiny());
        std::fs::write(root.join(MANIFEST_DIR).join("README.md"), "notes").unwrap();
        let (entries, problems) = Catalog::new(&root).list().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(problems.is_empty(), "{problems:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_layout_is_created_on_demand_and_is_idempotent() {
        // /var/lib/otwono is a mount, so anything created at build time is shadowed when
        // it mounts. The daemon has to make its own directories at startup.
        let root = std::env::temp_dir().join(format!("otwono-cat-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let c = Catalog::new(&root);
        assert!(!c.manifest_dir().exists());

        c.ensure_layout().unwrap();
        assert!(c.manifest_dir().is_dir());
        assert!(c.blob_dir().is_dir());

        c.ensure_layout().expect("a second call must be harmless");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn getting_a_missing_model_names_it() {
        let root = scratch("missing");
        let err = Catalog::new(&root).get("nope").unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }
}

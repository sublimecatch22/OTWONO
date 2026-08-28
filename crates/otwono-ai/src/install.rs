//! Getting a model into the catalog, and proving it is the model the manifest describes.
//!
//! # The hole this closes
//!
//! Until now `blake3` in a manifest was a *filename*. `Catalog::blob_path` joined it onto
//! the blob directory and nothing ever hashed the contents. A signed manifest paired with
//! a swapped blob would therefore load as trusted: the signature covers the manifest, the
//! manifest names a digest, and nobody checked that the bytes matched the digest. The
//! signature work of slice 2 was doing half a job.
//!
//! Installing now hashes the blob and refuses on mismatch, so the chain runs end to end:
//! a trusted publisher signs a manifest, the manifest names a digest, and the digest names
//! these exact bytes.
//!
//! # Why verification happens at install and not at load
//!
//! Hashing is linear in model size. A 4 GB model costs seconds of pure I/O, and paying
//! that on every load — on the small hardware this project exists for — would be a tax on
//! the common path to defend against an attacker who already has write access to a
//! root-owned directory, which is to say root. So: verify when the bytes arrive, and
//! expose [`Catalog::verify`] for re-checking on demand. `ai.models.verify` is that,
//! surfaced.
//!
//! # Atomicity
//!
//! A blob is staged beside its destination and renamed into place. An interrupted install
//! leaves a stray `.incoming-*` file, never a half-written blob under a name that claims
//! to be a complete one — because `weights_present` is a file-exists check, and a truncated
//! file would answer yes.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::catalog::{Catalog, CatalogError};
use crate::manifest::ModelManifest;
use crate::signature::{PublisherTrust, SignatureError, SignatureStatus};

/// How much to read at a time when hashing. Large enough that syscall overhead disappears,
/// small enough that a 4 GB model does not become 4 GB of resident memory.
const HASH_CHUNK: usize = 1024 * 1024;

/// What an install did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub model_id: String,
    pub blake3: String,
    pub size_bytes: u64,
    /// True when the blob was already present and verified, so nothing was copied.
    pub already_present: bool,
    pub provenance: Provenance,
}

/// How the manifest's signature checked out, in the form a caller can log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    Trusted {
        publisher: String,
    },
    /// Accepted only because the caller opted in.
    Unsigned,
    /// Signature verifies, signer unknown to this node. Accepted only with the opt-in.
    UntrustedPublisher,
}

/// Caller choices for one install.
#[derive(Debug, Clone, Default)]
pub struct InstallRequest {
    /// Accept a manifest with no signature, or one signed by an unknown publisher.
    ///
    /// Never accepts a *broken* signature: that is a manifest altered since it was signed,
    /// and no opt-in covers it (`docs/ai/AI-RUNTIME.md` §5).
    pub allow_unsigned: bool,
}

/// Hash a file with BLAKE3, streaming.
pub fn hash_file(path: &Path) -> Result<String, InstallError> {
    let mut file = std::fs::File::open(path).map_err(|e| InstallError::Io {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; HASH_CHUNK];
    loop {
        let read = file.read(&mut buffer).map_err(|e| InstallError::Io {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Re-hash an installed model's weights and compare them to its manifest.
///
/// The on-demand half of "verify at install, not at load". Linear in model size, so it is
/// a thing a caller asks for, never something that happens behind their back.
pub fn verify_installed(catalog: &Catalog, model_id: &str) -> Result<Verification, InstallError> {
    let entry = catalog.get(model_id).map_err(InstallError::Catalog)?;
    let blob = catalog.blob_path(&entry.manifest.blake3);
    if !entry.weights_present {
        return Ok(Verification {
            model_id: entry.manifest.id,
            weights_present: false,
            digest_matches: false,
            actual: None,
        });
    }
    let actual = hash_file(&blob)?;
    Ok(Verification {
        digest_matches: actual == entry.manifest.blake3,
        model_id: entry.manifest.id,
        weights_present: true,
        actual: Some(actual),
    })
}

/// The result of re-checking an installed model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    pub model_id: String,
    pub weights_present: bool,
    pub digest_matches: bool,
    /// The digest actually computed. `None` when there were no weights to hash.
    pub actual: Option<String>,
}

/// Install a model into `catalog` from a manifest and a blob already on local disk.
///
/// Deliberately takes local paths. Downloading is a separate concern with a separate threat
/// model — it needs network egress, which `otwono-aid` does not have and should not gain —
/// and keeping it out of here means the verification path can be tested exhaustively
/// without a network in the loop.
pub fn install(
    catalog: &Catalog,
    manifest: &ModelManifest,
    blob: &Path,
    trust: &PublisherTrust,
    request: &InstallRequest,
) -> Result<Installed, InstallError> {
    // Order matters. Provenance first: if we will not accept this manifest at all, there is
    // no reason to spend minutes hashing gigabytes to find out.
    let provenance = check_provenance(manifest, trust, request)?;

    manifest.validate().map_err(InstallError::Manifest)?;

    let metadata = std::fs::metadata(blob).map_err(|e| InstallError::Io {
        path: blob.to_path_buf(),
        reason: e.to_string(),
    })?;
    if !metadata.is_file() {
        return Err(InstallError::Io {
            path: blob.to_path_buf(),
            reason: "not a regular file".into(),
        });
    }
    // Checked before hashing: a size mismatch is the same answer for a thousandth of the
    // work, and a truncated download is the common case this catches.
    if metadata.len() != manifest.size_bytes {
        return Err(InstallError::SizeMismatch {
            expected: manifest.size_bytes,
            actual: metadata.len(),
        });
    }

    let digest = hash_file(blob)?;
    if digest != manifest.blake3 {
        return Err(InstallError::DigestMismatch {
            expected: manifest.blake3.clone(),
            actual: digest,
        });
    }

    catalog.ensure_layout().map_err(InstallError::Catalog)?;
    let destination = catalog.blob_path(&manifest.blake3);
    let already_present = destination.exists();
    if !already_present {
        stage_and_rename(blob, &destination)?;
    }

    let manifest_path = catalog.manifest_dir().join(format!("{}.json", manifest.id));
    let encoded = serde_json::to_vec_pretty(manifest).map_err(|e| InstallError::Io {
        path: manifest_path.clone(),
        reason: format!("cannot serialize the manifest: {e}"),
    })?;
    write_atomically(&manifest_path, &encoded)?;

    Ok(Installed {
        model_id: manifest.id.clone(),
        blake3: manifest.blake3.clone(),
        size_bytes: manifest.size_bytes,
        already_present,
        provenance,
    })
}

/// Decide whether this manifest may be installed at all.
/// Decide whether this manifest is one we will accept at all.
///
/// Public because the same reasoning applies one level up. `install` runs it before hashing
/// gigabytes; `ai.models.pull` runs it before *downloading* them, which is the same trade
/// against a much larger bill. A manifest we would refuse to install is a manifest whose
/// weights should never come down the wire.
pub fn check_provenance(
    manifest: &ModelManifest,
    trust: &PublisherTrust,
    request: &InstallRequest,
) -> Result<Provenance, InstallError> {
    match manifest.verify_signature(trust) {
        Ok(SignatureStatus::Trusted { name, .. }) => Ok(Provenance::Trusted { publisher: name }),
        Ok(SignatureStatus::Unsigned) if request.allow_unsigned => Ok(Provenance::Unsigned),
        Ok(SignatureStatus::Unsigned) => Err(InstallError::Unsigned),
        Err(SignatureError::UntrustedPublisher { .. }) if request.allow_unsigned => {
            Ok(Provenance::UntrustedPublisher)
        }
        Err(SignatureError::UntrustedPublisher { public_key }) => {
            Err(InstallError::UntrustedPublisher { key: public_key })
        }
        // Everything else is a signature that does not verify. No opt-in covers it: the
        // opt-in means "I know where this came from", never "somebody changed this".
        Err(e) => Err(InstallError::BadSignature(e.to_string())),
    }
}

/// Copy into a sibling temporary file, then rename.
///
/// A sibling, not `/tmp`: rename is only atomic within a filesystem, and the blob store is
/// on `/var/lib/otwono`, which is its own partition.
fn stage_and_rename(source: &Path, destination: &Path) -> Result<(), InstallError> {
    let directory = destination.parent().unwrap_or(Path::new("."));
    let staging = directory.join(format!(
        ".incoming-{}-{}",
        std::process::id(),
        destination.file_name().and_then(|n| n.to_str()).unwrap_or("blob")
    ));
    let _ = std::fs::remove_file(&staging);

    std::fs::copy(source, &staging).map_err(|e| InstallError::Io {
        path: staging.clone(),
        reason: e.to_string(),
    })?;
    if let Err(e) = std::fs::rename(&staging, destination) {
        let _ = std::fs::remove_file(&staging);
        return Err(InstallError::Io {
            path: destination.to_path_buf(),
            reason: e.to_string(),
        });
    }
    Ok(())
}

fn write_atomically(path: &Path, contents: &[u8]) -> Result<(), InstallError> {
    let directory = path.parent().unwrap_or(Path::new("."));
    let staging = directory.join(format!(
        ".incoming-{}-{}",
        std::process::id(),
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
    ));
    std::fs::write(&staging, contents).map_err(|e| InstallError::Io {
        path: staging.clone(),
        reason: e.to_string(),
    })?;
    if let Err(e) = std::fs::rename(&staging, path) {
        let _ = std::fs::remove_file(&staging);
        return Err(InstallError::Io {
            path: path.to_path_buf(),
            reason: e.to_string(),
        });
    }
    Ok(())
}

#[derive(Debug)]
pub enum InstallError {
    /// The manifest carries no signature and the caller did not opt in.
    Unsigned,
    /// The signature verifies but this node does not know the signer.
    UntrustedPublisher {
        key: String,
    },
    /// The signature does not verify. Never accepted.
    BadSignature(String),
    Manifest(crate::manifest::ManifestError),
    SizeMismatch {
        expected: u64,
        actual: u64,
    },
    DigestMismatch {
        expected: String,
        actual: String,
    },
    Catalog(CatalogError),
    Io {
        path: PathBuf,
        reason: String,
    },
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::Unsigned => write!(
                f,
                "this manifest is unsigned; installing it needs an explicit opt-in"
            ),
            InstallError::UntrustedPublisher { key } => write!(
                f,
                "the signature verifies but publisher {key} is not in this node's trust store; \
                 installing it needs an explicit opt-in"
            ),
            InstallError::BadSignature(why) => write!(
                f,
                "the manifest signature does not verify ({why}); it has been altered since it \
                 was signed and no opt-in covers that"
            ),
            InstallError::Manifest(e) => write!(f, "the manifest is not usable: {e}"),
            InstallError::SizeMismatch { expected, actual } => write!(
                f,
                "the manifest says {expected} bytes and the file is {actual}; \
                 the download is incomplete or this is the wrong file"
            ),
            InstallError::DigestMismatch { expected, actual } => write!(
                f,
                "the weights do not match the manifest: expected blake3 {expected}, got {actual}"
            ),
            InstallError::Catalog(e) => write!(f, "{e}"),
            InstallError::Io { path, reason } => write!(f, "{}: {reason}", path.display()),
        }
    }
}

impl std::error::Error for InstallError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::fixtures::tiny;
    use crate::signature::testing::sign;

    const PUBLISHER: u8 = 11;
    const STRANGER: u8 = 22;

    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str) -> Dir {
            let path = std::env::temp_dir().join(format!("otwono-install-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Dir(path)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A manifest and a blob whose contents genuinely hash to what the manifest claims.
    fn signed_model(dir: &Path, body: &[u8]) -> (ModelManifest, PathBuf, PublisherTrust) {
        let blob = dir.join("weights.gguf");
        std::fs::write(&blob, body).unwrap();
        let mut manifest = tiny();
        manifest.blake3 = blake3::hash(body).to_hex().to_string();
        manifest.size_bytes = body.len() as u64;
        manifest.footprint.weights_bytes = body.len() as u64;
        let trust = sign(&mut manifest, PUBLISHER);
        (manifest, blob, trust)
    }

    #[test]
    fn a_signed_model_whose_bytes_match_installs() {
        let dir = Dir::new("ok");
        let (manifest, blob, trust) = signed_model(&dir.0, b"pretend these are weights");
        let catalog = Catalog::new(dir.0.join("catalog"));

        let installed = install(&catalog, &manifest, &blob, &trust, &InstallRequest::default()).unwrap();
        assert_eq!(installed.model_id, manifest.id);
        assert!(!installed.already_present);
        assert!(matches!(installed.provenance, Provenance::Trusted { .. }));

        // And the catalog can see it, weights and all.
        let entry = catalog.get(&manifest.id).unwrap();
        assert!(entry.weights_present);
    }

    #[test]
    fn weights_that_do_not_match_the_manifest_are_refused() {
        // The hole this module exists to close. The manifest is properly signed by a
        // trusted publisher; only the bytes are wrong.
        let dir = Dir::new("swapped");
        let (manifest, _blob, trust) = signed_model(&dir.0, b"the real weights");
        let impostor = dir.0.join("impostor.gguf");
        std::fs::write(&impostor, b"the fake weights").unwrap();

        let catalog = Catalog::new(dir.0.join("catalog"));
        let mut m = manifest.clone();
        m.size_bytes = 16;
        m.footprint.weights_bytes = 16;
        // Re-sign so the failure below is definitely the digest and not the signature.
        let trust2 = sign(&mut m, PUBLISHER);
        let _ = trust;

        let err = install(&catalog, &m, &impostor, &trust2, &InstallRequest::default()).unwrap_err();
        assert!(matches!(err, InstallError::DigestMismatch { .. }), "{err:?}");
        assert!(err.to_string().contains("do not match the manifest"), "{err}");
        // Nothing was installed.
        assert!(!catalog.blob_path(&m.blake3).exists());
    }

    #[test]
    fn a_truncated_file_is_caught_by_size_before_anything_is_hashed() {
        let dir = Dir::new("short");
        let (manifest, blob, trust) = signed_model(&dir.0, b"0123456789");
        std::fs::write(&blob, b"012").unwrap();
        let catalog = Catalog::new(dir.0.join("catalog"));

        let err = install(&catalog, &manifest, &blob, &trust, &InstallRequest::default()).unwrap_err();
        let InstallError::SizeMismatch { expected, actual } = &err else {
            panic!("expected SizeMismatch, got {err:?}");
        };
        assert_eq!((*expected, *actual), (10, 3));
    }

    #[test]
    fn a_tampered_manifest_is_refused_whatever_the_opt_in_says() {
        let dir = Dir::new("tampered");
        let (mut manifest, blob, trust) = signed_model(&dir.0, b"weights");
        // Altered after signing. The blob is still correct.
        manifest.family = "somebody-elses-model".into();
        let catalog = Catalog::new(dir.0.join("catalog"));

        for allow_unsigned in [false, true] {
            let err = install(
                &catalog,
                &manifest,
                &blob,
                &trust,
                &InstallRequest { allow_unsigned },
            )
            .unwrap_err();
            assert!(
                matches!(err, InstallError::BadSignature(_)),
                "allow_unsigned={allow_unsigned} must not cover a broken signature: {err:?}"
            );
        }
    }

    #[test]
    fn an_unsigned_manifest_needs_the_opt_in() {
        let dir = Dir::new("unsigned");
        let body = b"weights".as_slice();
        let blob = dir.0.join("w.gguf");
        std::fs::write(&blob, body).unwrap();
        let mut manifest = tiny();
        manifest.blake3 = blake3::hash(body).to_hex().to_string();
        manifest.size_bytes = body.len() as u64;
        manifest.footprint.weights_bytes = body.len() as u64;
        manifest.signature = None;

        let mut probe = tiny();
        let trust = sign(&mut probe, PUBLISHER);
        let catalog = Catalog::new(dir.0.join("catalog"));

        assert!(matches!(
            install(&catalog, &manifest, &blob, &trust, &InstallRequest::default()).unwrap_err(),
            InstallError::Unsigned
        ));
        let installed = install(
            &catalog,
            &manifest,
            &blob,
            &trust,
            &InstallRequest { allow_unsigned: true },
        )
        .unwrap();
        assert_eq!(installed.provenance, Provenance::Unsigned);
    }

    #[test]
    fn a_stranger_signature_needs_the_opt_in_and_is_reported_as_such() {
        let dir = Dir::new("stranger");
        let body = b"weights".as_slice();
        let blob = dir.0.join("w.gguf");
        std::fs::write(&blob, body).unwrap();
        let mut manifest = tiny();
        manifest.blake3 = blake3::hash(body).to_hex().to_string();
        manifest.size_bytes = body.len() as u64;
        manifest.footprint.weights_bytes = body.len() as u64;
        sign(&mut manifest, STRANGER);

        // A trust store that knows a different publisher.
        let mut probe = tiny();
        let trust = sign(&mut probe, PUBLISHER);
        let catalog = Catalog::new(dir.0.join("catalog"));

        assert!(matches!(
            install(&catalog, &manifest, &blob, &trust, &InstallRequest::default()).unwrap_err(),
            InstallError::UntrustedPublisher { .. }
        ));
        let installed = install(
            &catalog,
            &manifest,
            &blob,
            &trust,
            &InstallRequest { allow_unsigned: true },
        )
        .unwrap();
        assert_eq!(installed.provenance, Provenance::UntrustedPublisher);
    }

    #[test]
    fn installing_twice_is_idempotent_and_says_the_blob_was_already_there() {
        let dir = Dir::new("twice");
        let (manifest, blob, trust) = signed_model(&dir.0, b"weights");
        let catalog = Catalog::new(dir.0.join("catalog"));

        assert!(
            !install(&catalog, &manifest, &blob, &trust, &InstallRequest::default())
                .unwrap()
                .already_present
        );
        assert!(
            install(&catalog, &manifest, &blob, &trust, &InstallRequest::default())
                .unwrap()
                .already_present
        );
    }

    #[test]
    fn a_failed_install_leaves_no_staging_files_behind() {
        // An interrupted install must not leave something that `weights_present` counts.
        let dir = Dir::new("clean");
        let (manifest, blob, trust) = signed_model(&dir.0, b"weights");
        let catalog = Catalog::new(dir.0.join("catalog"));
        let mut bad = manifest.clone();
        bad.blake3 = "0".repeat(64);
        let trust2 = {
            let mut m = bad.clone();
            let t = sign(&mut m, PUBLISHER);
            bad = m;
            t
        };

        assert!(install(&catalog, &bad, &blob, &trust2, &InstallRequest::default()).is_err());
        let _ = trust;

        catalog.ensure_layout().unwrap();
        let strays: Vec<_> = std::fs::read_dir(catalog.blob_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(strays.is_empty(), "blob directory should be empty: {strays:?}");
    }

    #[test]
    fn verifying_an_installed_model_notices_weights_swapped_afterwards() {
        // Install-time verification is not a promise about later. This is the method that
        // makes "check it again" possible, and the case it exists for.
        let dir = Dir::new("reverify");
        let (manifest, blob, trust) = signed_model(&dir.0, b"the real weights");
        let catalog = Catalog::new(dir.0.join("catalog"));
        install(&catalog, &manifest, &blob, &trust, &InstallRequest::default()).unwrap();

        let good = verify_installed(&catalog, &manifest.id).unwrap();
        assert!(good.digest_matches, "{good:?}");

        // Somebody with write access to the blob store replaces the weights.
        std::fs::write(catalog.blob_path(&manifest.blake3), b"tampered weights").unwrap();
        let bad = verify_installed(&catalog, &manifest.id).unwrap();
        assert!(!bad.digest_matches, "{bad:?}");
        assert_ne!(bad.actual.unwrap(), manifest.blake3);
    }

    #[test]
    fn verifying_a_model_whose_weights_were_never_downloaded_says_so() {
        let dir = Dir::new("noweights");
        let (manifest, _blob, _trust) = signed_model(&dir.0, b"weights");
        let catalog = Catalog::new(dir.0.join("catalog"));
        catalog.ensure_layout().unwrap();
        std::fs::write(
            catalog.manifest_dir().join(format!("{}.json", manifest.id)),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let v = verify_installed(&catalog, &manifest.id).unwrap();
        assert!(!v.weights_present);
        assert!(!v.digest_matches);
        assert_eq!(v.actual, None);
    }

    #[test]
    fn hashing_a_file_larger_than_one_chunk_is_correct() {
        // The loop is the kind of thing that works for small inputs and silently truncates
        // for large ones, so it gets an input bigger than the buffer.
        let dir = Dir::new("bigchunk");
        let body: Vec<u8> = (0..(HASH_CHUNK * 2 + 12345)).map(|i| (i % 251) as u8).collect();
        let path = dir.0.join("big.bin");
        std::fs::write(&path, &body).unwrap();
        assert_eq!(
            hash_file(&path).unwrap(),
            blake3::hash(&body).to_hex().to_string()
        );
    }

    #[test]
    fn hashing_an_empty_file_terminates() {
        let dir = Dir::new("empty");
        let path = dir.0.join("empty.bin");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(hash_file(&path).unwrap(), blake3::hash(b"").to_hex().to_string());
    }
}

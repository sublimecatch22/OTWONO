//! Who vouches for a model.
//!
//! `docs/ai/AI-RUNTIME.md` §5: "a model that can call tools is executable content". This
//! module is the check that makes that sentence enforceable rather than a warning.
//!
//! # Canonicalization
//!
//! A signature covers the manifest with its own `signature` field removed, serialized as
//! JSON with every object's keys sorted, no insignificant whitespace, prefixed with
//! [`MANIFEST_DOMAIN`](crate::manifest::MANIFEST_DOMAIN).
//!
//! The canonicalizer here is written out rather than delegated to `serde_json`'s default
//! map ordering. `serde_json` happens to sort keys today because it uses a `BTreeMap`, but
//! that is a consequence of the `preserve_order` feature being off — and any crate anywhere
//! in the dependency tree can turn that feature on. Signature verification silently
//! changing meaning because a transitive dependency enabled a feature is not a failure mode
//! worth leaving open.
//!
//! # Three outcomes, not two
//!
//! Signed-and-valid, unsigned, and *signed-but-wrong* are different, and collapsing the
//! last two would be a real weakness: a tampered manifest must never be treatable as merely
//! unsigned, because "unsigned" has an opt-in and tampering must not.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::manifest::{ModelManifest, MANIFEST_DOMAIN};

/// Ed25519 public keys this node accepts model signatures from.
#[derive(Debug, Clone, Default)]
pub struct PublisherTrust {
    /// Base64 public key to a human-readable name.
    keys: BTreeMap<String, String>,
}

impl PublisherTrust {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn with_key(mut self, public_key_base64: impl Into<String>, name: impl Into<String>) -> Self {
        self.keys.insert(public_key_base64.into(), name.into());
        self
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn name_for(&self, public_key_base64: &str) -> Option<&str> {
        self.keys.get(public_key_base64).map(String::as_str)
    }

    /// Load trusted publishers from a directory of `*.toml` files.
    ///
    /// ```toml
    /// [[publisher]]
    /// name = "OTWONO model catalog"
    /// public_key = "base64…"
    /// ```
    ///
    /// A missing directory yields an empty set, which trusts nobody. That is the correct
    /// default: a node with no configured publishers should refuse signed models from
    /// strangers, not accept them.
    pub fn load_dir(dir: &Path) -> Result<Self, TrustError> {
        let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "toml"))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::empty()),
            Err(e) => return Err(TrustError::Io(format!("{}: {e}", dir.display()))),
        };
        files.sort();

        let mut trust = Self::empty();
        for path in files {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| TrustError::Io(format!("{}: {e}", path.display())))?;
            let file: PublisherFile = toml::from_str(&text)
                .map_err(|e| TrustError::Malformed(format!("{}: {e}", path.display())))?;
            for p in file.publisher {
                // Reject a key that is not a well-formed Ed25519 public key at load time
                // rather than at first use: a typo in a trust store should be a startup
                // error, not a mysterious verification failure months later.
                decode_key(&p.public_key).map_err(|e| {
                    TrustError::Malformed(format!("{}: publisher {:?}: {e}", path.display(), p.name))
                })?;
                trust.keys.insert(p.public_key, p.name);
            }
        }
        Ok(trust)
    }
}

#[derive(Debug, serde::Deserialize)]
struct PublisherFile {
    #[serde(default)]
    publisher: Vec<PublisherEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct PublisherEntry {
    name: String,
    public_key: String,
}

/// What verification concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureStatus {
    /// No signature at all.
    Unsigned,
    /// Signature verifies and the publisher is in the trust store.
    Trusted { public_key: String, name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureError {
    /// The signature, key, or algorithm is not well formed.
    Malformed(String),
    /// The signature does not verify. The manifest has been altered since signing, or the
    /// signature was made over something else.
    BadSignature,
    /// The signature verifies, but this node does not trust the key that made it.
    UntrustedPublisher { public_key: String },
    /// The manifest could not be canonicalized.
    Canonicalization(String),
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignatureError::Malformed(e) => write!(f, "malformed model signature: {e}"),
            SignatureError::BadSignature => write!(
                f,
                "the model signature does not verify: the manifest has been altered since it \
                 was signed, or the signature was made over something else"
            ),
            SignatureError::UntrustedPublisher { public_key } => write!(
                f,
                "the model signature is valid but was made by an unknown publisher ({}); add \
                 the key to /etc/otwono/publishers.d to trust it",
                short(public_key)
            ),
            SignatureError::Canonicalization(e) => {
                write!(f, "cannot canonicalize the manifest for verification: {e}")
            }
        }
    }
}

impl std::error::Error for SignatureError {}

fn short(key: &str) -> String {
    key.chars().take(12).collect::<String>() + "…"
}

#[derive(Debug)]
pub enum TrustError {
    Io(String),
    Malformed(String),
}

impl std::fmt::Display for TrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrustError::Io(e) => write!(f, "{e}"),
            TrustError::Malformed(e) => write!(f, "malformed publisher trust store: {e}"),
        }
    }
}

impl std::error::Error for TrustError {}

impl ModelManifest {
    /// The exact bytes a signature covers.
    pub fn signing_message(&self) -> Result<Vec<u8>, SignatureError> {
        let mut value =
            serde_json::to_value(self).map_err(|e| SignatureError::Canonicalization(e.to_string()))?;
        if let Some(obj) = value.as_object_mut() {
            // The signature cannot cover itself.
            obj.remove("signature");
        }
        let mut message = MANIFEST_DOMAIN.to_vec();
        write_canonical(&value, &mut message);
        Ok(message)
    }

    /// Check the signature, if there is one, against the trust store.
    pub fn verify_signature(&self, trust: &PublisherTrust) -> Result<SignatureStatus, SignatureError> {
        let Some(sig) = &self.signature else {
            return Ok(SignatureStatus::Unsigned);
        };
        if sig.algorithm != "ed25519" {
            return Err(SignatureError::Malformed(format!(
                "unsupported algorithm {:?}; this build understands ed25519",
                sig.algorithm
            )));
        }
        let public_key = decode_key(&sig.public_key).map_err(SignatureError::Malformed)?;
        let signature = data_encoding::BASE64
            .decode(sig.signature.as_bytes())
            .map_err(|e| SignatureError::Malformed(format!("signature is not base64: {e}")))?;

        // Cryptography before trust. A tampered manifest is refused regardless of who
        // claims to have signed it, and the distinct error means an operator adding a key
        // to the trust store is never the fix for a broken signature.
        let message = self.signing_message()?;
        otwono_identity::verify_signature(&public_key, &message, &signature)
            .map_err(|_| SignatureError::BadSignature)?;

        match trust.name_for(&sig.public_key) {
            Some(name) => Ok(SignatureStatus::Trusted {
                public_key: sig.public_key.clone(),
                name: name.to_string(),
            }),
            None => Err(SignatureError::UntrustedPublisher {
                public_key: sig.public_key.clone(),
            }),
        }
    }
}

fn decode_key(base64: &str) -> Result<[u8; 32], String> {
    data_encoding::BASE64
        .decode(base64.as_bytes())
        .map_err(|e| format!("public key is not base64: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "an Ed25519 public key is 32 bytes".to_string())
}

/// Serialize `value` deterministically: object keys sorted, no insignificant whitespace.
fn write_canonical(value: &serde_json::Value, out: &mut Vec<u8>) {
    match value {
        serde_json::Value::Object(map) => {
            // Sorted explicitly. See the module docs: relying on serde_json's map type
            // would tie the meaning of every signature to a feature flag.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_json_string(key, out);
                out.push(b':');
                write_canonical(&map[*key], out);
            }
            out.push(b'}');
        }
        serde_json::Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        serde_json::Value::String(s) => write_json_string(s, out),
        // Numbers here are integers by construction (the manifest has no float fields),
        // so their textual form is unambiguous.
        other => out.extend_from_slice(other.to_string().as_bytes()),
    }
}

fn write_json_string(s: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(
        serde_json::to_string(s)
            .expect("a string always serializes")
            .as_bytes(),
    );
}

/// Signing helpers for tests in this crate and in others.
///
/// Public behind a feature rather than `#[cfg(test)]` because integration tests in other
/// crates need to produce genuinely signed manifests. A test that pasted a placeholder
/// signature would stop proving anything the moment verification became part of the path —
/// which is exactly what happened to this crate's own fixtures when it did.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;
    use crate::manifest::Signature;
    use otwono_identity::NodeIdentity;

    /// Sign `manifest` with a deterministic test key and return the trust store that
    /// accepts it.
    pub fn sign(manifest: &mut ModelManifest, seed: u8) -> PublisherTrust {
        let identity = NodeIdentity::from_seeds(&[seed; 32], &[seed; 32], 1);
        let public_key = data_encoding::BASE64.encode(&identity.public_key_bytes());
        manifest.signature = None;
        let message = manifest.signing_message().unwrap();
        manifest.signature = Some(Signature {
            algorithm: "ed25519".into(),
            public_key: public_key.clone(),
            signature: data_encoding::BASE64.encode(&identity.sign(&message).to_bytes()),
        });
        PublisherTrust::empty().with_key(public_key, format!("test publisher {seed}"))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::sign;
    use super::*;
    use crate::manifest::fixtures::*;

    #[test]
    fn a_signature_from_a_trusted_publisher_verifies() {
        let mut m = tiny();
        let trust = sign(&mut m, 1);
        assert_eq!(
            m.verify_signature(&trust).unwrap(),
            SignatureStatus::Trusted {
                public_key: m.signature.as_ref().unwrap().public_key.clone(),
                name: "test publisher 1".into(),
            }
        );
    }

    #[test]
    fn an_unsigned_manifest_is_reported_as_unsigned_not_as_an_error() {
        assert_eq!(
            tiny().verify_signature(&PublisherTrust::empty()).unwrap(),
            SignatureStatus::Unsigned
        );
    }

    #[test]
    fn altering_any_field_breaks_the_signature() {
        // The property the whole module exists for. Each of these is a field an attacker
        // would want to change: how much memory it claims to need, what it may do, and
        // which bytes are its weights.
        let mut m = medium();
        let trust = sign(&mut m, 2);
        assert!(m.verify_signature(&trust).is_ok());

        let mut cheaper = m.clone();
        cheaper.footprint.weights_bytes = 1;
        assert_eq!(
            cheaper.verify_signature(&trust),
            Err(SignatureError::BadSignature)
        );

        let mut lower_tier = m.clone();
        lower_tier.min_tier = otwono_capability::Tier::T0Micro;
        assert_eq!(
            lower_tier.verify_signature(&trust),
            Err(SignatureError::BadSignature)
        );

        let mut swapped_weights = m.clone();
        swapped_weights.blake3 = "e".repeat(64);
        assert_eq!(
            swapped_weights.verify_signature(&trust),
            Err(SignatureError::BadSignature)
        );

        let mut more_powers = m.clone();
        more_powers
            .capabilities
            .push(crate::manifest::ModelCapability::Tools);
        assert_eq!(
            more_powers.verify_signature(&trust),
            Err(SignatureError::BadSignature)
        );
    }

    #[test]
    fn a_valid_signature_from_an_unknown_publisher_is_its_own_outcome() {
        // Materially different from a broken signature: this one is intact, we just do not
        // know the signer. Adding the key is a sensible response; for BadSignature it
        // never is.
        let mut m = tiny();
        let _theirs = sign(&mut m, 3);
        let err = m.verify_signature(&PublisherTrust::empty()).unwrap_err();
        assert!(
            matches!(err, SignatureError::UntrustedPublisher { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("unknown publisher"), "{err}");
    }

    #[test]
    fn a_tampered_manifest_is_refused_even_if_the_signer_is_trusted() {
        let mut m = tiny();
        let trust = sign(&mut m, 4);
        m.parameters += 1;
        assert_eq!(m.verify_signature(&trust), Err(SignatureError::BadSignature));
    }

    #[test]
    fn a_signature_made_by_one_publisher_cannot_be_presented_under_anothers_key() {
        let mut m = tiny();
        let trust_a = sign(&mut m, 5);
        // Claim a different, also-trusted, publisher made it.
        let mut other = tiny();
        let trust_b = sign(&mut other, 6);
        let their_key = other.signature.as_ref().unwrap().public_key.clone();
        m.signature.as_mut().unwrap().public_key = their_key;

        let both = trust_a.with_key(
            other.signature.as_ref().unwrap().public_key.clone(),
            "publisher b",
        );
        let _ = trust_b;
        assert_eq!(m.verify_signature(&both), Err(SignatureError::BadSignature));
    }

    #[test]
    fn the_signature_does_not_cover_itself() {
        // Otherwise signing would be impossible. Verified by checking the message is
        // identical before and after the signature is attached.
        let mut m = tiny();
        let before = m.signing_message().unwrap();
        sign(&mut m, 7);
        assert_eq!(m.signing_message().unwrap(), before);
    }

    #[test]
    fn the_signing_message_is_domain_separated() {
        let m = tiny();
        let message = m.signing_message().unwrap();
        assert!(message.starts_with(MANIFEST_DOMAIN));
        // And the remainder is the canonical manifest, not something else.
        assert!(message[MANIFEST_DOMAIN.len()..].starts_with(b"{"));
    }

    #[test]
    fn canonicalization_does_not_depend_on_key_order() {
        // The failure this prevents: a manifest re-serialized by another tool, with the
        // same content in a different order, failing to verify.
        let mut m = tiny();
        let trust = sign(&mut m, 8);
        let shuffled: ModelManifest = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert!(shuffled.verify_signature(&trust).is_ok());
    }

    #[test]
    fn canonical_output_is_sorted_and_compact() {
        let value = serde_json::json!({ "z": 1, "a": { "y": [3, 2], "b": "x" } });
        let mut out = Vec::new();
        write_canonical(&value, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"a":{"b":"x","y":[3,2]},"z":1}"#,
            "keys sorted, array order preserved, no whitespace"
        );
    }

    #[test]
    fn a_malformed_signature_is_distinguished_from_a_wrong_one() {
        let mut m = tiny();
        let trust = sign(&mut m, 9);

        let mut bad_algorithm = m.clone();
        bad_algorithm.signature.as_mut().unwrap().algorithm = "rsa".into();
        assert!(matches!(
            bad_algorithm.verify_signature(&trust),
            Err(SignatureError::Malformed(_))
        ));

        let mut bad_key = m.clone();
        bad_key.signature.as_mut().unwrap().public_key = "not base64!!".into();
        assert!(matches!(
            bad_key.verify_signature(&trust),
            Err(SignatureError::Malformed(_))
        ));

        let mut short_key = m.clone();
        short_key.signature.as_mut().unwrap().public_key = data_encoding::BASE64.encode(b"tooshort");
        assert!(matches!(
            short_key.verify_signature(&trust),
            Err(SignatureError::Malformed(_))
        ));
    }

    #[test]
    fn an_empty_trust_store_trusts_nobody() {
        let t = PublisherTrust::empty();
        assert!(t.is_empty());
        assert_eq!(t.name_for("anything"), None);
    }

    #[test]
    fn a_missing_trust_directory_is_an_empty_store_not_an_error() {
        // A node that has configured no publishers should refuse signed models from
        // strangers, not fail to start.
        let t = PublisherTrust::load_dir(Path::new("/nonexistent/otwono/publishers.d")).unwrap();
        assert!(t.is_empty());
    }

    #[test]
    fn a_trust_store_loads_from_toml_and_rejects_a_malformed_key() {
        let dir = std::env::temp_dir().join(format!("otwono-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let good = data_encoding::BASE64.encode(&[7u8; 32]);
        std::fs::write(
            dir.join("10-good.toml"),
            format!("[[publisher]]\nname = \"Example\"\npublic_key = \"{good}\"\n"),
        )
        .unwrap();
        let t = PublisherTrust::load_dir(&dir).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t.name_for(&good), Some("Example"));

        // A typo in the trust store must fail at load, not at first verification.
        std::fs::write(
            dir.join("20-bad.toml"),
            "[[publisher]]\nname = \"Typo\"\npublic_key = \"AAAA\"\n",
        )
        .unwrap();
        let err = PublisherTrust::load_dir(&dir).unwrap_err();
        assert!(err.to_string().contains("32 bytes"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

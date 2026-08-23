//! Model manifests: what a node knows about a model before it loads one.
//!
//! The manifest is the contract (`schemas/model-manifest.schema.json`); the weights are a
//! separate content-addressed blob. Keeping them apart means a node can reason about a
//! model — refuse it, suggest a smaller one, show it greyed in a catalog — without having
//! downloaded gigabytes first.

use otwono_capability::Tier;
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: &str = "1.0.0";

/// Domain prefix for a manifest signature, distinct from every other signing context in
/// the system. Without it a signature over a manifest could be replayed as one over some
/// other JSON document the node signs.
pub const MANIFEST_DOMAIN: &[u8] = b"otwono-model-manifest-v1:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelFormat {
    Gguf,
    Onnx,
    Safetensors,
    Rknn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelCapability {
    Chat,
    Completion,
    Tools,
    Embedding,
    Vision,
    Asr,
    Tts,
}

/// What the model actually costs in resident memory.
///
/// Deliberately not derived from the parameter count. Quantization breaks that
/// relationship, and `size_bytes` is a file size — a memory-mapped GGUF still has to be
/// paged in to be used, so treating the file as free is how a node talks itself into a
/// load it cannot afford.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Footprint {
    pub weights_bytes: u64,
    /// KV cache per 1024 tokens of context, per sequence. This is the term that makes a
    /// model which "fits" fail once the conversation gets long.
    pub kv_per_1k_ctx_bytes: u64,
    /// Fixed runtime cost beyond weights and KV: compute buffers, backend allocations.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub overhead_bytes: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Footprint {
    /// Resident bytes required for `context_tokens` of context across `sequences`.
    ///
    /// Saturating throughout: a manifest is external data, and an overflow here would wrap
    /// a huge model into a small number and admit it. Saturating means an absurd manifest
    /// is refused, which is the safe direction.
    pub fn required_bytes(&self, context_tokens: u32, sequences: u32) -> u64 {
        let kv_units = u64::from(context_tokens).div_ceil(1024);
        let kv = self
            .kv_per_1k_ctx_bytes
            .saturating_mul(kv_units)
            .saturating_mul(u64::from(sequences.max(1)));
        self.weights_bytes
            .saturating_add(kv)
            .saturating_add(self.overhead_bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub algorithm: String,
    /// Base64 Ed25519 public key of the publisher.
    pub public_key: String,
    /// Base64 signature over the domain-separated canonical manifest.
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelManifest {
    pub schema_version: String,
    pub id: String,
    pub family: String,
    pub parameters: u64,
    pub quantization: String,
    pub format: ModelFormat,
    /// BLAKE3 content address of the weights blob.
    pub blake3: String,
    pub size_bytes: u64,
    pub min_tier: Tier,
    pub footprint: Footprint,
    pub max_context: u32,
    pub capabilities: Vec<ModelCapability>,
    pub license: String,
    pub backends: Vec<crate::BackendId>,
    /// Absent means unsigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Signature>,
}

impl ModelManifest {
    /// Structural checks that do not need the weights present.
    ///
    /// A manifest arrives from a catalog, a peer, or a file the user dropped in. Checking
    /// it before it reaches the admission calculation means a malformed one is a clear
    /// error rather than a strange refusal — or, worse, an admission based on a zero
    /// footprint.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ManifestError::UnsupportedSchema(self.schema_version.clone()));
        }
        if self.id.is_empty()
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        {
            return Err(ManifestError::BadId(self.id.clone()));
        }
        // Lowercase only, matching the schema. `is_ascii_hexdigit` would accept uppercase,
        // and a content address with two spellings is two names for one blob — enough to
        // break deduplication and to make a cache lookup miss a model already on disk.
        if self.blake3.len() != 64
            || !self
                .blake3
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ManifestError::BadContentAddress(self.blake3.clone()));
        }
        // A zero-weight model would sail through admission on any machine.
        if self.footprint.weights_bytes == 0 {
            return Err(ManifestError::ImplausibleFootprint(
                "weights_bytes is zero; nothing to load".into(),
            ));
        }
        if self.max_context == 0 {
            return Err(ManifestError::ImplausibleFootprint(
                "max_context is zero; the model can hold no input".into(),
            ));
        }
        if self.backends.is_empty() {
            return Err(ManifestError::NoBackendsDeclared);
        }
        if self.capabilities.is_empty() {
            return Err(ManifestError::NoCapabilitiesDeclared);
        }
        Ok(())
    }

    pub fn is_signed(&self) -> bool {
        self.signature.is_some()
    }

    pub fn supports(&self, capability: ModelCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// A model that can call tools is executable content, so an unsigned one is held to a
    /// different standard than an unsigned model that can only emit text.
    pub fn is_unsigned_and_tool_capable(&self) -> bool {
        !self.is_signed() && self.supports(ModelCapability::Tools)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    UnsupportedSchema(String),
    BadId(String),
    BadContentAddress(String),
    ImplausibleFootprint(String),
    NoBackendsDeclared,
    NoCapabilitiesDeclared,
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::UnsupportedSchema(v) => write!(
                f,
                "manifest schema_version {v:?} is not supported; this build understands {SCHEMA_VERSION}"
            ),
            ManifestError::BadId(id) => write!(
                f,
                "model id {id:?} must be lowercase alphanumerics with . _ or -; it is used as a path component"
            ),
            ManifestError::BadContentAddress(h) => {
                write!(f, "blake3 {h:?} is not 64 lowercase hex characters")
            }
            ManifestError::ImplausibleFootprint(why) => write!(f, "implausible manifest: {why}"),
            ManifestError::NoBackendsDeclared => {
                write!(f, "the manifest declares no backend that can execute it")
            }
            ManifestError::NoCapabilitiesDeclared => {
                write!(f, "the manifest declares no capabilities, so nothing could use it")
            }
        }
    }
}

impl std::error::Error for ManifestError {}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use crate::BackendId;

    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    pub fn manifest(id: &str, weights: u64, kv_per_1k: u64, min_tier: Tier) -> ModelManifest {
        ModelManifest {
            schema_version: SCHEMA_VERSION.to_string(),
            id: id.to_string(),
            family: "test".into(),
            parameters: 1_000_000_000,
            quantization: "Q4_K_M".into(),
            format: ModelFormat::Gguf,
            blake3: "a".repeat(64),
            size_bytes: weights,
            min_tier,
            footprint: Footprint {
                weights_bytes: weights,
                kv_per_1k_ctx_bytes: kv_per_1k,
                overhead_bytes: 128 * MIB,
            },
            max_context: 32768,
            capabilities: vec![ModelCapability::Chat],
            license: "apache-2.0".into(),
            backends: vec![BackendId::LlamaCppCpu],
            signature: None,
        }
    }

    /// ~1B at Q4: the kind of model a Pi 4 can actually run.
    pub fn tiny() -> ModelManifest {
        manifest("tiny-1b-q4", 700 * MIB, 30 * MIB, Tier::T1Edge)
    }

    /// ~8B at Q4: comfortable on a laptop, hopeless on a Pi Zero.
    pub fn medium() -> ModelManifest {
        manifest("medium-8b-q4", 5 * GIB, 130 * MIB, Tier::T2Balanced)
    }

    /// ~70B: needs a workstation.
    pub fn huge() -> ModelManifest {
        manifest("huge-70b-q4", 40 * GIB, 500 * MIB, Tier::T4Workstation)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn a_well_formed_manifest_validates() {
        assert_eq!(tiny().validate(), Ok(()));
    }

    #[test]
    fn context_length_changes_what_a_model_costs() {
        // The whole reason footprint is not a single number: the same model is affordable
        // at 4k context and not at 128k.
        let m = medium();
        let short = m.footprint.required_bytes(4096, 1);
        let long = m.footprint.required_bytes(131_072, 1);
        assert!(long > short * 2, "short {short} long {long}");
    }

    #[test]
    fn kv_cost_is_charged_per_sequence() {
        let m = medium();
        let one = m.footprint.required_bytes(8192, 1);
        let four = m.footprint.required_bytes(8192, 4);
        assert!(four > one);
        // Weights and overhead are shared; only KV multiplies.
        let kv_once = one - m.footprint.weights_bytes - m.footprint.overhead_bytes;
        assert_eq!(four - one, kv_once * 3);
    }

    #[test]
    fn a_partial_kv_block_is_charged_in_full() {
        // Rounding down would under-count and admit a model that then grows past its
        // budget on the first long turn.
        let f = Footprint {
            weights_bytes: 100,
            kv_per_1k_ctx_bytes: 1000,
            overhead_bytes: 0,
        };
        assert_eq!(f.required_bytes(1, 1), 1100, "one token still costs a block");
        assert_eq!(f.required_bytes(1024, 1), 1100);
        assert_eq!(f.required_bytes(1025, 1), 2100);
    }

    #[test]
    fn an_absurd_footprint_saturates_rather_than_wrapping() {
        // A manifest is external data. Wrapping would turn a colossal model into a small
        // number and admit it.
        let f = Footprint {
            weights_bytes: u64::MAX - 1,
            kv_per_1k_ctx_bytes: u64::MAX,
            overhead_bytes: u64::MAX,
        };
        assert_eq!(f.required_bytes(1_000_000, 64), u64::MAX);
    }

    #[test]
    fn a_zero_weight_manifest_is_refused() {
        // Otherwise it is admitted everywhere, including on a node with no memory to spare.
        let mut m = tiny();
        m.footprint.weights_bytes = 0;
        assert!(matches!(
            m.validate(),
            Err(ManifestError::ImplausibleFootprint(_))
        ));
    }

    #[test]
    fn an_id_that_could_escape_a_directory_is_refused() {
        // The id becomes a path component in the catalog.
        for bad in ["../etc/passwd", "Model", "a/b", "with space", ""] {
            let mut m = tiny();
            m.id = bad.to_string();
            assert!(
                matches!(m.validate(), Err(ManifestError::BadId(_))),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn a_malformed_content_address_is_refused() {
        for bad in ["", "xyz", &"A".repeat(64), &"a".repeat(63)] {
            let mut m = tiny();
            m.blake3 = bad.to_string();
            assert!(
                matches!(m.validate(), Err(ManifestError::BadContentAddress(_))),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn an_unknown_schema_version_is_refused_rather_than_guessed() {
        let mut m = tiny();
        m.schema_version = "2.0.0".into();
        assert!(matches!(m.validate(), Err(ManifestError::UnsupportedSchema(_))));
    }

    #[test]
    fn a_manifest_with_no_backend_is_refused() {
        let mut m = tiny();
        m.backends.clear();
        assert_eq!(m.validate(), Err(ManifestError::NoBackendsDeclared));
    }

    #[test]
    fn an_unsigned_tool_capable_model_is_flagged() {
        // A model that can call tools is executable content.
        let mut m = tiny();
        assert!(!m.is_unsigned_and_tool_capable(), "chat only");
        m.capabilities.push(ModelCapability::Tools);
        assert!(m.is_unsigned_and_tool_capable());
        m.signature = Some(Signature {
            algorithm: "ed25519".into(),
            public_key: "AA==".into(),
            signature: "AA==".into(),
        });
        assert!(
            !m.is_unsigned_and_tool_capable(),
            "signed is a different question"
        );
    }

    #[test]
    fn the_manifest_domain_is_distinct_from_every_other_signing_context() {
        let domain = String::from_utf8_lossy(MANIFEST_DOMAIN).to_string();
        for other in [
            "otwono-agreement-binding-v1:",
            "otwono-succession-v1:",
            "otwono-session-v1:",
            "otwono-application-v1:",
        ] {
            assert_ne!(domain, other);
            assert!(!other.starts_with(&domain));
            assert!(!domain.starts_with(other));
        }
    }

    #[test]
    fn a_manifest_round_trips_as_json() {
        let m = medium();
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<ModelManifest>(&json).unwrap(), m);
    }

    #[test]
    fn overhead_defaults_to_zero_when_absent() {
        let json = serde_json::json!({
            "weights_bytes": 100u64,
            "kv_per_1k_ctx_bytes": 10u64
        });
        let f: Footprint = serde_json::from_value(json).unwrap();
        assert_eq!(f.overhead_bytes, 0);
        assert_eq!(f.required_bytes(1024, 1), 110);
    }

    #[test]
    fn fixtures_span_the_range_the_tiers_care_about() {
        assert!(tiny().footprint.weights_bytes < 1024 * MIB);
        assert!(huge().footprint.weights_bytes > 32 * 1024 * MIB);
    }
}

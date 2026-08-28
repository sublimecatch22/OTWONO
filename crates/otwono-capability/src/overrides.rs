//! Operator overrides, read from `/etc/otwono/capability.override.toml`.
//!
//! Forcing a tier upward is allowed — it is the user's machine — but it is recorded and
//! warned about, and the detected values are preserved in the profile so a bug report shows
//! both what was found and what was forced.

use super::axes::{
    AcceleratorClass, CapabilityAxes, ComputeClass, MemoryClass, NetworkClass, PowerClass, StorageClass,
};
use super::Tier;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const DEFAULT_OVERRIDE_PATH: &str = "/etc/otwono/capability.override.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AxisOverrides {
    pub compute: Option<ComputeClass>,
    pub memory: Option<MemoryClass>,
    pub accelerator: Option<AcceleratorClass>,
    pub storage: Option<StorageClass>,
    pub network: Option<NetworkClass>,
    pub power: Option<PowerClass>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityOverrides {
    pub tier: Option<Tier>,
    #[serde(default)]
    pub axes: AxisOverrides,
}

impl CapabilityOverrides {
    /// Load overrides from a TOML file. A missing file is not an error — it is the normal
    /// case. A malformed file *is* an error, because silently ignoring it would leave the
    /// operator believing an override applied when it did not.
    pub fn load(path: &Path) -> Result<Self, OverrideError> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| OverrideError::Parse(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(OverrideError::Io(e.to_string())),
        }
    }

    pub fn changes_anything(&self) -> bool {
        self.tier.is_some() || self.axes != AxisOverrides::default()
    }

    pub(crate) fn apply_to_axes(&self, mut axes: CapabilityAxes) -> CapabilityAxes {
        if let Some(v) = self.axes.compute {
            axes.compute = v;
        }
        if let Some(v) = self.axes.memory {
            axes.memory = v;
        }
        if let Some(v) = self.axes.accelerator {
            axes.accelerator = v;
        }
        if let Some(v) = self.axes.storage {
            axes.storage = v;
        }
        if let Some(v) = self.axes.network {
            axes.network = v;
        }
        if let Some(v) = self.axes.power {
            axes.power = v;
        }
        axes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverrideError {
    Io(String),
    Parse(String),
}

impl std::fmt::Display for OverrideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverrideError::Io(e) => write!(f, "cannot read override file: {e}"),
            OverrideError::Parse(e) => write!(f, "malformed override file: {e}"),
        }
    }
}

impl std::error::Error for OverrideError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_an_empty_override_not_an_error() {
        let ov = CapabilityOverrides::load(Path::new("/nonexistent/override.toml")).unwrap();
        assert!(!ov.changes_anything());
    }

    #[test]
    fn parses_a_realistic_override_file() {
        let text = r#"
tier = "T3_CAPABLE"

[axes]
accelerator = "gpu_large"
"#;
        let ov: CapabilityOverrides = toml::from_str(text).unwrap();
        assert_eq!(ov.tier, Some(Tier::T3Capable));
        assert_eq!(ov.axes.accelerator, Some(AcceleratorClass::GpuLarge));
        assert_eq!(ov.axes.memory, None);
        assert!(ov.changes_anything());
    }

    #[test]
    fn a_malformed_override_is_an_error_not_a_silent_default() {
        let dir = std::env::temp_dir().join(format!("otwono-ov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.toml");
        std::fs::write(&p, "tier = \"NOT_A_TIER\"\n").unwrap();
        assert!(matches!(
            CapabilityOverrides::load(&p),
            Err(OverrideError::Parse(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn axis_overrides_replace_only_what_they_name() {
        let base = CapabilityAxes {
            compute: ComputeClass::Low,
            memory: MemoryClass::Low,
            accelerator: AcceleratorClass::None,
            storage: StorageClass::Standard,
            network: NetworkClass::Lan,
            power: PowerClass::Unconstrained,
        };
        let ov = CapabilityOverrides {
            tier: None,
            axes: AxisOverrides {
                accelerator: Some(AcceleratorClass::GpuLarge),
                ..Default::default()
            },
        };
        let out = ov.apply_to_axes(base.clone());
        assert_eq!(out.accelerator, AcceleratorClass::GpuLarge);
        assert_eq!(out.compute, base.compute);
        assert_eq!(out.memory, base.memory);
    }
}

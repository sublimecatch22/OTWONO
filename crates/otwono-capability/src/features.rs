//! Feature gates derived from the tier and the capability vector.
//!
//! This is the single place that answers "may this machine do X". No other subsystem may
//! re-derive it (CLAUDE.md §2.6).

use super::axes::{AcceleratorClass, CapabilityAxes, NetworkClass, PowerClass, StorageClass};
use super::Tier;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopProfile {
    /// No graphical session; CLI and the node services only.
    Headless,
    /// A minimal Wayland compositor, no heavyweight desktop environment.
    Light,
    /// A full desktop environment.
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureGates {
    /// May a local language model run at all?
    pub local_llm: bool,
    /// Largest model, in parameters, this machine should be offered. `None` at T0.
    pub max_model_parameters: Option<u64>,
    pub recommended_quantization: Option<String>,
    /// Local embeddings and a retrieval index over the user's own content.
    pub local_rag: bool,
    pub speech_to_text: bool,
    pub text_to_speech: bool,
    pub image_generation: bool,
    /// Concurrent agents the planner may run.
    pub max_concurrent_agents: u32,
    /// Node roles this hardware could take. Every one of them still requires the user to
    /// opt in — nothing that spends their bandwidth, disk or GPU turns on by itself.
    pub eligible_node_roles: Vec<String>,
    pub background_services: Vec<String>,
    pub desktop: DesktopProfile,
    /// Serve inference to authorized peers over the node mesh.
    pub serve_ai_to_peers: bool,
    /// Hold replicas of other nodes' `REPLICATED` content.
    pub content_replication: bool,
    /// Bytes this machine may contribute to the cluster cache (ADR-0015).
    ///
    /// A default, not a setting: an operator raises or lowers it, and no subsystem may
    /// infer its own (CLAUDE.md §2.6). Zero means "do not cache for peers at all", which is
    /// what a machine with no room to spare gets whatever its tier says.
    pub cluster_cache_bytes: u64,
}

/// The tier defaults from `CLUSTER-CACHE.md` §3.
///
/// T0 is deliberately non-zero. An 8 GB eMMC has little to spare, but a node that
/// contributes nothing is a node its neighbours cannot help either — 512 MiB is the
/// smallest slice that still lets a street's caches overlap.
pub const CACHE_BYTES_T0: u64 = 512 * 1024 * 1024;
pub const CACHE_BYTES_T1: u64 = 4 * 1024 * 1024 * 1024;
pub const CACHE_BYTES_T2: u64 = 32 * 1024 * 1024 * 1024;
pub const CACHE_BYTES_T3: u64 = 128 * 1024 * 1024 * 1024;
pub const CACHE_BYTES_T4: u64 = 128 * 1024 * 1024 * 1024;

impl FeatureGates {
    pub fn for_tier(tier: Tier, axes: &CapabilityAxes) -> Self {
        let mut g = match tier {
            Tier::T0Micro => FeatureGates {
                local_llm: false,
                max_model_parameters: None,
                recommended_quantization: None,
                local_rag: false,
                speech_to_text: false,
                text_to_speech: false,
                image_generation: false,
                max_concurrent_agents: 0,
                eligible_node_roles: vec!["leaf".into(), "relay".into()],
                background_services: vec!["otwono-idd".into(), "otwono-netd".into()],
                desktop: DesktopProfile::Headless,
                serve_ai_to_peers: false,
                content_replication: false,
                cluster_cache_bytes: CACHE_BYTES_T0,
            },
            Tier::T1Edge => FeatureGates {
                local_llm: true,
                max_model_parameters: Some(3_000_000_000),
                recommended_quantization: Some("Q4_K_M".into()),
                local_rag: false,
                speech_to_text: true,
                text_to_speech: true,
                image_generation: false,
                max_concurrent_agents: 1,
                eligible_node_roles: vec!["leaf".into(), "relay".into()],
                background_services: vec![
                    "otwono-idd".into(),
                    "otwono-netd".into(),
                    "otwono-aid".into(),
                    "otwono-stored".into(),
                ],
                desktop: DesktopProfile::Headless,
                serve_ai_to_peers: false,
                content_replication: false,
                cluster_cache_bytes: CACHE_BYTES_T1,
            },
            Tier::T2Balanced => FeatureGates {
                local_llm: true,
                max_model_parameters: Some(8_000_000_000),
                recommended_quantization: Some("Q4_K_M".into()),
                local_rag: true,
                speech_to_text: true,
                text_to_speech: true,
                image_generation: false,
                max_concurrent_agents: 3,
                eligible_node_roles: vec!["leaf".into(), "relay".into(), "cache".into()],
                background_services: vec![
                    "otwono-idd".into(),
                    "otwono-netd".into(),
                    "otwono-aid".into(),
                    "otwono-stored".into(),
                    "otwono-svcd".into(),
                ],
                desktop: DesktopProfile::Light,
                serve_ai_to_peers: false,
                content_replication: true,
                cluster_cache_bytes: CACHE_BYTES_T2,
            },
            Tier::T3Capable => FeatureGates {
                local_llm: true,
                max_model_parameters: Some(32_000_000_000),
                recommended_quantization: Some("Q4_K_M".into()),
                local_rag: true,
                speech_to_text: true,
                text_to_speech: true,
                image_generation: true,
                max_concurrent_agents: 6,
                eligible_node_roles: vec![
                    "leaf".into(),
                    "relay".into(),
                    "cache".into(),
                    "ai-provider".into(),
                ],
                background_services: vec![
                    "otwono-idd".into(),
                    "otwono-netd".into(),
                    "otwono-aid".into(),
                    "otwono-stored".into(),
                    "otwono-svcd".into(),
                ],
                desktop: DesktopProfile::Full,
                serve_ai_to_peers: true,
                content_replication: true,
                cluster_cache_bytes: CACHE_BYTES_T3,
            },
            Tier::T4Workstation => FeatureGates {
                local_llm: true,
                max_model_parameters: Some(70_000_000_000),
                recommended_quantization: Some("Q4_K_M".into()),
                local_rag: true,
                speech_to_text: true,
                text_to_speech: true,
                image_generation: true,
                max_concurrent_agents: 12,
                eligible_node_roles: vec![
                    "leaf".into(),
                    "relay".into(),
                    "cache".into(),
                    "ai-provider".into(),
                    "archive".into(),
                ],
                background_services: vec![
                    "otwono-idd".into(),
                    "otwono-netd".into(),
                    "otwono-aid".into(),
                    "otwono-stored".into(),
                    "otwono-svcd".into(),
                ],
                desktop: DesktopProfile::Full,
                serve_ai_to_peers: true,
                content_replication: true,
                cluster_cache_bytes: CACHE_BYTES_T4,
            },
        };

        apply_axis_adjustments(&mut g, axes);
        g
    }
}

/// Cross-axis corrections the tier alone cannot express.
fn apply_axis_adjustments(g: &mut FeatureGates, axes: &CapabilityAxes) {
    // Image generation is GPU work. A tier reached without one does not get it.
    if axes.accelerator < AcceleratorClass::GpuSmall {
        g.image_generation = false;
    }

    // Replication and archival need real free space, whatever the tier says.
    if axes.storage <= StorageClass::Constrained {
        g.content_replication = false;
        g.eligible_node_roles.retain(|r| r != "cache" && r != "archive");
        // And the cache goes with them. A full disk is a broken node
        // (CLUSTER-CACHE.md §3), so a machine with no room contributes nothing rather
        // than contributing until it dies.
        g.cluster_cache_bytes = 0;
    }

    // A gateway role requires the connectivity to be one.
    if axes.network >= NetworkClass::Gateway && !g.eligible_node_roles.iter().any(|r| r == "gateway") {
        g.eligible_node_roles.push("gateway".into());
    }
    // Offline machines cannot serve peers over the Internet, but they remain full local
    // nodes — that is the entire point of the project.
    if axes.network == NetworkClass::Offline {
        g.eligible_node_roles.retain(|r| r != "gateway");
    }

    // On battery, halve the agent budget and stop offering the machine as a peer provider.
    if axes.power == PowerClass::Constrained {
        g.max_concurrent_agents = (g.max_concurrent_agents / 2).max(if g.local_llm { 1 } else { 0 });
        g.serve_ai_to_peers = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::axes::{ComputeClass, MemoryClass};

    fn axes(
        accel: AcceleratorClass,
        storage: StorageClass,
        net: NetworkClass,
        power: PowerClass,
    ) -> CapabilityAxes {
        CapabilityAxes {
            compute: ComputeClass::Medium,
            memory: MemoryClass::Medium,
            accelerator: accel,
            storage,
            network: net,
            power,
        }
    }

    fn roomy() -> CapabilityAxes {
        axes(
            AcceleratorClass::GpuSmall,
            StorageClass::Bulk,
            NetworkClass::Lan,
            PowerClass::Unconstrained,
        )
    }

    #[test]
    fn the_cache_budget_grows_with_the_tier() {
        let a = roomy();
        let budgets: Vec<u64> = [Tier::T0Micro, Tier::T1Edge, Tier::T2Balanced, Tier::T3Capable]
            .iter()
            .map(|t| FeatureGates::for_tier(*t, &a).cluster_cache_bytes)
            .collect();
        assert_eq!(
            budgets,
            vec![CACHE_BYTES_T0, CACHE_BYTES_T1, CACHE_BYTES_T2, CACHE_BYTES_T3],
            "the defaults must match CLUSTER-CACHE.md §3"
        );
        assert!(budgets.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn even_the_smallest_tier_contributes_something() {
        // A node that contributes nothing is a node its neighbours cannot help either.
        let g = FeatureGates::for_tier(Tier::T0Micro, &roomy());
        assert_eq!(g.cluster_cache_bytes, 512 * 1024 * 1024);
    }

    #[test]
    fn a_machine_with_no_room_caches_nothing_whatever_its_tier() {
        // A full disk is a broken node. Storage overrides the tier, in the one place that
        // is allowed to decide this.
        for tier in [Tier::T0Micro, Tier::T2Balanced, Tier::T4Workstation] {
            let g = FeatureGates::for_tier(
                tier,
                &axes(
                    AcceleratorClass::GpuSmall,
                    StorageClass::Constrained,
                    NetworkClass::Lan,
                    PowerClass::Unconstrained,
                ),
            );
            assert_eq!(g.cluster_cache_bytes, 0, "{tier:?} on a constrained disk");
            assert!(!g.content_replication);
        }
    }

    #[test]
    fn t0_promises_no_llm() {
        let g = FeatureGates::for_tier(
            Tier::T0Micro,
            &axes(
                AcceleratorClass::None,
                StorageClass::Standard,
                NetworkClass::Lan,
                PowerClass::Unconstrained,
            ),
        );
        assert!(!g.local_llm);
        assert_eq!(g.max_model_parameters, None);
        assert_eq!(g.max_concurrent_agents, 0);
        assert_eq!(g.desktop, DesktopProfile::Headless);
    }

    #[test]
    fn image_generation_requires_a_gpu_even_at_high_tiers() {
        let g = FeatureGates::for_tier(
            Tier::T3Capable,
            &axes(
                AcceleratorClass::Igpu,
                StorageClass::Fast,
                NetworkClass::Broadband,
                PowerClass::Unconstrained,
            ),
        );
        assert!(!g.image_generation);
    }

    #[test]
    fn constrained_storage_disables_replication_and_cache_roles() {
        let g = FeatureGates::for_tier(
            Tier::T4Workstation,
            &axes(
                AcceleratorClass::GpuLarge,
                StorageClass::Constrained,
                NetworkClass::Broadband,
                PowerClass::Unconstrained,
            ),
        );
        assert!(!g.content_replication);
        assert!(!g.eligible_node_roles.contains(&"cache".to_string()));
        assert!(!g.eligible_node_roles.contains(&"archive".to_string()));
    }

    #[test]
    fn gateway_role_appears_only_with_gateway_connectivity() {
        let with = FeatureGates::for_tier(
            Tier::T2Balanced,
            &axes(
                AcceleratorClass::None,
                StorageClass::Fast,
                NetworkClass::Gateway,
                PowerClass::Unconstrained,
            ),
        );
        assert!(with.eligible_node_roles.contains(&"gateway".to_string()));

        let without = FeatureGates::for_tier(
            Tier::T2Balanced,
            &axes(
                AcceleratorClass::None,
                StorageClass::Fast,
                NetworkClass::Lan,
                PowerClass::Unconstrained,
            ),
        );
        assert!(!without.eligible_node_roles.contains(&"gateway".to_string()));
    }

    #[test]
    fn an_offline_node_is_still_a_full_local_node() {
        let g = FeatureGates::for_tier(
            Tier::T2Balanced,
            &axes(
                AcceleratorClass::None,
                StorageClass::Fast,
                NetworkClass::Offline,
                PowerClass::Unconstrained,
            ),
        );
        assert!(g.local_llm, "offline must not disable local AI");
        assert!(g.local_rag);
        assert!(g.eligible_node_roles.contains(&"leaf".to_string()));
        assert!(!g.eligible_node_roles.contains(&"gateway".to_string()));
    }

    #[test]
    fn battery_reduces_agents_but_never_below_one_when_llm_is_available() {
        let g = FeatureGates::for_tier(
            Tier::T3Capable,
            &axes(
                AcceleratorClass::GpuSmall,
                StorageClass::Fast,
                NetworkClass::Broadband,
                PowerClass::Constrained,
            ),
        );
        assert_eq!(g.max_concurrent_agents, 3);
        assert!(!g.serve_ai_to_peers, "a laptop on battery must not serve peers");

        let t1 = FeatureGates::for_tier(
            Tier::T1Edge,
            &axes(
                AcceleratorClass::None,
                StorageClass::Standard,
                NetworkClass::Lan,
                PowerClass::Constrained,
            ),
        );
        assert_eq!(
            t1.max_concurrent_agents, 1,
            "1/2 rounds to 0, which must be clamped to 1"
        );
    }

    #[test]
    fn model_size_grows_monotonically_with_tier() {
        let a = axes(
            AcceleratorClass::GpuLarge,
            StorageClass::Bulk,
            NetworkClass::Broadband,
            PowerClass::Unconstrained,
        );
        let sizes: Vec<u64> = Tier::ALL
            .iter()
            .filter_map(|t| FeatureGates::for_tier(*t, &a).max_model_parameters)
            .collect();
        assert!(
            sizes.windows(2).all(|w| w[0] < w[1]),
            "{sizes:?} must be increasing"
        );
    }
}

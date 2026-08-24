//! Visibility labels, which fail closed.
//!
//! Four labels, defined in `docs/security/DATA-VISIBILITY.md` and normative in CLAUDE.md
//! §8. Two rules matter more than the rest and are enforced by the types here rather than
//! by remembering:
//!
//! 1. **The default is `Private`.** Not "unset", not "unknown" — a value that has to be
//!    deliberately widened.
//! 2. **A missing or unparseable label is `Private`.** A corrupt record, a truncated write,
//!    a field from a newer version this node does not understand: every one of those reads
//!    as the most restrictive answer. The alternative is that damage to a file makes its
//!    contents *more* available, which is precisely backwards.
//!
//! Deserialization is deliberately infallible for that reason. There is no way to get an
//! error out of reading a label, because an error is a decision a caller might get wrong.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Visibility {
    /// Local-only unless explicitly exported. Never leaves the node on its own.
    #[default]
    Private,
    /// Available to explicitly authorized users or nodes.
    Shared,
    /// Available to other nodes according to network policy.
    Public,
    /// Explicitly permitted to be copied to other nodes for availability.
    Replicated,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Shared => "shared",
            Visibility::Public => "public",
            Visibility::Replicated => "replicated",
        }
    }

    /// Parse, falling back to `Private` for anything not recognised.
    ///
    /// Infallible on purpose — see the module docs. Use [`Visibility::parse_strict`] where a
    /// human typed the value and should be told they typed it wrong.
    pub fn parse(s: &str) -> Visibility {
        Visibility::parse_strict(s).unwrap_or(Visibility::Private)
    }

    /// Parse, reporting an unrecognised value. For configuration files and CLI arguments,
    /// where silently treating a typo as `Private` would confuse rather than protect.
    pub fn parse_strict(s: &str) -> Option<Visibility> {
        match s.trim().to_ascii_lowercase().as_str() {
            "private" => Some(Visibility::Private),
            "shared" => Some(Visibility::Shared),
            "public" => Some(Visibility::Public),
            "replicated" => Some(Visibility::Replicated),
            _ => None,
        }
    }

    /// May an object with this label be served to another node at all?
    ///
    /// `Shared` is false here: it needs a per-peer authorization decision that this type
    /// has no way to make, and answering "maybe" as "yes" is how data leaks.
    pub fn may_leave_the_node_unattended(self) -> bool {
        matches!(self, Visibility::Public | Visibility::Replicated)
    }

    /// May an object with this label enter the shared neighbourhood cache?
    ///
    /// The rule from ADR-0015, in one place so no caller re-derives it.
    pub fn may_be_cached_for_peers(self) -> bool {
        matches!(self, Visibility::Public | Visibility::Replicated)
    }

    /// Is `self` at least as restrictive as `other`?
    pub fn is_at_least_as_restrictive_as(self, other: Visibility) -> bool {
        self <= other
    }

    /// The label derived content must carry.
    ///
    /// Content computed from several inputs inherits the **most restrictive** of them:
    /// a summary of a private note is private, and a thumbnail of a shared photo is
    /// shared. Getting this backwards would let derivation launder a label, which is the
    /// most likely way a system like this leaks without anyone deciding to.
    pub fn most_restrictive(inputs: impl IntoIterator<Item = Visibility>) -> Visibility {
        inputs.into_iter().min().unwrap_or(Visibility::Private)
    }
}

impl std::fmt::Display for Visibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Visibility {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Visibility {
    /// Never fails. A label that cannot be read is `Private`, because the alternative is
    /// that corrupting a record makes its contents more available.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Visibility, D::Error> {
        let raw = serde_json::Value::deserialize(d)?;
        Ok(raw.as_str().map(Visibility::parse).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_private() {
        assert_eq!(Visibility::default(), Visibility::Private);
    }

    #[test]
    fn every_label_round_trips() {
        for v in [
            Visibility::Private,
            Visibility::Shared,
            Visibility::Public,
            Visibility::Replicated,
        ] {
            assert_eq!(Visibility::parse(v.as_str()), v);
            let json = serde_json::to_string(&v).expect("serialize");
            assert_eq!(serde_json::from_str::<Visibility>(&json).expect("deserialize"), v);
        }
    }

    #[test]
    fn anything_unrecognised_reads_as_private() {
        // The rule that makes damage safe: a truncated write, a field from a newer version,
        // or an outright lie all land on the most restrictive answer.
        for bad in ["", "PUBLIC-ish", "world-readable", "prívate", "1", "null", "  "] {
            assert_eq!(
                Visibility::parse(bad),
                Visibility::Private,
                "{bad:?} must read as private"
            );
        }
    }

    #[test]
    fn a_label_of_the_wrong_json_type_reads_as_private_rather_than_failing() {
        // Deserialization cannot error, so a caller has no error path to get wrong.
        for json in ["null", "42", "true", r#"["public"]"#, r#"{"label":"public"}"#] {
            assert_eq!(
                serde_json::from_str::<Visibility>(json).expect("must not fail"),
                Visibility::Private,
                "{json} must read as private"
            );
        }
    }

    #[test]
    fn case_and_whitespace_do_not_change_the_meaning() {
        assert_eq!(Visibility::parse("  PUBLIC  "), Visibility::Public);
        assert_eq!(Visibility::parse("Replicated"), Visibility::Replicated);
    }

    #[test]
    fn strict_parsing_reports_a_typo_instead_of_hiding_it() {
        // A human editing a config should be told, not quietly protected.
        assert_eq!(Visibility::parse_strict("public"), Some(Visibility::Public));
        assert_eq!(Visibility::parse_strict("publik"), None);
    }

    #[test]
    fn only_public_and_replicated_leave_the_node_by_themselves() {
        assert!(!Visibility::Private.may_leave_the_node_unattended());
        // Shared needs a per-peer decision this type cannot make. Answering "maybe" as
        // "yes" is how data leaks.
        assert!(!Visibility::Shared.may_leave_the_node_unattended());
        assert!(Visibility::Public.may_leave_the_node_unattended());
        assert!(Visibility::Replicated.may_leave_the_node_unattended());
    }

    #[test]
    fn the_cache_rule_matches_the_egress_rule() {
        // ADR-0015 restricts the neighbourhood cache to exactly what may be served
        // unattended. If these ever diverge, one of them is a leak.
        for v in [
            Visibility::Private,
            Visibility::Shared,
            Visibility::Public,
            Visibility::Replicated,
        ] {
            assert_eq!(v.may_be_cached_for_peers(), v.may_leave_the_node_unattended());
        }
    }

    #[test]
    fn derived_content_inherits_the_most_restrictive_input() {
        // The property test from DATA-VISIBILITY.md Section 6, exhaustively rather than
        // randomly: four labels, every pair and triple.
        let all = [
            Visibility::Private,
            Visibility::Shared,
            Visibility::Public,
            Visibility::Replicated,
        ];
        for a in all {
            for b in all {
                let derived = Visibility::most_restrictive([a, b]);
                assert!(
                    derived.is_at_least_as_restrictive_as(a) && derived.is_at_least_as_restrictive_as(b),
                    "{a} + {b} derived {derived}, which is looser than an input"
                );
                for c in all {
                    let d3 = Visibility::most_restrictive([a, b, c]);
                    assert!(d3 <= a.min(b).min(c), "{a}+{b}+{c} derived {d3}");
                }
            }
        }
    }

    #[test]
    fn deriving_from_nothing_is_private() {
        // A derivation with no inputs is a bug in the caller. It must not be the loosest
        // label by default.
        assert_eq!(Visibility::most_restrictive([]), Visibility::Private);
    }

    #[test]
    fn the_ordering_runs_from_most_to_least_restrictive() {
        // most_restrictive relies on this being the derive order of the enum. Asserted so
        // that reordering the variants breaks a test rather than the security model.
        assert!(Visibility::Private < Visibility::Shared);
        assert!(Visibility::Shared < Visibility::Public);
        assert!(Visibility::Public < Visibility::Replicated);
    }
}

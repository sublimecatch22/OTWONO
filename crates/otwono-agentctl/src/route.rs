//! Which daemon serves a given method.
//!
//! # Why this is not a prefix split
//!
//! It looks like one: `store.*` to the store, `hw.*` to the hardware daemon, `net.*` to the
//! mesh daemon. But `cache.*` is served by **`otwono-stored`** — the cluster cache is a
//! second store inside that daemon, not a daemon of its own (ADR-0015). A prefix rule that
//! derived the socket name from the method's first component would send `cache.status` to
//! `/run/otwono/cache.sock`, which nothing listens on, and the assistant would fail with a
//! connection error for a method that works perfectly.
//!
//! So the map is written out, and the one entry that surprises people carries the reason.

use std::path::PathBuf;

/// The service whose socket serves `method`, or `None` if nothing here does.
///
/// Returns the service *name*, not a path, so the caller can override the socket directory
/// for a test without this module knowing anything about how sockets are located.
pub fn service_for(method: &str) -> Option<&'static str> {
    match method.split('.').next()? {
        "store" => Some("store"),
        // Not "cache". The cluster cache lives inside otwono-stored (ADR-0015), so its
        // methods arrive on the store socket. This is the entry a prefix rule gets wrong.
        "cache" => Some("store"),
        "hw" => Some("hw"),
        "net" => Some("net"),
        _ => None,
    }
}

/// Where that service listens, honouring `OTWONO_SOCKET_DIR`.
pub fn socket_for(method: &str) -> Option<PathBuf> {
    service_for(method).map(otwono_proto::socket_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_methods_go_to_the_store_daemon() {
        // The whole reason this module exists rather than a `split('.')`.
        assert_eq!(service_for("cache.status"), Some("store"));
        assert_eq!(service_for("cache.put"), Some("store"));
        assert_eq!(service_for("store.put"), Some("store"));
    }

    #[test]
    fn the_other_prefixes_go_where_they_look_like_they_go() {
        assert_eq!(service_for("hw.tier"), Some("hw"));
        assert_eq!(service_for("hw.profile"), Some("hw"));
        assert_eq!(service_for("net.peers"), Some("net"));
    }

    #[test]
    fn an_unroutable_method_is_none_rather_than_a_guess() {
        // A guess would produce a socket path nothing listens on, and the user would see a
        // connection error for what is really a missing route.
        assert_eq!(service_for("wallet.read"), None);
        assert_eq!(service_for("nonsense"), None);
        assert_eq!(service_for(""), None);
    }

    /// Every verb the grammar offers must be routable.
    ///
    /// The grammar checks that a verb names a method some daemon serves; this checks that
    /// *this binary* knows where to send it. Both are needed: a verb can name a real method
    /// and still be undeliverable, which fails at the last step with the user having done
    /// everything right.
    #[test]
    fn every_verb_in_the_grammar_can_be_routed() {
        use otwono_agent::{AssistantShape, Grammar};
        let grammar = Grammar::for_shape(AssistantShape::CommandGrammar);
        assert!(!grammar.verbs().is_empty(), "an empty table proves nothing");
        for verb in grammar.verbs() {
            assert!(
                service_for(verb.method).is_some(),
                "verb \"{}\" names {}, which this binary cannot route to any daemon",
                verb.word,
                verb.method
            );
        }
    }
}

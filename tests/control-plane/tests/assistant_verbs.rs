//! Every verb the T0 assistant offers must name a method a daemon really serves.
//!
//! `otwono-agent` cannot check this itself. It is a library that deliberately depends on
//! nothing but the capability engine — an assistant that linked every daemon in order to
//! validate its own vocabulary would be an assistant you could not unit-test without the
//! whole system. So the crate's own test compares the verb table against a hand-written
//! list of method names, which catches a typo inside the crate and nothing else.
//!
//! A hand-written list is exactly the shape that goes stale. A method renamed in
//! `otwono-stored` would leave the grammar pointing at nothing, the in-crate test would
//! still pass because it agrees with itself, and the failure would surface as an assistant
//! that parses a sentence perfectly and then dies at dispatch — the worst place, because
//! the user did everything right.
//!
//! This test closes that by asking the daemons. `describe` is public on every OTWONO
//! service and returns the methods it serves with the capability each needs, so the
//! comparison is against the running truth rather than against a copy of it.

use otwono_agent::{AssistantShape, Grammar};
use otwono_permd::{ActionRegistry, AuditLog, Broker, Policy};
use otwono_proto::{Client, Server, Shutdown};
use otwono_store::{StorageKey, Store};
use otwono_stored::StoreService;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Stand up the daemons whose methods the grammar names, and ask each what it serves.
///
/// Only `describe` is called, which needs no capability and no policy beyond a broker
/// existing — so this test says nothing about whether the assistant may *do* any of it.
/// That is the right split: this is a vocabulary check, and authorization is checked where
/// authorization lives.
struct Daemons {
    dir: PathBuf,
    methods: BTreeMap<String, Option<String>>,
    shutdown: Shutdown,
}

impl Daemons {
    fn start(tag: &str) -> Daemons {
        // Per test, not per process: these run in parallel and would otherwise race for one
        // socket path, which fails as a bind error that looks nothing like the cause.
        let dir = std::env::temp_dir().join(format!("otw-verbs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::write(dir.join("policy.d/10.toml"), "").unwrap();
        let shutdown = Shutdown::new();

        let perm_socket = dir.join("perm.sock");
        let policy = Policy::load_dir(&dir.join("policy.d")).unwrap();
        policy.validate(&ActionRegistry::builtin()).unwrap();
        let broker = Arc::new(Broker::new(
            policy,
            AuditLog::open(dir.join("audit.jsonl")).unwrap(),
        ));
        let s = shutdown.clone();
        let server = Server::bind(&perm_socket).unwrap();
        std::thread::spawn(move || server.serve(broker, s));

        let store_socket = dir.join("store.sock");
        let store = Store::encrypted(dir.join("store"), StorageKey::generate());
        store.ensure_layout().unwrap();
        let cache = otwono_store::Cache::at(dir.join("cache"), StorageKey::generate(), 1 << 20).unwrap();
        let service = Arc::new(StoreService::new(store, perm_socket.clone()).with_cache(cache));
        let s = shutdown.clone();
        let server = Server::bind(&store_socket).unwrap();
        std::thread::spawn(move || server.serve(service, s));

        let hw_socket = dir.join("hw.sock");
        // Probes `/` because only `describe` is called and it reads no hardware; a fixture
        // root would be scaffolding for a question this test never asks.
        let hw = Arc::new(otwono_hwd::HwService::new(
            PathBuf::from("/"),
            perm_socket.clone(),
            Default::default(),
        ));
        let s = shutdown.clone();
        let server = Server::bind(&hw_socket).unwrap();
        std::thread::spawn(move || server.serve(hw, s));

        let net_socket = dir.join("net.sock");
        let identity = otwono_identity::NodeIdentity::generate().unwrap();
        let state = Arc::new(otwono_netd::NetState::new(Arc::new(identity)));
        let net = Arc::new(otwono_netd::NetService::new(state, perm_socket.clone()));
        let s = shutdown.clone();
        let server = Server::bind(&net_socket).unwrap();
        std::thread::spawn(move || server.serve(net, s));

        let mut methods = BTreeMap::new();
        for sock in [&store_socket, &hw_socket, &net_socket] {
            let mut client = Client::connect_waiting(sock, Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{} never came up", sock.display()));
            let described = client
                .describe()
                .unwrap()
                .expect("describe must not need a token");
            for m in described.methods {
                methods.insert(m.name, m.capability);
            }
        }
        Daemons {
            dir,
            methods,
            shutdown,
        }
    }
}

impl Drop for Daemons {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn every_t0_verb_names_a_method_a_daemon_actually_serves() {
    let daemons = Daemons::start("serves");
    let grammar = Grammar::for_shape(AssistantShape::CommandGrammar);

    // A grammar with no verbs would pass every assertion below.
    assert!(
        grammar.verbs().len() >= 8,
        "the verb table has shrunk to {}; this test would pass vacuously",
        grammar.verbs().len()
    );

    for verb in grammar.verbs() {
        let described = daemons.methods.get(verb.method).unwrap_or_else(|| {
            panic!(
                "verb \"{}\" maps to {}, which no daemon describes. Known: {:?}",
                verb.word,
                verb.method,
                daemons.methods.keys().collect::<Vec<_>>()
            )
        });

        // And the capability the grammar tells the user they need is the one the daemon
        // will actually check. A mismatch here sends someone to request the wrong grant,
        // which fails in a way that looks like the policy is broken rather than the message.
        assert_eq!(
            verb.capability,
            described.as_deref(),
            "verb \"{}\" ({}) says it needs {:?}, the daemon checks {:?}",
            verb.word,
            verb.method,
            verb.capability,
            described
        );
    }
}

/// The assistant may not name a method that needs no capability at all.
///
/// Not a hypothetical tidiness rule. `describe` is unauthenticated on the local socket by
/// design, and a verb pointing at an open method would be the one call in the table that
/// bypasses the broker — the assistant's whole safety argument is that it can only name
/// things the broker already gates.
#[test]
fn no_verb_reaches_an_unguarded_method() {
    let daemons = Daemons::start("guarded");
    for verb in Grammar::for_shape(AssistantShape::CommandGrammar).verbs() {
        assert!(
            verb.capability.is_some(),
            "verb \"{}\" declares no capability",
            verb.word
        );
        assert_eq!(
            daemons.methods.get(verb.method),
            Some(&verb.capability.map(str::to_string)),
            "verb \"{}\" reaches {} which the daemon leaves open",
            verb.word,
            verb.method
        );
    }
}

/// Every capability a verb names is one the permission broker registers.
///
/// The broker validates policy against this registry, so a capability the grammar invents
/// could never be granted — the user would be told to hold something no policy file can
/// name.
#[test]
fn every_capability_a_verb_names_is_one_the_broker_knows() {
    let registry = ActionRegistry::builtin();
    for verb in Grammar::for_shape(AssistantShape::CommandGrammar).verbs() {
        let Some(cap) = verb.capability else { continue };
        assert!(
            registry.get(cap).is_some(),
            "verb \"{}\" needs capability {cap}, which the broker does not register",
            verb.word
        );
    }
}

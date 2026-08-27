//! `otwono do` against real daemons, run as a real process.
//!
//! Everything else about the assistant is tested as a function call. This runs the actual
//! binary with `std::process::Command`, against a broker and daemons on a temp socket
//! directory, and reads what a person would see on their terminal.
//!
//! That distinction matters more here than usual, because most of what this binary does is
//! *arrange* things — locate sockets, request a token, shape the output, choose an exit
//! code. None of that is exercised by calling a function; all of it is what breaks.

use otwono_permd::{ActionRegistry, AuditLog, Broker, Policy};
use otwono_proto::{Server, Shutdown};
use otwono_store::{Cache, StorageKey, Store};
use otwono_stored::StoreService;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::Duration;

/// A policy that allows what the verbs need, and nothing else.
///
/// `store.serve` and `net.content` are absent on purpose: the assistant must not be able to
/// reach the network boundary just because it can reach the store.
const POLICY: &str = r#"
[[rule]]
action = "hw.read"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "store.write"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "store.read"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "cache.read"
decision = "allow"
ttl_seconds = 300
"#;

struct Node {
    dir: PathBuf,
    shutdown: Shutdown,
}

impl Node {
    fn start(tag: &str, policy: &str) -> Node {
        Node::start_as(tag, policy, otwono_capability::Tier::T0Micro)
    }

    /// Stand up a node that reports `tier`, whatever machine this actually is.
    ///
    /// Pinned rather than probed, and that is the whole point. An earlier version let the
    /// hardware daemon classify the host, so the assistant's shape — and therefore every
    /// message it produced — depended on what the tests happened to run on. They passed on a
    /// T0_MICRO development box and failed on a larger CI runner, which is the failure mode
    /// CLAUDE.md §6 exists to prevent: a test that reads the machine under it is testing the
    /// machine.
    fn start_as(tag: &str, policy: &str, tier: otwono_capability::Tier) -> Node {
        let dir = std::env::temp_dir().join(format!("otw-do-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::write(dir.join("policy.d/10.toml"), policy).unwrap();
        let shutdown = Shutdown::new();
        let perm_socket = dir.join("perm.sock");

        let loaded = Policy::load_dir(&dir.join("policy.d")).expect("policy loads");
        loaded
            .validate(&ActionRegistry::builtin())
            .expect("the test policy names only registered actions");
        let broker = Arc::new(Broker::new(
            loaded,
            AuditLog::open(dir.join("audit.jsonl")).unwrap(),
        ));
        let s = shutdown.clone();
        let srv = Server::bind(&perm_socket).unwrap();
        std::thread::spawn(move || srv.serve(broker, s));

        let store = Store::encrypted(dir.join("store"), StorageKey::generate());
        store.ensure_layout().unwrap();
        let cache = Cache::at(dir.join("cache"), StorageKey::generate(), 1 << 20).unwrap();
        let service = Arc::new(StoreService::new(store, perm_socket.clone()).with_cache(cache));
        let s = shutdown.clone();
        let srv = Server::bind(dir.join("store.sock")).unwrap();
        std::thread::spawn(move || srv.serve(service, s));

        // A captured fixture tree, never `/`, following the pattern authorization.rs already
        // sets: nothing about these tests may read the machine they run on. The tier is then
        // pinned on top, because the shape under test is derived from the tier and the
        // fixtures do not include a T0-class machine. The override is a shipped mechanism
        // for exactly "I know better than the probe", so this uses it rather than inventing
        // a test-only path into the daemon.
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../otwono-hal/tests/fixtures/x86_64-cloud-vm");
        let hw = Arc::new(otwono_hwd::HwService::new(
            fixture,
            perm_socket.clone(),
            otwono_capability::CapabilityOverrides {
                tier: Some(tier),
                ..Default::default()
            },
        ));
        let s = shutdown.clone();
        let srv = Server::bind(dir.join("hw.sock")).unwrap();
        std::thread::spawn(move || srv.serve(hw, s));

        for sock in ["perm.sock", "store.sock", "hw.sock"] {
            otwono_proto::Client::connect_waiting(dir.join(sock), Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{sock} never came up"));
        }
        Node { dir, shutdown }
    }

    /// Run the real binary, as a person would.
    fn otwono(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_otwono"))
            .args(args)
            .arg("--socket-dir")
            .arg(&self.dir)
            .arg("--perm-socket")
            .arg(self.dir.join("perm.sock"))
            .output()
            .expect("the otwono binary runs")
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

#[test]
fn a_verb_reaches_the_daemon_and_comes_back_with_an_answer() {
    let node = Node::start("tier", POLICY);
    let out = node.otwono(&["do", "tier"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    // The tier of whatever machine runs this test, so the assertion is on the shape of the
    // answer rather than its value.
    assert!(text.contains("tier (hw.tier)"), "{text}");
    assert!(
        text.contains("T0_")
            || text.contains("T1_")
            || text.contains("T2_")
            || text.contains("T3_")
            || text.contains("T4_"),
        "{text}"
    );
}

/// A file goes in through the assistant and can be read back by its id.
///
/// The whole round trip: words to intent, token from the broker, base64 over the control
/// plane, content-addressed back out. This is where the hand-written base64 would show up
/// as corruption if it were wrong.
#[test]
fn saving_a_file_and_reading_it_back_works_end_to_end() {
    let node = Node::start("save", POLICY);
    let file = node.dir.join("notes.md");
    let contents = "a T0 node still has an assistant\n";
    std::fs::write(&file, contents).unwrap();

    let out = node.otwono(&["do", "save", file.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let saved = stdout(&out);
    assert!(saved.contains("save (store.put)"), "{saved}");

    // Pull the content id out of what it printed, then ask for it back.
    let id = saved
        .split_whitespace()
        .find(|w| w.len() == 64 && w.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| panic!("no content id in: {saved}"))
        .to_string();

    let out = node.otwono(&["do", "fetch", &id, "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid json");
    let data = value["result"]["data"].as_str().expect("data field");
    let decoded = data_encoding::BASE64.decode(data.as_bytes()).expect("base64");
    assert_eq!(
        String::from_utf8_lossy(&decoded),
        contents,
        "the file came back different from how it went in"
    );
}

/// The default label is PRIVATE, because the user did not say otherwise.
///
/// CLAUDE.md §8. Worth asserting through the assistant specifically: a convenience layer
/// that quietly widened the default would be the easiest place in the system to leak from,
/// and the user typed nothing about visibility at all.
#[test]
fn a_saved_file_is_private_unless_the_user_says_otherwise() {
    let node = Node::start("private", POLICY);
    let file = node.dir.join("secret.txt");
    std::fs::write(&file, "not for the street").unwrap();

    let out = node.otwono(&["do", "save", file.to_str().unwrap(), "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(
        value["result"]["visibility"], "private",
        "the assistant widened a label the user never mentioned: {value}"
    );
}

/// Declining is not failing, and the exit codes say which.
///
/// The message is §6's, not "I do not know how to". This assertion used to be the other way
/// round, matching what the code did rather than what AI-RUNTIME.md §6 says — which is how
/// a wrong behaviour stays green. Running the binary by hand is what caught it.
#[test]
fn an_open_ended_request_declines_with_its_own_exit_code() {
    let node = Node::start("decline", POLICY);
    let out = node.otwono(&["do", "summarise", "my", "week"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let text = stderr(&out);
    assert!(text.contains("on this machine"), "{text}");
    assert!(text.contains("command-grammar"), "{text}");
    assert!(
        !text.contains("Did you mean"),
        "an open-ended request is not a misspelling: {text}"
    );
    // Nothing was attempted, so nothing is on stdout to be piped into something else.
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
}

#[test]
fn a_near_miss_is_offered_the_verb_it_nearly_typed() {
    let node = Node::start("nearmiss", POLICY);
    let out = node.otwono(&["do", "sav", "/tmp/whatever"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(stderr(&out).contains("save"), "{}", stderr(&out));
}

/// A policy refusal is the user's own system saying no — reported as a decline, not a fault.
#[test]
fn a_refused_capability_is_reported_as_the_policy_refusing() {
    let node = Node::start(
        "refused",
        "[[rule]]\naction = \"store.write\"\ndecision = \"deny\"\n",
    );
    let file = node.dir.join("x.txt");
    std::fs::write(&file, "x").unwrap();
    let out = node.otwono(&["do", "save", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let text = stderr(&out);
    assert!(text.contains("policy refuses store.write"), "{text}");
}

/// `--dry-run` says what would happen and touches nothing.
#[test]
fn a_dry_run_changes_nothing() {
    let node = Node::start("dryrun", POLICY);
    let file = node.dir.join("dry.txt");
    std::fs::write(&file, "dry").unwrap();

    let out = node.otwono(&["do", "save", file.to_str().unwrap(), "--dry-run"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("would:"), "{text}");
    assert!(text.contains("needs: store.write"), "{text}");

    // The cache is a proxy for "the daemons were not asked to do anything": a dry run that
    // had dispatched would have put an object in the store, and the store's own count is
    // not reachable without store.read on an id we would not have.
    let out = node.otwono(&["do", "cache", "--json"]);
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(value["result"]["objects"], 0, "a dry run stored something");
}

/// Help names the shape before it lists the verbs.
///
/// A user who is told the machine runs a command grammar asks different questions from one
/// who discovers it by being refused three times.
#[test]
fn help_says_what_kind_of_assistant_this_is() {
    let node = Node::start("help", POLICY);
    let out = node.otwono(&["do", "help"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("command-grammar assistant"), "{text}");
    assert!(text.contains("save"), "{text}");
    assert!(
        text.contains("[changes something]"),
        "a verb that writes should be marked as one: {text}"
    );
}

/// The assistant is not a way around the permission model.
///
/// It holds no token of its own and adds no capability. The policy here grants the store
/// but not the network boundary, and there is no verb that reaches past it — so this asserts
/// both that the vocabulary is bounded and that being refused looks like being refused.
#[test]
fn the_assistant_cannot_reach_past_what_the_policy_granted() {
    let node = Node::start("bounded", POLICY);
    for reach in [
        vec!["do", "serve", "anything"],
        vec!["do", "publish", "everything"],
        vec!["do", "connect", "somewhere"],
    ] {
        let out = node.otwono(&reach);
        assert_eq!(
            out.status.code(),
            Some(3),
            "\"{}\" was not declined: {}",
            reach.join(" "),
            stdout(&out)
        );
    }
}

/// The same binary on a bigger machine behaves differently, and says so.
///
/// This is the assertion whose absence let the suite depend on its host. Every other test
/// here pins T0 and checks the command-grammar behaviour; without a second tier, "the shape
/// comes from the capability engine" was an untested claim, and the whole suite passed on a
/// T0 development box while failing on a larger CI runner for a reason none of it named.
///
/// The refusal is the visible difference: at T0 an unrecognised request is a limit of the
/// machine and says "command-grammar". At T2 the request would go to a model — there is no
/// model installed here, so it still refuses, but it must not claim to be a command grammar.
#[test]
fn a_larger_machine_is_not_described_as_a_command_grammar() {
    let node = Node::start_as("t2", POLICY, otwono_capability::Tier::T2Balanced);

    let out = node.otwono(&["do", "tier"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("T2_BALANCED"), "{}", stdout(&out));

    // Help must not tell a T2 user their machine answers a fixed set of commands.
    let out = node.otwono(&["do", "help", "--json"]);
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(
        value["assistant_shape"], "planning_with_retrieval",
        "the binary did not take its shape from the capability engine: {value}"
    );

    let out = node.otwono(&["do", "summarise", "my", "week"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(
        !stderr(&out).contains("command-grammar"),
        "a T2 machine described itself as a command grammar: {}",
        stderr(&out)
    );
}

/// A verb still works on a bigger machine.
///
/// The grammar is not a T0 fallback that a larger node discards — "save this file" is a
/// deterministic request at every tier, and routing it through a model would be slower,
/// less reliable, and no more useful.
#[test]
fn the_verbs_still_work_on_a_machine_with_a_bigger_shape() {
    let node = Node::start_as("t2verbs", POLICY, otwono_capability::Tier::T2Balanced);
    let file = node.dir.join("t2.txt");
    std::fs::write(&file, "verbs are not a T0 fallback").unwrap();
    let out = node.otwono(&["do", "save", file.to_str().unwrap(), "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(value["did"], "store.put");
    assert_eq!(value["result"]["visibility"], "private");
}

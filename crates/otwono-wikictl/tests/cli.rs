//! `otwono-wikictl` run as a program, against real daemons.
//!
//! The control-plane tests next door drive the *calls* this tool makes, which is the right
//! place to find out whether a method behaves — a failure there names a method rather than a
//! subprocess. What they cannot reach is the binary itself: its argument parsing, its exit
//! codes, and the text it prints. Every one of those is something a person or a boot check
//! depends on, and all of it was unexercised.
//!
//! That is not hypothetical either. `read --from` was written, reviewed and committed with a
//! call that omitted the peer `net.fetch` requires; it would have failed the first time
//! anybody ran it, and nothing here would have said so.

use otwono_idd::IdentityService;
use otwono_identity::{SharingKeystore, SigningKeystore};
use otwono_permd::{ActionRegistry, AuditLog, Broker, Policy};
use otwono_proto::{Client, Server, Shutdown};
use otwono_store::{StorageKey, Store};
use otwono_stored::StoreService;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::Duration;

const POLICY: &str = r#"
[[rule]]
action = "store.*"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "pointer.*"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "id.sign"
decision = "allow"
ttl_seconds = 300
"#;

struct Node {
    dir: PathBuf,
    shutdown: Shutdown,
}

impl Node {
    fn start(tag: &str) -> Node {
        let dir = std::env::temp_dir().join(format!("otw-wikictl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::write(dir.join("policy.d/10-test.toml"), POLICY).unwrap();
        let shutdown = Shutdown::new();

        let policy = Policy::load_dir(&dir.join("policy.d")).expect("policy must load");
        policy
            .validate(&ActionRegistry::builtin())
            .expect("the test policy must name only registered actions");
        let broker = Arc::new(Broker::new(
            policy,
            AuditLog::open(dir.join("audit.jsonl")).unwrap(),
        ));
        let s = shutdown.clone();
        let srv = Server::bind(dir.join("perm.sock")).unwrap();
        std::thread::spawn(move || srv.serve(broker, s));

        let keystore = SigningKeystore::new(dir.join("identity"));
        let sharing = SharingKeystore::new(dir.join("identity"));
        let (signing, _) = keystore.load_or_generate().unwrap();
        let idd = Arc::new(
            IdentityService::new(
                keystore,
                signing,
                sharing.load_or_generate().unwrap().0,
                dir.join("perm.sock"),
            )
            .unwrap(),
        );
        let s = shutdown.clone();
        let srv = Server::bind(dir.join("id.sock")).unwrap();
        std::thread::spawn(move || srv.serve(idd, s));

        let store = Store::encrypted(dir.join("store"), StorageKey::generate());
        store.ensure_layout().unwrap();
        let service = Arc::new(
            StoreService::new(store, dir.join("perm.sock"))
                .with_identity(dir.join("id.sock"))
                .with_pointers(otwono_store::PointerStore::at(dir.join("pointers")).unwrap()),
        );
        let s = shutdown.clone();
        let srv = Server::bind(dir.join("store.sock")).unwrap();
        std::thread::spawn(move || srv.serve(service, s));

        for sock in ["perm.sock", "id.sock", "store.sock"] {
            Client::connect_waiting(dir.join(sock), Duration::from_secs(5))
                .unwrap_or_else(|e| panic!("{sock} never came up: {e}"));
        }
        Node { dir, shutdown }
    }

    /// Run the real binary, as a person or a boot check would.
    fn wikictl(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_otwono-wikictl"))
            .args(args)
            .arg("--socket")
            .arg(self.dir.join("store.sock"))
            .arg("--perm-socket")
            .arg(self.dir.join("perm.sock"))
            .arg("--id-socket")
            .arg(self.dir.join("id.sock"))
            .output()
            .expect("the otwono-wikictl binary runs")
    }

    fn page(&self, name: &str, text: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

#[test]
fn a_page_written_and_read_back_through_the_binary() {
    let n = Node::start("roundtrip");
    let file = n.page("in.md", "the first page\n");

    let w = n.wikictl(&["write", "Getting-Started", "--file", file.to_str().unwrap()]);
    assert!(w.status.success(), "write failed: {}", err(&w));
    assert!(
        out(&w).contains("first revision"),
        "a new page's write should say it has no parent: {}",
        out(&w)
    );

    let back = n.dir.join("out.md");
    let r = n.wikictl(&["read", "Getting-Started", "--out", back.to_str().unwrap()]);
    assert!(r.status.success(), "read failed: {}", err(&r));
    assert_eq!(std::fs::read_to_string(&back).unwrap(), "the first page\n");
}

#[test]
fn the_page_name_is_positional_and_comes_before_the_flags() {
    // `write --file x Page` must be a usage error rather than a page called "--file". The
    // parser takes the name only when it does not start with `--`, and nothing else checks
    // this — it is exactly the sort of thing that works until somebody types it.
    let n = Node::start("argorder");
    let file = n.page("in.md", "text\n");
    let o = n.wikictl(&["write", "--file", file.to_str().unwrap(), "Getting-Started"]);
    assert!(!o.status.success(), "that ordering must not be accepted");
    assert_eq!(o.status.code(), Some(2), "a usage error exits 2: {}", err(&o));
}

#[test]
fn a_page_that_does_not_exist_is_a_runtime_error_not_a_usage_one() {
    // The two exit codes mean different things to a script: 2 is "you typed it wrong" and 1
    // is "the thing you asked for is not here". Conflating them makes a boot check unable to
    // tell a bug in itself from a state of the node.
    let n = Node::start("absent");
    let o = n.wikictl(&[
        "read",
        "Never-Written",
        "--out",
        n.dir.join("x").to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(err(&o).contains("no page called"), "{}", err(&o));
}

#[test]
fn history_reports_how_the_walk_ended() {
    // A list of revisions cannot tell a whole history from as much of one as this node holds,
    // so the end marker is part of the answer and the boot check greps for it.
    let n = Node::start("history");
    for text in ["one\n", "two\n"] {
        let f = n.page("in.md", text);
        let w = n.wikictl(&["write", "Getting-Started", "--file", f.to_str().unwrap()]);
        assert!(w.status.success(), "{}", err(&w));
    }
    let h = n.wikictl(&["history", "Getting-Started"]);
    assert!(h.status.success(), "{}", err(&h));
    assert!(
        out(&h).contains("end: complete"),
        "history must say how it ended: {}",
        out(&h)
    );
    assert_eq!(
        out(&h).lines().filter(|l| l.starts_with("end:")).count(),
        1,
        "exactly one end line: {}",
        out(&h)
    );
    assert_eq!(
        out(&h).lines().count(),
        3,
        "two revisions and an end line: {}",
        out(&h)
    );
}

#[test]
fn listing_a_node_with_no_pages_says_so_rather_than_printing_nothing() {
    // Silence and "there are none" are the same on a terminal and different to a script, and
    // an empty listing is the state a fresh node is in.
    let n = Node::start("empty");
    let l = n.wikictl(&["list"]);
    assert!(l.status.success(), "{}", err(&l));
    assert!(out(&l).contains("no wiki pages"), "{}", out(&l));
}

#[test]
fn delete_says_what_it_does_not_do_and_refuses_a_name_nobody_used() {
    let n = Node::start("delete");
    let file = n.page("in.md", "here for now\n");
    assert!(n
        .wikictl(&["write", "Temporary", "--file", file.to_str().unwrap()])
        .status
        .success());

    let d = n.wikictl(&["delete", "Temporary"]);
    assert!(d.status.success(), "{}", err(&d));
    assert!(
        out(&d).contains("still readable by id"),
        "a delete must not imply a reach this system does not have: {}",
        out(&d)
    );

    // Listed, not hidden: the tombstone is why the name cannot be reused as if it were fresh.
    let l = n.wikictl(&["list"]);
    assert!(out(&l).contains("Temporary deleted"), "{}", out(&l));

    // And deleting a name nobody used is refused rather than publishing a signed assertion
    // about the absence of something that never existed.
    let again = n.wikictl(&["delete", "Never-Used"]);
    assert_eq!(again.status.code(), Some(1), "{}", err(&again));
    assert!(err(&again).contains("nothing to delete"), "{}", err(&again));
}

#[test]
fn json_output_is_json() {
    // `--json` exists for scripts. A stray human-readable line in it makes the whole reply
    // unparseable, which is a failure mode that only shows up in whatever consumes it.
    let n = Node::start("json");
    let file = n.page("in.md", "structured\n");
    let w = n.wikictl(&[
        "write",
        "Getting-Started",
        "--file",
        file.to_str().unwrap(),
        "--json",
    ]);
    assert!(w.status.success(), "{}", err(&w));
    let v: serde_json::Value = serde_json::from_str(out(&w).trim())
        .unwrap_or_else(|e| panic!("write --json did not print json ({e}): {}", out(&w)));
    assert_eq!(v["page"], "Getting-Started");
    assert!(v["revision"].as_str().is_some_and(|s| s.len() == 64));
}

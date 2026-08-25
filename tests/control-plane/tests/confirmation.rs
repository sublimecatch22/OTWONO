//! The confirmation channel over two real sockets (ADR-0024).
//!
//! Both surfaces run as real servers: `perm.*` on one socket, `confirm.*` on another. The
//! point of the split is that a caller reaching one cannot use the other, and a test that
//! called both through one object would prove nothing about that.
//!
//! Under ADR-0024 §3a only a subject in the configured confirmer set may answer. These
//! harnesses run as one uid, so a test either designates that uid a confirmer (and the
//! approval path works) or does not (and every answer is refused). Both are exercised.
//!
//! `a_second_user_can_approve_and_then_the_asker_gets_its_token` goes further and connects
//! as a **different real uid** over the socket, because the rule is about `SO_PEERCRED` and
//! a stubbed subject would prove nothing about it. It is skipped when not run as root.

use otwono_permd::{ActionRegistry, AuditLog, Broker, Policy};
use otwono_proto::{Client, Server, Shutdown};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// `fs.delete` is `always_confirm` in the registry, so `allow` here still resolves to `ask`.
const POLICY: &str = r#"
[[rule]]
action = "fs.delete"
decision = "allow"
ttl_seconds = 300

[[rule]]
action = "hw.read"
decision = "allow"
ttl_seconds = 300
"#;

struct Harness {
    dir: PathBuf,
    perm: PathBuf,
    confirm: PathBuf,
    shutdown: Shutdown,
}

impl Harness {
    /// A node where nobody is designated to confirm — the default, and the fail-closed one.
    fn start(tag: &str) -> Harness {
        Harness::start_with(tag, Vec::new())
    }

    /// A node where `confirmers` may answer.
    fn start_with(tag: &str, confirmers: Vec<String>) -> Harness {
        let dir = std::env::temp_dir().join(format!("otw-cf{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::write(dir.join("policy.d/10-test.toml"), POLICY).unwrap();

        let perm = dir.join("perm.sock");
        let confirm = dir.join("confirm.sock");
        let shutdown = Shutdown::new();

        let policy = Policy::load_dir(&dir.join("policy.d")).expect("policy must load");
        policy.validate(&ActionRegistry::builtin()).unwrap();
        let broker = Arc::new(
            Broker::new(policy, AuditLog::open(dir.join("audit.jsonl")).unwrap()).with_confirmers(confirmers),
        );
        let confirmations = Arc::new(broker.confirmations());

        let ps = Server::bind(&perm).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || ps.serve(broker, s));
        let cs = Server::bind(&confirm).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || cs.serve(confirmations, s));

        for sock in [&perm, &confirm] {
            Client::connect_waiting(sock, Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{} never came up", sock.display()));
        }
        Harness {
            dir,
            perm,
            confirm,
            shutdown,
        }
    }

    fn call(&self, sock: &PathBuf, method: &str, params: Value) -> Result<Value, otwono_proto::RpcError> {
        Client::connect(sock).unwrap().call(method, params).unwrap()
    }

    /// Ask for something that needs a person, and return the confirmation id it opened.
    fn ask(&self, resource: &str) -> String {
        let err = self
            .call(
                &self.perm,
                "perm.request",
                json!({ "action": "fs.delete", "resource": resource, "reason": "tidying up" }),
            )
            .expect_err("fs.delete always confirms");
        let msg = err.message;
        // The id is in the message; a caller reads it the same way.
        let id = msg
            .split("Confirmation ")
            .nth(1)
            .and_then(|t| t.split_whitespace().next())
            .unwrap_or_else(|| panic!("no confirmation id in: {msg}"));
        id.to_string()
    }

    fn audit_lines(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn an_action_that_needs_a_person_opens_a_confirmation_instead_of_just_failing() {
    let h = Harness::start("open");
    let id = h.ask("/home/u/tax-2025.ods");
    assert!(!id.is_empty());

    let listed = h.call(&h.confirm, "confirm.list", json!({})).expect("list");
    let pending = listed["pending"].as_array().unwrap();
    assert_eq!(pending.len(), 1);
    let p = &pending[0];
    assert_eq!(p["confirmation_id"], json!(id));
    assert_eq!(p["action"], json!("fs.delete"));
    // "Delete a file" and "delete which file" are not the same question.
    assert_eq!(p["resource"], json!("/home/u/tax-2025.ods"));
    // The word that changes an answer.
    assert_eq!(p["blast_radius"], json!("Irreversible"));
    // And the caller's reason is labelled as the caller's, not as an explanation.
    assert_eq!(p["caller_claims"], json!("tidying up"));
    assert!(listed["note"].as_str().unwrap().contains("not a fact"));
}

#[test]
fn nothing_is_authorised_until_somebody_answers() {
    let h = Harness::start("pending");
    let id = h.ask("/home/u/a");
    let err = h
        .call(&h.perm, "perm.claim", json!({ "confirmation_id": id }))
        .expect_err("nobody has answered yet");
    assert!(err.message.contains("nobody has confirmed"), "{}", err.message);
}

#[test]
fn nobody_can_answer_on_a_node_with_no_confirmer_configured() {
    // The default, and the honest version of "this node cannot confirm anything" (ADR-0024
    // §3a). A set that fell back to "anyone" would turn an unconfigured node into an open
    // door on the exact actions that most need a person.
    let h = Harness::start("noconfirmer");
    let id = h.ask("/home/u/b");

    let err = h
        .call(&h.confirm, "confirm.approve", json!({ "confirmation_id": id }))
        .expect_err("nobody is designated, so nobody may answer");
    assert!(err.message.contains("may not answer"), "{}", err.message);

    // And nothing was authorised by the attempt.
    let claim = h
        .call(&h.perm, "perm.claim", json!({ "confirmation_id": id }))
        .expect_err("still pending");
    assert!(
        claim.message.contains("nobody has confirmed"),
        "{}",
        claim.message
    );

    // The refusal is in the audit chain: "something tried to answer and was not allowed to"
    // is exactly what an audit reader is looking for.
    let refused = h
        .audit_lines()
        .into_iter()
        .filter(|l| l.contains("confirmation_decided") && l.contains("refused"))
        .count();
    assert_eq!(refused, 1, "the refused answer was not recorded");
}

#[test]
fn a_confirmer_may_answer_their_own_request() {
    // The flow the first version of ADR-0024 §3 would have refused, and the normal one on a
    // household node with one person: they run a CLI, are shown what it will do, and say
    // yes. Asking and approving are two real acts by one party, and the second is where the
    // consequence is seen.
    let me = format!("uid:{}", unsafe { libc_geteuid() });
    let h = Harness::start_with("ownreq", vec![me.clone()]);
    let id = h.ask("/home/u/own");

    let ok = h
        .call(&h.confirm, "confirm.approve", json!({ "confirmation_id": id }))
        .expect("a designated confirmer may answer their own request");
    assert_eq!(ok["state"], json!("approved"));
    assert_eq!(ok["decided_by"], json!(me));

    let token = h
        .call(&h.perm, "perm.claim", json!({ "confirmation_id": id }))
        .expect("and the token follows");
    assert!(token["token"].as_str().is_some(), "{token}");
    assert_eq!(token["one_shot"], json!(true), "{token}");
}

#[test]
fn a_denial_by_a_confirmer_is_final() {
    let me = format!("uid:{}", unsafe { libc_geteuid() });
    let h = Harness::start_with("denyreal", vec![me]);
    let id = h.ask("/home/u/c");

    h.call(&h.confirm, "confirm.deny", json!({ "confirmation_id": id }))
        .expect("a confirmer may say no");
    let err = h
        .call(&h.perm, "perm.claim", json!({ "confirmation_id": id }))
        .expect_err("a denial authorises nothing");
    assert!(err.message.contains("denied"), "{}", err.message);

    // And it is not re-readable: a caller cannot sit on a "no" and retry it.
    let again = h
        .call(&h.perm, "perm.claim", json!({ "confirmation_id": id }))
        .expect_err("consumed");
    assert!(
        again.message.contains("no such confirmation"),
        "{}",
        again.message
    );
}

#[test]
fn an_unknown_confirmation_is_refused_rather_than_invented() {
    let h = Harness::start("unknown");
    let err = h
        .call(
            &h.perm,
            "perm.claim",
            json!({ "confirmation_id": "0".repeat(64) }),
        )
        .expect_err("no such confirmation");
    assert!(err.message.contains("no such confirmation"), "{}", err.message);
}

#[test]
fn the_confirmation_socket_cannot_be_used_to_ask_for_anything() {
    // The reason there are two sockets. Whatever can reach the confirmation socket may
    // answer requests; it must not be able to make them, or the separation buys nothing.
    let h = Harness::start("noask");
    for method in ["perm.request", "perm.claim", "perm.verify", "perm.actions"] {
        let err = h
            .call(&h.confirm, method, json!({ "action": "fs.delete" }))
            .expect_err("{method} must not be served on the confirmation socket");
        assert!(
            err.message.contains(method),
            "expected an unknown-method refusal naming {method}, got {}",
            err.message
        );
    }
}

#[test]
fn the_control_plane_socket_cannot_be_used_to_answer_anything() {
    // And the converse, which is the half that actually matters: if confirm.approve were
    // reachable on the socket every daemon already talks to, the channel would be
    // decoration.
    let h = Harness::start("noanswer");
    let id = h.ask("/home/u/d");
    for method in ["confirm.approve", "confirm.deny", "confirm.list"] {
        let err = h
            .call(&h.perm, method, json!({ "confirmation_id": id }))
            .expect_err("{method} must not be served on the control-plane socket");
        assert!(
            err.message.contains(method),
            "expected an unknown-method refusal naming {method}, got {}",
            err.message
        );
    }
}

#[test]
fn an_action_that_does_not_confirm_still_goes_straight_through() {
    // The channel must not have made ordinary permissions slower or stranger.
    let h = Harness::start("normal");
    let out = h
        .call(&h.perm, "perm.request", json!({ "action": "hw.read" }))
        .expect("hw.read does not need a person");
    assert!(out["token"].as_str().is_some());
}

#[test]
fn opening_a_confirmation_is_recorded_before_the_caller_learns_anything() {
    let h = Harness::start("audit");
    let id = h.ask("/home/u/e");
    let opened: Vec<String> = h
        .audit_lines()
        .into_iter()
        .filter(|l| l.contains("confirmation_opened"))
        .collect();
    assert_eq!(opened.len(), 1);
    assert!(
        opened[0].contains(&id),
        "the record does not name the confirmation"
    );
    assert!(opened[0].contains("fs.delete"));
    assert!(opened[0].contains("/home/u/e"));
}

#[test]
fn the_audit_chain_survives_a_confirmation_flow() {
    let h = Harness::start("chain");
    h.ask("/home/u/f");
    let _ = h.call(
        &h.confirm,
        "confirm.approve",
        json!({ "confirmation_id": "x".repeat(64) }),
    );
    let report = h.call(&h.perm, "perm.audit.verify", json!({})).expect("verify");
    assert_eq!(report["intact"], json!(true), "{report}");
}

/// Speak one JSON-RPC call over a Unix socket as another uid, using socat.
///
/// The protocol is newline-delimited JSON-RPC 2.0 precisely so it can be driven this way
/// (CLAUDE.md §4.1). Doing it with a real second uid is the only way to exercise the
/// approval path: the rule under test is about `SO_PEERCRED`, and a stubbed subject would
/// assert nothing about it.
fn call_as_uid(sock: &std::path::Path, uid: u32, method: &str, params: Value) -> String {
    let body = serde_json::to_string(&json!({
        "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
    }))
    .unwrap();
    let out = std::process::Command::new("setpriv")
        .args([
            "--reuid",
            &uid.to_string(),
            "--regid",
            &uid.to_string(),
            "--clear-groups",
            "socat",
            "-T",
            "5",
            "-",
            &format!("UNIX-CONNECT:{}", sock.display()),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .as_mut()
                .unwrap()
                .write_all(format!("{body}\n").as_bytes())?;
            c.wait_with_output()
        })
        .expect("socat must run");
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn have_tools() -> bool {
    let root = unsafe { libc_geteuid() } == 0;
    root && std::process::Command::new("socat")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

#[test]
fn a_second_user_can_approve_and_then_the_asker_gets_its_token() {
    // The path everything else exists to protect, and the only one that shows the channel
    // actually working: a *different real uid* approves, and the asker then gets a token it
    // could not otherwise have had.
    if !have_tools() {
        eprintln!("skipped: needs root and socat to connect as a second uid");
        return;
    }
    use std::os::unix::fs::PermissionsExt;

    let h = Harness::start_with("twouid", vec!["uid:65534".to_string()]);
    let id = h.ask("/home/u/two-uid");

    // Let another uid reach the confirmation socket. On a real node this is what the
    // socket's ownership would express; here it has to be arranged deliberately.
    std::fs::set_permissions(&h.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&h.confirm, std::fs::Permissions::from_mode(0o666)).unwrap();

    let reply = call_as_uid(
        &h.confirm,
        65534,
        "confirm.approve",
        json!({ "confirmation_id": id }),
    );
    assert!(
        reply.contains("approved"),
        "a different uid should have been able to approve: {reply}"
    );

    // And now the asker -- and only the asker -- collects the token.
    let token = h
        .call(&h.perm, "perm.claim", json!({ "confirmation_id": id }))
        .expect("an approved confirmation yields the token it authorised");
    assert!(token["token"].as_str().is_some(), "{token}");
    // One-shot regardless of the rule's ttl: the person agreed to one thing happening once.
    assert_eq!(token["one_shot"], json!(true), "{token}");

    // Consumed. The same approval cannot be spent twice.
    let again = h
        .call(&h.perm, "perm.claim", json!({ "confirmation_id": id }))
        .expect_err("one approval authorises one request");
    assert!(
        again.message.contains("no such confirmation"),
        "{}",
        again.message
    );

    // The audit chain names who approved it.
    let decided: Vec<String> = h
        .audit_lines()
        .into_iter()
        .filter(|l| l.contains("confirmation_decided") && l.contains("approved"))
        .collect();
    assert_eq!(decided.len(), 1);
    assert!(decided[0].contains("uid:65534"), "{}", decided[0]);
}

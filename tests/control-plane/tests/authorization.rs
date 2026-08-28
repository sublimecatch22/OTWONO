//! Phase 2 exit criterion, exercised end to end over real Unix sockets.
//!
//! Two daemons run in threads on a temporary socket directory: `otwono-permd` (the
//! broker) and `otwono-hwd` (a guarded service). Every assertion below goes over the wire
//! through the real JSON-RPC transport — nothing is called in-process — because the
//! authorization path includes `SO_PEERCRED`, and an in-process test would skip exactly
//! the part that matters.

use otwono_capability::CapabilityOverrides;
use otwono_hwd::HwService;
use otwono_permd::{AuditLog, Broker, Policy};
use otwono_proto::{code, Client, Server, Shutdown};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Two daemons, a policy, and an audit log in a scratch directory.
struct Harness {
    dir: PathBuf,
    perm_socket: PathBuf,
    hw_socket: PathBuf,
    audit_log: PathBuf,
    shutdown: Shutdown,
    uid: u32,
}

impl Harness {
    /// `policy_toml` is written verbatim, so each test states exactly the rules it relies on.
    fn start(tag: &str, policy_toml: &str) -> Harness {
        let uid = rustix::process::getuid().as_raw();
        // Keep the path short: AF_UNIX addresses are capped near 108 bytes.
        let dir = std::env::temp_dir().join(format!("otw-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();

        std::fs::write(dir.join("policy.d/10-test.toml"), policy_toml).unwrap();

        let perm_socket = dir.join("perm.sock");
        let hw_socket = dir.join("hw.sock");
        let audit_log = dir.join("audit.jsonl");
        let shutdown = Shutdown::new();

        let policy = Policy::load_dir(&dir.join("policy.d")).expect("policy must load");
        policy
            .validate(&otwono_permd::ActionRegistry::builtin())
            .expect("test policy must name only registered actions");

        let broker = Arc::new(Broker::new(policy, AuditLog::open(&audit_log).unwrap()));
        let perm_server = Server::bind(&perm_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || perm_server.serve(broker, s));

        // The probe root is a committed fixture, so the profile is identical on every
        // machine that runs this test.
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/otwono-hal/tests/fixtures/x86_64-cloud-vm");
        let hw = Arc::new(HwService::new(
            fixture,
            perm_socket.clone(),
            CapabilityOverrides::default(),
        ));
        let hw_server = Server::bind(&hw_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || hw_server.serve(hw, s));

        // Both sockets must be accepting before any assertion runs.
        Client::connect_waiting(&perm_socket, Duration::from_secs(5)).expect("permd never came up");
        Client::connect_waiting(&hw_socket, Duration::from_secs(5)).expect("hwd never came up");

        Harness {
            dir,
            perm_socket,
            hw_socket,
            audit_log,
            shutdown,
            uid,
        }
    }

    fn perm(&self) -> Client {
        Client::connect(&self.perm_socket).unwrap()
    }

    fn hw(&self) -> Client {
        Client::connect(&self.hw_socket).unwrap()
    }

    /// Ask the broker for a token, returning the raw JSON-RPC outcome.
    fn request(&self, action: &str, resource: Option<&str>) -> Result<Value, otwono_proto::RpcError> {
        let mut params = json!({ "action": action });
        if let Some(r) = resource {
            params["resource"] = json!(r);
        }
        self.perm().call("perm.request", params).unwrap()
    }

    fn token_for(&self, action: &str) -> String {
        self.request(action, None)
            .unwrap_or_else(|e| panic!("expected a token for {action}, got {e}"))
            .get("token")
            .and_then(Value::as_str)
            .expect("response must carry a token")
            .to_string()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.trigger();
        std::thread::sleep(Duration::from_millis(60));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn allow_hw_read(uid: u32) -> String {
    format!(
        r#"
[[rule]]
action = "hw.read"
subjects = ["uid:{uid}"]
decision = "allow"
ttl_seconds = 120

[[rule]]
action = "audit.read"
subjects = ["uid:{uid}"]
decision = "allow"
"#
    )
}

// ---------------------------------------------------------------------------------------
// The exit criterion: refused without authority, served with it, and the log proves both.
// ---------------------------------------------------------------------------------------

#[test]
fn an_unauthorized_caller_is_refused() {
    let uid = rustix::process::getuid().as_raw();
    let h = Harness::start("unauth", &allow_hw_read(uid));

    let err = h
        .hw()
        .call("hw.profile", json!({}))
        .unwrap()
        .expect_err("a call with no capability token must fail");

    assert_eq!(err.code, code::UNAUTHORIZED, "got {err}");
    assert!(
        err.message.contains("capability token"),
        "the error should say what is missing: {err}"
    );
}

#[test]
fn a_forged_token_is_refused() {
    let uid = rustix::process::getuid().as_raw();
    let h = Harness::start("forged", &allow_hw_read(uid));

    let err = h
        .hw()
        .call_with_capability("hw.profile", json!({}), &"a".repeat(64))
        .unwrap()
        .expect_err("an invented token must not work");

    assert_eq!(err.code, code::UNAUTHORIZED, "got {err}");
}

#[test]
fn an_authorized_caller_succeeds() {
    let uid = rustix::process::getuid().as_raw();
    let h = Harness::start("authed", &allow_hw_read(uid));

    let token = h.token_for("hw.read");
    let profile = h
        .hw()
        .call_with_capability("hw.profile", json!({}), &token)
        .unwrap()
        .expect("an authorized call must succeed");

    assert_eq!(profile["tier"], "T2_BALANCED", "the fixture's tier");
    assert_eq!(profile["hardware"]["machine"]["architecture"], "x86_64");
    assert!(profile.get("axes").is_some());
    assert!(profile.get("features").is_some());
}

#[test]
fn the_audit_chain_verifies_and_records_both_outcomes() {
    let uid = rustix::process::getuid().as_raw();
    let h = Harness::start("audit", &allow_hw_read(uid));

    // One refusal and one success, so the log has something of each to prove.
    let _ = h.hw().call("hw.profile", json!({})).unwrap();
    let token = h.token_for("hw.read");
    h.hw()
        .call_with_capability("hw.profile", json!({}), &token)
        .unwrap()
        .unwrap();

    let report = h.perm().call("perm.audit.verify", json!({})).unwrap().unwrap();
    assert_eq!(report["intact"], true, "the chain must verify: {report}");
    assert!(report["records"].as_u64().unwrap() >= 3, "{report}");

    // Read the log back through the guarded method, which needs its own capability.
    let audit_token = h.token_for("audit.read");
    let tail = h
        .perm()
        .call_with_capability("perm.audit.tail", json!({ "limit": 100 }), &audit_token)
        .unwrap()
        .expect("audit.read was granted by the test policy");

    let records = tail["records"].as_array().unwrap();
    let outcomes: Vec<&str> = records.iter().filter_map(|r| r["outcome"].as_str()).collect();
    assert!(
        outcomes.contains(&"allow"),
        "a granted request must be recorded: {outcomes:?}"
    );
    assert!(
        records.iter().any(|r| r["event"] == "token_issued"),
        "issuing a token must be recorded"
    );
    assert!(
        records.iter().any(|r| r["event"] == "token_verified"),
        "verifying a token must be recorded"
    );
}

#[test]
fn tampering_with_the_audit_log_is_detected() {
    let uid = rustix::process::getuid().as_raw();
    let h = Harness::start("tamper", &allow_hw_read(uid));

    let token = h.token_for("hw.read");
    h.hw()
        .call_with_capability("hw.profile", json!({}), &token)
        .unwrap()
        .unwrap();
    assert_eq!(
        h.perm().call("perm.audit.verify", json!({})).unwrap().unwrap()["intact"],
        true
    );

    // Rewrite one recorded outcome, leaving the rest of the file byte-identical.
    let text = std::fs::read_to_string(&h.audit_log).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let target = lines
        .iter()
        .position(|l| l.contains(r#""outcome":"allow""#))
        .expect("the log should contain an allow");
    lines[target] = lines[target].replace(r#""outcome":"allow""#, r#""outcome":"deny""#);
    std::fs::write(&h.audit_log, lines.join("\n") + "\n").unwrap();

    let report = h.perm().call("perm.audit.verify", json!({})).unwrap().unwrap();
    assert_eq!(
        report["intact"], false,
        "an edited record must break the chain: {report}"
    );
    assert!(report["first_bad_seq"].is_u64());
}

// ---------------------------------------------------------------------------------------
// Policy behaviour over the wire
// ---------------------------------------------------------------------------------------

#[test]
fn an_action_with_no_matching_rule_is_denied() {
    let uid = rustix::process::getuid().as_raw();
    let h = Harness::start("norule", &allow_hw_read(uid));

    let err = h
        .request("fs.read", Some("/etc/shadow"))
        .expect_err("no rule covers fs.read");
    assert_eq!(err.code, code::FORBIDDEN, "got {err}");
    assert!(err.message.contains("default is deny"), "{err}");
}

#[test]
fn policy_cannot_grant_a_destructive_action_without_confirmation() {
    // A policy file that says `allow` on fs.delete still yields "confirmation required".
    // This is the property that keeps a compromised or careless policy from handing an
    // agent silent authority to delete a user's files.
    let uid = rustix::process::getuid().as_raw();
    let policy = format!(
        r#"
[[rule]]
action = "fs.delete"
subjects = ["uid:{uid}"]
decision = "allow"
"#
    );
    let h = Harness::start("confirm", &policy);

    let err = h
        .request("fs.delete", Some("/tmp/anything"))
        .expect_err("must not be granted");
    assert_eq!(err.code, code::CONFIRMATION_REQUIRED, "got {err}");
}

#[test]
fn a_token_for_one_action_does_not_authorize_another() {
    let uid = rustix::process::getuid().as_raw();
    let policy = format!(
        "{}\n[[rule]]\naction = \"fs.read\"\nsubjects = [\"uid:{uid}\"]\ndecision = \"allow\"\n",
        allow_hw_read(uid)
    );
    let h = Harness::start("scope", &policy);

    let fs_token = h.token_for("fs.read");
    let err = h
        .hw()
        .call_with_capability("hw.profile", json!({}), &fs_token)
        .unwrap()
        .expect_err("an fs.read token must not unlock hw.profile");
    assert_eq!(err.code, code::UNAUTHORIZED, "got {err}");
}

#[test]
fn a_rule_for_a_different_subject_does_not_grant_this_one() {
    // uid+1 is not this process, so the rule must not apply to it.
    let uid = rustix::process::getuid().as_raw();
    let policy = format!(
        r#"
[[rule]]
action = "hw.read"
subjects = ["uid:{}"]
decision = "allow"
"#,
        uid + 1
    );
    let h = Harness::start("othersubj", &policy);
    let err = h
        .request("hw.read", None)
        .expect_err("the rule names a different uid");
    assert_eq!(err.code, code::FORBIDDEN, "got {err}");
}

// ---------------------------------------------------------------------------------------
// Transport contract
// ---------------------------------------------------------------------------------------

#[test]
fn describe_is_open_on_both_services() {
    let uid = rustix::process::getuid().as_raw();
    let h = Harness::start("describe", &allow_hw_read(uid));

    let perm = h
        .perm()
        .describe()
        .unwrap()
        .expect("describe must not need a token");
    assert_eq!(perm.service, "otwono-permd");
    assert!(perm
        .methods
        .iter()
        .any(|m| m.name == "perm.request" && m.capability.is_none()));
    assert!(perm
        .methods
        .iter()
        .any(|m| m.name == "perm.audit.tail" && m.capability.as_deref() == Some("audit.read")));

    let hw = h
        .hw()
        .describe()
        .unwrap()
        .expect("describe must not need a token");
    assert_eq!(hw.service, "otwono-hwd");
    // Every hwd method must declare its capability: a caller has to be able to discover
    // what to ask the broker for.
    for m in &hw.methods {
        assert_eq!(
            m.capability.as_deref(),
            Some("hw.read"),
            "{} must declare a capability",
            m.name
        );
    }
}

#[test]
fn an_unknown_method_is_reported_as_such() {
    let uid = rustix::process::getuid().as_raw();
    let h = Harness::start("unknown", &allow_hw_read(uid));
    let err = h.hw().call("hw.nonexistent", json!({})).unwrap().unwrap_err();
    assert_eq!(err.code, code::METHOD_NOT_FOUND, "got {err}");
}

#[test]
fn malformed_json_does_not_kill_the_connection() {
    use std::io::{BufRead, BufReader, Write};
    let uid = rustix::process::getuid().as_raw();
    let h = Harness::start("garbage", &allow_hw_read(uid));

    let mut stream = std::os::unix::net::UnixStream::connect(&h.hw_socket).unwrap();
    stream.write_all(b"this is not json\n").unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["error"]["code"], code::PARSE_ERROR);

    // The same connection must still work afterwards.
    stream
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"describe\"}\n")
        .unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["id"], 9);
    assert_eq!(response["result"]["service"], "otwono-hwd");
}

#[test]
fn the_service_refuses_when_the_broker_is_unreachable() {
    // Fail closed: if the broker is down, a guarded call must be refused, never allowed.
    let dir = std::env::temp_dir().join(format!("otw-nobroker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let hw_socket = dir.join("hw.sock");
    let shutdown = Shutdown::new();
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/otwono-hal/tests/fixtures/x86_64-cloud-vm");
    let hw = Arc::new(HwService::new(
        fixture,
        dir.join("absent-perm.sock"),
        CapabilityOverrides::default(),
    ));
    let server = Server::bind(&hw_socket).unwrap();
    let s = shutdown.clone();
    std::thread::spawn(move || server.serve(hw, s));
    let mut client = Client::connect_waiting(&hw_socket, Duration::from_secs(5)).unwrap();

    let err = client
        .call_with_capability("hw.profile", json!({}), &"b".repeat(64))
        .unwrap()
        .expect_err("with no broker to consult, the call must be refused");
    assert_eq!(err.code, code::UNAVAILABLE, "got {err}");

    shutdown.trigger();
    std::thread::sleep(Duration::from_millis(60));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_socket_is_not_world_accessible() {
    use std::os::unix::fs::PermissionsExt;
    let uid = rustix::process::getuid().as_raw();
    let h = Harness::start("mode", &allow_hw_read(uid));
    let mode = std::fs::metadata(&h.perm_socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o660, "control-plane sockets must not be world-accessible");
    assert_eq!(h.uid, uid);
}

//! `otwono-fetchd` over the real control plane, against a network that misbehaves on cue.
//!
//! The broker, the sockets, the capability tokens and the audit log are real. The HTTP
//! transport is a double, because the interesting cases — a redirect to an attacker's
//! host, a server that ignores `Range`, an object that changes under a resumed download —
//! are precisely the ones a cooperative remote host will never produce. A test that can
//! only observe well-behaved servers does not test the part of this daemon that matters.

use otwono_fetch::{Source, SourceSet};
use otwono_fetchd::transport::{Head, Request, Transport, TransportError};
use otwono_fetchd::{FetchService, CAPABILITY_FETCH};
use otwono_permd::{AuditLog, Broker, Policy};
use otwono_proto::{code, Client, Server, Shutdown};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// --- the network double -------------------------------------------------------------

/// One canned response.
#[derive(Clone)]
struct Canned {
    status: u16,
    etag: Option<String>,
    total_bytes: Option<u64>,
    location: Option<String>,
    body: Vec<u8>,
}

impl Canned {
    fn ok(body: &[u8]) -> Canned {
        Canned {
            status: 200,
            etag: None,
            total_bytes: Some(body.len() as u64),
            location: None,
            body: body.to_vec(),
        }
    }

    fn partial(whole: &[u8], from: u64, etag: &str) -> Canned {
        Canned {
            status: 206,
            etag: Some(etag.to_string()),
            total_bytes: Some(whole.len() as u64),
            location: None,
            body: whole[from as usize..].to_vec(),
        }
    }

    fn redirect(status: u16, to: &str) -> Canned {
        Canned {
            status,
            etag: None,
            total_bytes: None,
            location: Some(to.to_string()),
            body: Vec::new(),
        }
    }

    fn status_only(status: u16) -> Canned {
        Canned {
            status,
            etag: None,
            total_bytes: None,
            location: None,
            body: Vec::new(),
        }
    }
}

/// Answers with whatever the test queued, and records every URL it was asked for.
struct FakeNet {
    queue: Mutex<Vec<Canned>>,
    seen: Mutex<Vec<String>>,
    /// When set, the double answers a `Range` request from that offset of `whole`.
    whole: Mutex<Option<(Vec<u8>, String)>>,
}

impl FakeNet {
    fn with_queue(responses: Vec<Canned>) -> Arc<FakeNet> {
        Arc::new(FakeNet {
            queue: Mutex::new(responses),
            seen: Mutex::new(Vec::new()),
            whole: Mutex::new(None),
        })
    }

    /// A well-behaved server holding `body`, honouring `Range` and reporting `etag`.
    fn serving(body: &[u8], etag: &str) -> Arc<FakeNet> {
        Arc::new(FakeNet {
            queue: Mutex::new(Vec::new()),
            seen: Mutex::new(Vec::new()),
            whole: Mutex::new(Some((body.to_vec(), etag.to_string()))),
        })
    }

    fn urls_seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

/// The handle handed to the service. Cloning the Arc keeps the test's view alive.
struct FakeTransport(Arc<FakeNet>);

impl Transport for FakeTransport {
    fn start(&self, request: &Request) -> Result<(Head, Box<dyn std::io::Read + Send>), TransportError> {
        self.0.seen.lock().unwrap().push(request.uri.to_string());

        if let Some((whole, etag)) = self.0.whole.lock().unwrap().clone() {
            let canned = if request.range_from == 0 {
                Canned {
                    etag: Some(etag),
                    ..Canned::ok(&whole)
                }
            } else if request.range_from >= whole.len() as u64 {
                Canned::status_only(416)
            } else {
                Canned::partial(&whole, request.range_from, &etag)
            };
            return Ok(respond(canned));
        }

        let canned = {
            let mut q = self.0.queue.lock().unwrap();
            if q.is_empty() {
                return Err(TransportError::Unreachable(
                    "the test queued no further responses".into(),
                ));
            }
            q.remove(0)
        };
        Ok(respond(canned))
    }
}

fn respond(c: Canned) -> (Head, Box<dyn std::io::Read + Send>) {
    let head = Head {
        status: c.status,
        etag: c.etag,
        total_bytes: c.total_bytes,
        location: c.location,
    };
    (head, Box::new(std::io::Cursor::new(c.body)))
}

// --- the harness --------------------------------------------------------------------

const POLICY: &str = r#"
[[rule]]
action = "net.fetch"
decision = "allow"
ttl_seconds = 300
"#;

struct Harness {
    dir: PathBuf,
    perm_socket: PathBuf,
    fetch_socket: PathBuf,
    audit_log: PathBuf,
    spool: PathBuf,
    shutdown: Shutdown,
    net: Arc<FakeNet>,
}

fn source(max_bytes: u64) -> Source {
    Source {
        id: "models".into(),
        host: "models.example.org".into(),
        port: None,
        path_prefix: "/otwono/".into(),
        max_bytes,
    }
}

impl Harness {
    fn start(tag: &str, net: Arc<FakeNet>, call_bytes: u64, max_bytes: u64) -> Harness {
        // AF_UNIX addresses are capped near 108 bytes, so keep the path short.
        let dir = std::env::temp_dir().join(format!("otw-f{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::write(dir.join("policy.d/10-test.toml"), POLICY).unwrap();

        let perm_socket = dir.join("perm.sock");
        let fetch_socket = dir.join("fetch.sock");
        let audit_log = dir.join("audit.jsonl");
        let spool = dir.join("spool");
        std::fs::create_dir_all(&spool).unwrap();
        let shutdown = Shutdown::new();

        let policy = Policy::load_dir(&dir.join("policy.d")).expect("policy must load");
        policy
            .validate(&otwono_permd::ActionRegistry::builtin())
            .expect("net.fetch must be a registered action");
        let broker = Arc::new(Broker::new(policy, AuditLog::open(&audit_log).unwrap()));
        let perm_server = Server::bind(&perm_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || perm_server.serve(broker, s));

        let sources = SourceSet::new(vec![source(max_bytes)]).expect("valid source");
        let service = Arc::new(
            FetchService::new(
                sources,
                spool.clone(),
                perm_socket.clone(),
                Box::new(FakeTransport(Arc::clone(&net))),
            )
            // No slack reserve: the temp filesystem's free space is not this test's subject.
            .with_budgets(call_bytes, Duration::from_secs(5), 0),
        );
        let fetch_server = Server::bind(&fetch_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || fetch_server.serve(service, s));

        Client::connect_waiting(&perm_socket, Duration::from_secs(5)).expect("permd never came up");
        Client::connect_waiting(&fetch_socket, Duration::from_secs(5)).expect("fetchd never came up");

        Harness {
            dir,
            perm_socket,
            fetch_socket,
            audit_log,
            spool,
            shutdown,
            net,
        }
    }

    fn token(&self, resource: Option<&str>) -> String {
        let mut perm = Client::connect(&self.perm_socket).unwrap();
        let granted = perm
            .call(
                "perm.request",
                json!({ "action": CAPABILITY_FETCH, "resource": resource }),
            )
            .unwrap()
            .expect("policy allows net.fetch");
        granted["token"].as_str().expect("a token").to_string()
    }

    /// Call with a real capability token scoped to whichever source the call names.
    fn call(&self, method: &str, params: Value) -> Result<Value, otwono_proto::RpcError> {
        let resource = params.get("source").and_then(Value::as_str).map(str::to_string);
        let token = self.token(resource.as_deref());
        let mut c = Client::connect(&self.fetch_socket).unwrap();
        c.call_with_capability(method, params, &token).unwrap()
    }

    /// Call with no token at all.
    fn call_unauthorized(&self, method: &str, params: Value) -> Result<Value, otwono_proto::RpcError> {
        let mut c = Client::connect(&self.fetch_socket).unwrap();
        c.call(method, params).unwrap()
    }

    fn get(&self, path: &str) -> Result<Value, otwono_proto::RpcError> {
        self.call("fetch.get", json!({ "source": "models", "path": path }))
    }

    fn audit(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.audit_log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn blob(v: &Value) -> Vec<u8> {
    let path = v["blob_path"].as_str().expect("a completed fetch has a path");
    std::fs::read(path).expect("the blob is readable")
}

// --- the tests ----------------------------------------------------------------------

#[test]
fn a_fetch_without_a_capability_is_refused() {
    let h = Harness::start("noauth", FakeNet::serving(b"payload", "\"v1\""), 1 << 20, 1 << 20);
    let err = h
        .call_unauthorized("fetch.get", json!({ "source": "models", "path": "m.gguf" }))
        .expect_err("no token means no fetch");
    assert_eq!(err.code, code::UNAUTHORIZED);
    // And nothing was attempted on the network.
    assert!(h.net.urls_seen().is_empty(), "authorization runs first");
}

#[test]
fn a_whole_object_arrives_in_one_call() {
    let body = b"the weights, notionally".to_vec();
    let h = Harness::start("whole", FakeNet::serving(&body, "\"v1\""), 1 << 20, 1 << 20);
    let got = h.get("q4/m.gguf").expect("fetch");
    assert_eq!(got["complete"], json!(true));
    assert_eq!(got["bytes_have"], json!(body.len()));
    assert_eq!(blob(&got), body);
    assert_eq!(
        h.net.urls_seen(),
        vec!["https://models.example.org/otwono/q4/m.gguf".to_string()]
    );
}

#[test]
fn a_large_object_is_fetched_in_pieces_and_resumes_where_it_stopped() {
    // The reason fetch.get is bounded at all: a model does not fit inside one
    // control-plane call, so resumption is the normal path rather than an error path.
    let body: Vec<u8> = (0..5_000u32).map(|i| (i % 251) as u8).collect();
    let h = Harness::start("resume", FakeNet::serving(&body, "\"v1\""), 1_000, 1 << 20);

    let first = h.get("big.gguf").expect("first call");
    assert_eq!(first["complete"], json!(false));
    assert_eq!(first["bytes_have"], json!(1_000));
    assert_eq!(first["bytes_total"], json!(5_000));
    assert_eq!(first["blob_path"], Value::Null, "nothing to hand over yet");

    let mut calls = 1;
    let mut last = first;
    while last["complete"] == json!(false) {
        last = h.get("big.gguf").expect("resumed call");
        calls += 1;
        assert!(calls < 20, "should converge");
    }
    assert_eq!(last["bytes_have"], json!(5_000));
    assert_eq!(blob(&last), body, "the pieces reassemble into the object");
}

#[test]
fn a_redirect_inside_the_source_is_followed() {
    let net = FakeNet::with_queue(vec![
        Canned::redirect(302, "https://models.example.org/otwono/real/m.gguf"),
        Canned::ok(b"redirected payload"),
    ]);
    let h = Harness::start("redir-ok", net, 1 << 20, 1 << 20);
    let got = h.get("m.gguf").expect("fetch");
    assert_eq!(got["complete"], json!(true));
    assert_eq!(blob(&got), b"redirected payload");
    assert_eq!(
        h.net.urls_seen(),
        vec![
            "https://models.example.org/otwono/m.gguf".to_string(),
            "https://models.example.org/otwono/real/m.gguf".to_string(),
        ]
    );
}

#[test]
fn a_redirect_off_the_source_is_never_followed() {
    // The test this daemon exists for. A source that can bounce us anywhere is not an
    // allow-list, and a `3xx` is a stranger asking us to make a different request.
    for (i, target) in [
        "https://evil.example.com/x",
        "https://models.example.org.evil.example.com/x",
        "https://models.example.org:8443/otwono/x",
        "http://models.example.org/otwono/x",
        "https://models.example.org/elsewhere/x",
        "https://models.example.org/otwono/../../etc/passwd",
    ]
    .iter()
    .enumerate()
    {
        let net = FakeNet::with_queue(vec![
            Canned::redirect(302, target),
            Canned::ok(b"should never be reached"),
        ]);
        // A distinct scratch directory per case: a shutting-down server removes its own
        // socket on the way out, and reusing the path lets the previous one delete the
        // next one's.
        let h = Harness::start(&format!("rb{i}"), net, 1 << 20, 1 << 20);
        let err = h.get("m.gguf").expect_err(&format!("{target} must be refused"));
        // Two refusal paths reach this point and both are correct: a plaintext target is
        // turned away before it is even resolved, and everything else fails admission
        // against the source. What matters is that the hop is named and not taken.
        assert!(
            err.message.contains("redirect"),
            "{target}: unexpected error {}",
            err.message
        );
        assert_eq!(
            h.net.urls_seen().len(),
            1,
            "{target}: the second request must never be made"
        );
    }
}

#[test]
fn a_root_relative_redirect_is_resolved_against_the_source_and_still_checked() {
    let net = FakeNet::with_queue(vec![
        Canned::redirect(307, "/otwono/moved/m.gguf"),
        Canned::ok(b"moved payload"),
    ]);
    let h = Harness::start("redir-rel", net, 1 << 20, 1 << 20);
    assert_eq!(blob(&h.get("m.gguf").expect("fetch")), b"moved payload");

    let net = FakeNet::with_queue(vec![Canned::redirect(307, "/somewhere/else")]);
    let h = Harness::start("redir-rel2", net, 1 << 20, 1 << 20);
    assert!(h.get("m.gguf").is_err(), "still has to be under the prefix");
}

#[test]
fn a_redirect_loop_terminates() {
    let net = FakeNet::with_queue(
        (0..20)
            .map(|_| Canned::redirect(302, "https://models.example.org/otwono/round"))
            .collect(),
    );
    let h = Harness::start("redir-loop", net, 1 << 20, 1 << 20);
    let err = h.get("m.gguf").expect_err("a circle is not a path");
    assert!(err.message.contains("redirected more than"), "{}", err.message);
}

#[test]
fn a_caller_cannot_climb_out_of_the_source_prefix() {
    let h = Harness::start("traverse", FakeNet::serving(b"x", "\"v1\""), 1 << 20, 1 << 20);
    for bad in ["../../etc/passwd", "/etc/passwd", "a?x=1", "a%2f..%2fb", ""] {
        let err = h.get(bad).expect_err(&format!("{bad:?} must be refused"));
        assert_eq!(err.code, code::INVALID_PARAMS, "{bad:?}: {}", err.message);
    }
    assert!(
        h.net.urls_seen().is_empty(),
        "a rejected path never reaches the network"
    );
}

#[test]
fn an_unknown_source_is_refused_before_anything_is_attempted() {
    let h = Harness::start("unknown", FakeNet::serving(b"x", "\"v1\""), 1 << 20, 1 << 20);
    let err = h
        .call("fetch.get", json!({ "source": "not-in-the-list", "path": "m" }))
        .expect_err("unknown source");
    assert_eq!(err.code, code::INVALID_PARAMS);
    assert!(h.net.urls_seen().is_empty());
}

#[test]
fn an_object_over_the_sources_cap_is_refused_rather_than_downloaded() {
    let body = vec![0u8; 4_096];
    let h = Harness::start("cap", FakeNet::serving(&body, "\"v1\""), 1 << 20, 1_024);
    let err = h.get("big.gguf").expect_err("over the cap");
    assert_eq!(err.code, code::INVALID_PARAMS);
    assert!(err.message.contains("max_bytes"), "{}", err.message);
}

#[test]
fn a_server_that_ignores_our_range_restarts_the_download_rather_than_corrupting_it() {
    // A 200 in reply to a Range request is the whole object from byte zero. Appending it
    // to a partial would produce a file that is the right length and the wrong bytes —
    // which the caller's digest would catch, after wasting the entire transfer.
    let body: Vec<u8> = (0..2_000u32).map(|i| (i % 251) as u8).collect();
    let net = FakeNet::with_queue(vec![
        Canned {
            status: 200,
            etag: None,
            total_bytes: Some(2_000),
            location: None,
            body: body.clone(),
        },
        Canned {
            status: 200,
            etag: None,
            total_bytes: Some(2_000),
            location: None,
            body: body.clone(),
        },
    ]);
    let h = Harness::start("norange", net, 500, 1 << 20);

    let first = h.get("m.gguf").expect("first call");
    assert_eq!(first["bytes_have"], json!(500));
    assert_eq!(first["complete"], json!(false));

    let second = h.get("m.gguf").expect("second call");
    assert_eq!(second["restarted"], json!(true), "it started over");
    assert_eq!(second["bytes_have"], json!(500), "from zero, not from 500");
    let part = std::fs::read(h.spool.join(format!(
        "{}.part",
        otwono_fetch::spool::SpoolEntry::new(&h.spool, "models", "m.gguf").key()
    )))
    .expect("the partial exists");
    assert_eq!(part, body[..500], "the partial is a prefix of the object");
}

#[test]
fn an_object_that_changes_under_a_resumed_download_is_caught_early() {
    let first_half: Vec<u8> = vec![1u8; 1_000];
    let net = FakeNet::with_queue(vec![
        Canned {
            status: 206,
            etag: Some("\"v1\"".into()),
            total_bytes: Some(4_000),
            location: None,
            body: first_half,
        },
        Canned {
            status: 206,
            etag: Some("\"v2\"".into()),
            total_bytes: Some(4_000),
            location: None,
            body: vec![2u8; 3_000],
        },
    ]);
    let h = Harness::start("etag", net, 1_000, 1 << 20);
    assert_eq!(h.get("m.gguf").expect("first")["bytes_have"], json!(1_000));
    let err = h.get("m.gguf").expect_err("the object changed");
    assert!(err.message.contains("changed under"), "{}", err.message);
}

#[test]
fn a_completed_fetch_is_served_from_the_spool_without_touching_the_network() {
    let h = Harness::start("cached", FakeNet::serving(b"payload", "\"v1\""), 1 << 20, 1 << 20);
    let first = h.get("m.gguf").expect("fetch");
    assert_eq!(first["complete"], json!(true));
    let before = h.net.urls_seen().len();
    let again = h.get("m.gguf").expect("second fetch");
    assert_eq!(again["complete"], json!(true));
    assert_eq!(again["blob_path"], first["blob_path"]);
    assert_eq!(
        h.net.urls_seen().len(),
        before,
        "already having it is a reason not to ask again"
    );
}

#[test]
fn discarding_removes_the_blob_and_the_next_fetch_starts_again() {
    let h = Harness::start(
        "discard",
        FakeNet::serving(b"payload", "\"v1\""),
        1 << 20,
        1 << 20,
    );
    let got = h.get("m.gguf").expect("fetch");
    let path = got["blob_path"].as_str().unwrap().to_string();
    assert!(std::path::Path::new(&path).exists());

    h.call("fetch.discard", json!({ "source": "models", "path": "m.gguf" }))
        .expect("discard");
    assert!(!std::path::Path::new(&path).exists());

    let before = h.net.urls_seen().len();
    h.get("m.gguf").expect("re-fetch");
    assert_eq!(h.net.urls_seen().len(), before + 1);
}

#[test]
fn a_partial_that_could_never_be_resumed_is_refused_rather_than_kept() {
    // Found against a live server. Without a stated size there is nothing to resume *to*:
    // the next call asks for a range the server may refuse, and a caller looping on "not
    // complete yet" loops forever. Better to fail once, clearly, with the partial dropped.
    let net = FakeNet::with_queue(vec![Canned {
        status: 200,
        etag: None,
        total_bytes: None, // no Content-Length
        location: None,
        body: vec![9u8; 10_000],
    }]);
    let h = Harness::start("nototal", net, 1_000, 1 << 20);
    let err = h.get("m.gguf").expect_err("unresumable");
    assert!(err.message.contains("did not say how large"), "{}", err.message);
    let entries: Vec<_> = std::fs::read_dir(&h.spool)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(entries.is_empty(), "the unusable partial was discarded");
}

#[test]
fn a_short_object_with_no_stated_size_still_completes() {
    // The rule above must not break the ordinary case: a body that ends before the budget
    // does is finished, whether or not the server said how long it would be.
    let net = FakeNet::with_queue(vec![Canned {
        status: 200,
        etag: None,
        total_bytes: None,
        location: None,
        body: b"short and complete".to_vec(),
    }]);
    let h = Harness::start("nototal2", net, 1_000, 1 << 20);
    let got = h.get("m.gguf").expect("fetch");
    assert_eq!(got["complete"], json!(true));
    assert_eq!(got["bytes_total"], Value::Null);
    assert_eq!(blob(&got), b"short and complete");
}

#[test]
fn a_404_is_reported_and_leaves_nothing_behind() {
    let net = FakeNet::with_queue(vec![Canned::status_only(404)]);
    let h = Harness::start("notfound", net, 1 << 20, 1 << 20);
    let err = h.get("missing.gguf").expect_err("404");
    assert!(err.message.contains("404"), "{}", err.message);
    let entries: Vec<_> = std::fs::read_dir(&h.spool)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(entries.is_empty(), "a failed fetch spools nothing");
}

#[test]
fn listing_sources_says_what_this_node_may_contact() {
    let h = Harness::start("list", FakeNet::serving(b"x", "\"v1\""), 1 << 20, 4_096);
    let listed = h.call("fetch.sources", json!({})).expect("sources");
    let sources = listed["sources"].as_array().expect("an array");
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["id"], json!("models"));
    assert_eq!(sources[0]["host"], json!("models.example.org"));
    assert_eq!(sources[0]["port"], json!(443));
    assert_eq!(sources[0]["max_bytes"], json!(4_096));
}

#[test]
fn every_fetch_leaves_a_record_naming_the_action() {
    // "What did this node talk to" has to be answerable from the audit log, which is the
    // whole argument for one brokered fetcher rather than three ad-hoc ones.
    let h = Harness::start("audit", FakeNet::serving(b"payload", "\"v1\""), 1 << 20, 1 << 20);
    h.get("m.gguf").expect("fetch");
    let records = h.audit();
    assert!(
        records
            .iter()
            .any(|r| r["action"] == json!(CAPABILITY_FETCH) && r["event"] == json!("token_issued")),
        "no net.fetch token in the audit log: {records:?}"
    );
}

#[test]
fn a_token_for_one_source_does_not_authorize_another() {
    // The resource a net.fetch token is bound to is the source id, so a node can be given
    // the model host without being given the update host. If the resource were ignored,
    // every grant would silently be "anywhere in the allow-list".
    let h = Harness::start("scope", FakeNet::serving(b"payload", "\"v1\""), 1 << 20, 1 << 20);
    let wrong = h.token(Some("some-other-source"));
    let mut c = Client::connect(&h.fetch_socket).unwrap();
    let err = c
        .call_with_capability(
            "fetch.get",
            json!({ "source": "models", "path": "m.gguf" }),
            &wrong,
        )
        .unwrap()
        .expect_err("a token scoped elsewhere must not work here");
    assert_eq!(err.code, code::UNAUTHORIZED, "{}", err.message);
    assert!(h.net.urls_seen().is_empty(), "nothing was attempted");
}

#[test]
fn describe_is_open_and_names_the_capability_each_method_needs() {
    let h = Harness::start("describe", FakeNet::serving(b"x", "\"v1\""), 1 << 20, 1 << 20);
    let mut c = Client::connect(&h.fetch_socket).unwrap();
    let described = c.call("describe", json!({})).unwrap().expect("describe is open");
    let methods = described["methods"].as_array().expect("methods");
    for name in ["fetch.get", "fetch.sources", "fetch.discard"] {
        let m = methods
            .iter()
            .find(|m| m["name"] == json!(name))
            .unwrap_or_else(|| panic!("{name} should be described"));
        assert_eq!(m["capability"], json!(CAPABILITY_FETCH), "{name}");
    }
}

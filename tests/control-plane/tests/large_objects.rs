//! ADR-0018 over the real control plane: objects too big for a JSON-RPC line.
//!
//! The inline path caps at 640 KiB because the control plane is newline-delimited JSON with
//! a 1 MiB line limit. `store.export` and `store.import` move the bytes as a file instead
//! and put only a path on the socket.
//!
//! Two things are being checked, and the second is the one that could hurt:
//!
//! 1. An object far larger than any line round-trips out and back unchanged.
//! 2. **A root daemon cannot be talked into reading a file the caller does not own.** The
//!    path is opened with `O_NOFOLLOW` and then checked on the descriptor, so a symlink
//!    swapped in after the check refers to nothing the daemon will look at.

use otwono_permd::{ActionRegistry, AuditLog, Broker, Policy};
use otwono_proto::{code, Client, Server, Shutdown};
use otwono_store::{Handoff, StorageKey, Store};
use otwono_stored::StoreService;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const POLICY: &str = r#"
[[rule]]
action = "store.*"
decision = "allow"
ttl_seconds = 300
"#;

struct Harness {
    dir: PathBuf,
    perm_socket: PathBuf,
    store_socket: PathBuf,
    export_dir: PathBuf,
    shutdown: Shutdown,
}

impl Harness {
    fn start(tag: &str) -> Harness {
        let dir = std::env::temp_dir().join(format!("otw-lo-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::write(dir.join("policy.d/10-test.toml"), POLICY).unwrap();

        let perm_socket = dir.join("perm.sock");
        let store_socket = dir.join("store.sock");
        let export_dir = dir.join("export");
        let shutdown = Shutdown::new();

        let policy = Policy::load_dir(&dir.join("policy.d")).unwrap();
        policy.validate(&ActionRegistry::builtin()).unwrap();
        let broker = Arc::new(Broker::new(
            policy,
            AuditLog::open(dir.join("audit.jsonl")).unwrap(),
        ));
        let s = shutdown.clone();
        let server = Server::bind(&perm_socket).unwrap();
        std::thread::spawn(move || server.serve(broker, s));

        // Encrypted, as on a real node — which is what makes the export interesting: the
        // store on disk is ciphertext and the exported file is not.
        let (key, _) = StorageKey::load_or_generate(&dir.join("storage.key")).unwrap();
        let store = Store::encrypted(dir.join("store"), key);
        store.ensure_layout().unwrap();
        let handoff = Handoff::new(&export_dir);
        handoff.ensure_layout().unwrap();
        let service = Arc::new(StoreService::new(store, perm_socket.clone()).with_handoff(handoff));
        let s = shutdown.clone();
        let server = Server::bind(&store_socket).unwrap();
        std::thread::spawn(move || server.serve(service, s));

        for sock in [&perm_socket, &store_socket] {
            Client::connect_waiting(sock, Duration::from_secs(5)).unwrap();
        }
        Harness {
            dir,
            perm_socket,
            store_socket,
            export_dir,
            shutdown,
        }
    }

    fn token(&self, action: &str) -> String {
        let mut broker = Client::connect(&self.perm_socket).unwrap();
        broker
            .call(
                "perm.request",
                json!({ "action": action, "reason": "large object test" }),
            )
            .unwrap()
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn call(&self, method: &str, params: Value, action: &str) -> Result<Value, otwono_proto::RpcError> {
        let token = self.token(action);
        Client::connect_with_timeout(&self.store_socket, Duration::from_secs(60))
            .unwrap()
            .call_with_capability(method, params, &token)
            .unwrap()
    }

    fn scratch(&self) -> PathBuf {
        let d = self.dir.join("caller");
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Comfortably past every inline limit: 8 MiB is thirteen times the 640 KiB cap and eight
/// times the whole control-plane line.
const BIG: usize = 8 * 1024 * 1024;

#[test]
fn an_object_far_larger_than_a_control_plane_line_round_trips_as_a_file() {
    let h = Harness::start("roundtrip");
    let bytes = payload(BIG, 1);
    let source = h.scratch().join("big.bin");
    std::fs::write(&source, &bytes).unwrap();

    let imported = h
        .call(
            "store.import",
            json!({ "path": source.display().to_string(), "visibility": "private" }),
            "store.write",
        )
        .expect("an 8 MiB file must import");
    assert_eq!(imported["size_bytes"].as_u64().unwrap(), BIG as u64);
    let id = imported["content_id"].as_str().unwrap().to_string();

    // The same object is far too big for the inline path, which must say so *itself* and
    // name the method that works — not let the caller's own reader refuse the reply after
    // the daemon has already assembled and encoded 8 MiB.
    let inline = h
        .call("store.get", json!({ "content_id": id }), "store.read")
        .expect_err("store.get cannot carry 8 MiB");
    assert_eq!(inline.code, code::INVALID_PARAMS);
    assert!(
        inline.message.contains("store.export"),
        "the refusal must point at the method that can: {}",
        inline.message
    );

    let exported = h
        .call("store.export", json!({ "content_id": id }), "store.read")
        .expect("but store.export can");
    let path = PathBuf::from(exported["path"].as_str().unwrap());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "the exported file is not the object"
    );
    assert_eq!(exported["exported_bytes"].as_u64().unwrap(), BIG as u64);
}

#[test]
fn an_exported_file_is_plaintext_while_the_store_on_disk_is_not() {
    // Worth asserting rather than assuming: an operator has to be told that exporting is a
    // decryption, and a test that says so is harder to forget than a comment.
    let h = Harness::start("plaintext");
    let marker = b"MARKER-e6a1c9-the-user-would-recognise-this".repeat(64);
    let source = h.scratch().join("marked.bin");
    std::fs::write(&source, &marker).unwrap();
    let id = h
        .call(
            "store.import",
            json!({ "path": source.display().to_string(), "visibility": "private" }),
            "store.write",
        )
        .unwrap()["content_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Nothing under the store directory contains the marker.
    let mut found_in_store = false;
    for entry in walk(&h.dir.join("store")) {
        if let Ok(bytes) = std::fs::read(&entry) {
            if find(&bytes, b"MARKER-e6a1c9") {
                found_in_store = true;
            }
        }
    }
    assert!(!found_in_store, "the store held the object in the clear");

    let exported = h
        .call("store.export", json!({ "content_id": id }), "store.read")
        .unwrap();
    let path = PathBuf::from(exported["path"].as_str().unwrap());
    assert!(
        find(&std::fs::read(&path).unwrap(), b"MARKER-e6a1c9"),
        "the export was not plaintext, which means it is not usable"
    );
    assert!(
        exported["note"].as_str().unwrap().contains("plaintext"),
        "the reply must say what it just handed over"
    );
}

#[test]
fn a_symlink_cannot_make_the_daemon_read_a_file_the_caller_does_not_own() {
    // The one that matters. This daemon runs as root; a caller pointing it at a link to
    // something only root can read must not get its contents into the store.
    let h = Harness::start("symlink");
    let secret = h.dir.join("root-only.txt");
    std::fs::write(&secret, b"pretend this is /etc/shadow").unwrap();
    let link = h.scratch().join("innocent.bin");
    std::os::unix::fs::symlink(&secret, &link).unwrap();

    let err = h
        .call(
            "store.import",
            json!({ "path": link.display().to_string(), "visibility": "private" }),
            "store.write",
        )
        .expect_err("a symlink must be refused");
    assert_eq!(err.code, code::INVALID_PARAMS);
    assert!(
        err.message.contains("not a regular file belonging to you"),
        "{}",
        err.message
    );
}

#[test]
fn a_directory_and_a_missing_path_are_refused_the_same_way() {
    // A caller that can tell "that is a directory" from "that is not here" learns about the
    // filesystem through a root daemon.
    let h = Harness::start("shapes");
    let dir = h.scratch();
    let absent = dir.join("nothing-here");
    let mut messages = Vec::new();
    for path in [dir.clone(), absent.clone()] {
        let err = h
            .call(
                "store.import",
                json!({ "path": path.display().to_string() }),
                "store.write",
            )
            .expect_err("neither is importable");
        assert_eq!(err.code, code::INVALID_PARAMS);
        messages.push(err.message.replace(path.to_str().unwrap(), "<path>"));
    }
    assert_eq!(messages[0], messages[1], "the refusals differ: {messages:?}");
}

#[test]
fn an_import_inherits_the_most_restrictive_label_of_its_inputs() {
    // Derivation cannot launder a label, and the file path must not be a way around that.
    let h = Harness::start("derive");
    let private = h
        .call(
            "store.put",
            json!({ "data": data_encoding::BASE64.encode(b"a private input"), "visibility": "private" }),
            "store.write",
        )
        .unwrap()["content_id"]
        .as_str()
        .unwrap()
        .to_string();

    let source = h.scratch().join("derived.bin");
    std::fs::write(&source, payload(1024 * 1024, 3)).unwrap();
    let imported = h
        .call(
            "store.import",
            json!({
                "path": source.display().to_string(),
                "visibility": "public",
                "derived_from": [private],
            }),
            "store.write",
        )
        .unwrap();
    assert_eq!(
        imported["visibility"], "private",
        "a public import over a private input laundered the label"
    );
    assert_eq!(imported["requested_visibility"], "public");
}

#[test]
fn an_exported_object_is_owned_by_the_caller_and_readable_by_nobody_else() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::start("mode");
    let source = h.scratch().join("f.bin");
    std::fs::write(&source, payload(1024 * 1024, 4)).unwrap();
    let id = h
        .call(
            "store.import",
            json!({ "path": source.display().to_string() }),
            "store.write",
        )
        .unwrap()["content_id"]
        .as_str()
        .unwrap()
        .to_string();
    let exported = h
        .call("store.export", json!({ "content_id": id }), "store.read")
        .unwrap();
    let path = PathBuf::from(exported["path"].as_str().unwrap());
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let dir_mode = std::fs::metadata(&h.export_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "the export directory is listable");
}

#[test]
fn both_large_methods_are_guarded() {
    let h = Harness::start("guards");
    for (method, params) in [
        ("store.export", json!({ "content_id": "0".repeat(64) })),
        ("store.import", json!({ "path": "/tmp/whatever" })),
    ] {
        let err = Client::connect(&h.store_socket)
            .unwrap()
            .call(method, params)
            .unwrap()
            .expect_err("must be refused without a token");
        assert_eq!(err.code, code::UNAUTHORIZED, "{method} was not guarded");
    }
}

#[test]
fn exporting_something_that_is_not_there_is_a_caller_error() {
    let h = Harness::start("absent");
    let err = h
        .call(
            "store.export",
            json!({ "content_id": "0".repeat(64) }),
            "store.read",
        )
        .expect_err("nothing to export");
    assert_eq!(err.code, code::INVALID_PARAMS);
}

fn walk(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

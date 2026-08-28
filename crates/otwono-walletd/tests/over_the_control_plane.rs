//! `otwono-walletd` behind a real `otwono-permd`, over real Unix sockets.
//!
//! The permission broker is not stubbed, because the thing most worth testing about this
//! daemon is what the broker does to it: three of its four actions are `always_confirm`, and
//! a stub would happily let them through and prove nothing (ADR-0023 §4).

use otwono_permd::{ActionRegistry, AuditLog, Broker, Policy};
use otwono_proto::{Client, Server, Shutdown};
use otwono_walletd::WalletService;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Everything the wallet has, granted as widely as a policy can grant it.
///
/// Deliberately `wallet.*` rather than four rules: it is what an operator would actually
/// write, and it is the shape a compromised policy file would take. The tests below then
/// show that writing it does not buy what it appears to.
const POLICY: &str = r#"
[[rule]]
action = "wallet.*"
decision = "allow"
ttl_seconds = 300
"#;

struct Harness {
    dir: PathBuf,
    perm_socket: PathBuf,
    wallet_socket: PathBuf,
    vault: PathBuf,
    shutdown: Shutdown,
}

impl Harness {
    fn start(tag: &str) -> Harness {
        let dir = std::env::temp_dir().join(format!("otw-wd{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("policy.d")).unwrap();
        std::fs::write(dir.join("policy.d/10-test.toml"), POLICY).unwrap();

        let perm_socket = dir.join("perm.sock");
        let wallet_socket = dir.join("wallet.sock");
        let vault = dir.join("wallet/seed.vault");
        let shutdown = Shutdown::new();

        let policy = Policy::load_dir(&dir.join("policy.d")).expect("policy must load");
        policy
            .validate(&ActionRegistry::builtin())
            .expect("the test policy must name only registered actions");
        let broker = Arc::new(Broker::new(
            policy,
            AuditLog::open(dir.join("audit.jsonl")).unwrap(),
        ));
        let ps = Server::bind(&perm_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || ps.serve(broker, s));

        let service = Arc::new(WalletService::new(&vault, &perm_socket));
        let ws = Server::bind(&wallet_socket).unwrap();
        let s = shutdown.clone();
        std::thread::spawn(move || ws.serve(service, s));

        for sock in [&perm_socket, &wallet_socket] {
            Client::connect_waiting(sock, Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{} never came up", sock.display()));
        }
        Harness {
            dir,
            perm_socket,
            wallet_socket,
            vault,
            shutdown,
        }
    }

    /// A token for `action`, or the decision that stopped one being issued.
    fn token(&self, action: &str) -> Result<String, Value> {
        let reply = Client::connect(&self.perm_socket)
            .unwrap()
            .call("perm.request", json!({ "action": action }))
            .unwrap();
        match reply {
            Ok(v) => match v.get("token").and_then(Value::as_str) {
                Some(t) => Ok(t.to_string()),
                None => Err(v),
            },
            Err(e) => Err(json!({ "error": e.message })),
        }
    }

    fn call(&self, method: &str, params: Value, action: &str) -> Result<Value, otwono_proto::RpcError> {
        let token = self
            .token(action)
            .unwrap_or_else(|v| panic!("expected a token for {action}, got {v}"));
        Client::connect_with_timeout(&self.wallet_socket, Duration::from_secs(30))
            .unwrap()
            .call_with_capability(method, params, &token)
            .unwrap()
    }

    /// Put a wallet on disk without going through `wallet.create`.
    ///
    /// Necessary rather than convenient: `wallet.create` is `always_confirm`, so no policy
    /// can produce a token for it and there is no way to create a wallet through this daemon
    /// until Phase 7. Testing the read paths therefore means writing the vault directly —
    /// which is exactly what a person will do through a console flow later.
    fn plant_a_wallet(&self, passphrase: &str) -> String {
        let m = otwono_wallet::Mnemonic::generate();
        otwono_wallet::Vault::new(&self.vault)
            .write(&m.seed(""), passphrase)
            .unwrap();
        m.phrase().to_string()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn creating_signing_and_exporting_are_all_refused_a_token_however_the_policy_is_written() {
    // ADR-0023 §4, end to end. The policy above says allow on wallet.*; the broker still
    // refuses to hand out a token, because policy cannot clear an intrinsic confirmation
    // requirement and there is nobody to confirm.
    //
    // This is the test that would fail loudly if somebody "temporarily" cleared
    // always_confirm to get the wallet working, which is the change most likely to be made
    // for good reasons.
    let h = Harness::start("confirm");
    for action in ["wallet.create", "wallet.export_seed", "wallet.sign"] {
        let outcome = h.token(action);
        let refusal = outcome
            .err()
            .unwrap_or_else(|| panic!("{action} produced a usable token; nothing is confirming it"));
        // And refused for the *right* reason. "No token" would also be the answer if the
        // action were unregistered or the policy had a typo, and either of those would make
        // this test pass while proving nothing about confirmation.
        let text = refusal.to_string();
        assert!(
            text.contains("confirm") || refusal["decision"] == serde_json::json!("ask"),
            "{action} was refused, but not for want of a person: {text}"
        );
    }
}

#[test]
fn reading_needs_no_confirmation() {
    // The other half. If reading stopped for a person too, a finance screen could not render
    // at all and the pressure would be to widen something that must not widen.
    let h = Harness::start("read");
    h.token("wallet.read")
        .expect("reading the wallet must not need a person");
}

#[test]
fn a_node_with_no_wallet_says_so_rather_than_failing() {
    let h = Harness::start("empty");
    let out = h.call("wallet.status", json!({}), "wallet.read").expect("status");
    assert_eq!(out["exists"], json!(false));
    assert!(out["note"].as_str().unwrap().contains("needs a person"));
}

#[test]
fn status_describes_the_vault_and_never_anything_secret() {
    let h = Harness::start("status");
    let phrase = h.plant_a_wallet("correct horse battery staple");

    let out = h.call("wallet.status", json!({}), "wallet.read").expect("status");
    assert_eq!(out["exists"], json!(true));
    assert_eq!(out["kdf"], json!("argon2id"));
    assert_eq!(out["cipher"], json!("xchacha20poly1305"));

    // Structural, not substring. The first version of this asserted that no individual
    // recovery word appeared anywhere in the reply -- which fails 20% of the time, because
    // the reply's own prose ("address", "public key", "this", "use", "because"...) contains
    // 19 BIP-39 words, and a random 24-word phrase hits one of them one run in five. It
    // passed locally and went red in CI, which is the only reason it was caught.
    //
    // Checking the field set is both deterministic and stronger: it catches a *new* field
    // that leaks something, which per-word matching never could.
    const ALLOWED: [&str; 9] = [
        "exists", "path", "version", "kdf", "cipher", "m_cost", "t_cost", "p_cost", "note",
    ];
    for field in out.as_object().unwrap().keys() {
        assert!(
            ALLOWED.contains(&field.as_str()),
            "wallet.status grew a field, {field:?}. Everything it returns is readable by \
             anyone who can read the vault file; a new one may not be"
        );
    }

    // And the secrets themselves, by whole phrase rather than by word. Three consecutive
    // BIP-39 words occurring in prose by chance is not a thing that happens.
    let shown = out.to_string();
    assert!(
        !shown.contains(&phrase),
        "status leaked the whole recovery phrase"
    );
    let words: Vec<&str> = phrase.split_whitespace().collect();
    for window in words.windows(3) {
        let run = window.join(" ");
        assert!(!shown.contains(&run), "status leaked part of the phrase: {run}");
    }
    assert!(
        !shown.contains("correct horse"),
        "status echoed a passphrase it was never even given"
    );
    // And it says why there is no address here, rather than leaving a UI to wonder.
    assert!(out["note"].as_str().unwrap().contains("without the passphrase"));
}

#[test]
fn public_keys_need_the_right_passphrase() {
    let h = Harness::start("pubkeys");
    h.plant_a_wallet("right");

    let ok = h
        .call(
            "wallet.public_keys",
            json!({ "passphrase": "right", "coin": 60, "indices": [0, 1, 2] }),
            "wallet.read",
        )
        .expect("the right passphrase derives");
    let keys = ok["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 3);
    assert_eq!(keys[0]["path"], json!("m/44'/60'/0'/0/0"));

    // Every index a different key -- ADR-0022 §5's premise, over the wire this time.
    let distinct: std::collections::HashSet<&str> =
        keys.iter().map(|k| k["public_key"].as_str().unwrap()).collect();
    assert_eq!(distinct.len(), 3, "two indices produced the same key");

    let wrong = h
        .call(
            "wallet.public_keys",
            json!({ "passphrase": "wrong", "coin": 60, "indices": [0] }),
            "wallet.read",
        )
        .expect_err("the wrong passphrase must not derive anything");
    assert!(wrong.message.contains("does not open"), "{}", wrong.message);
}

#[test]
fn deriving_is_deterministic_across_calls() {
    // What "the user owns the keys" has to mean in practice: the same wallet and the same
    // path give the same key every time, including across daemon restarts, because nothing
    // is cached between calls.
    let h = Harness::start("determ");
    h.plant_a_wallet("right");
    let ask = || {
        h.call(
            "wallet.public_keys",
            json!({ "passphrase": "right", "coin": 60, "indices": [7] }),
            "wallet.read",
        )
        .unwrap()["keys"][0]["public_key"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(ask(), ask());
}

#[test]
fn an_unbounded_batch_is_refused_rather_than_attempted() {
    // The count is caller-chosen and the work is linear in it. Unbounded, this is a way to
    // make a small board sit still for a long time.
    let h = Harness::start("batch");
    h.plant_a_wallet("right");
    let err = h
        .call(
            "wallet.public_keys",
            json!({ "passphrase": "right", "coin": 60, "indices": (0..500).collect::<Vec<u32>>() }),
            "wallet.read",
        )
        .expect_err("500 indices is more than one call should derive");
    assert!(err.message.contains("one call will derive"), "{}", err.message);

    let empty = h
        .call(
            "wallet.public_keys",
            json!({ "passphrase": "right", "coin": 60, "indices": [] }),
            "wallet.read",
        )
        .expect_err("asking for nothing is a mistake, not an empty answer");
    assert!(empty.message.contains("at least one"), "{}", empty.message);
}

#[test]
fn a_world_readable_vault_is_refused_over_the_wire_too() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::start("insecure");
    h.plant_a_wallet("right");
    std::fs::set_permissions(&h.vault, std::fs::Permissions::from_mode(0o644)).unwrap();

    let err = h
        .call("wallet.status", json!({}), "wallet.read")
        .expect_err("a wallet anybody can read is refused, not reported as healthy");
    assert!(err.message.contains("Refusing"), "{}", err.message);
}

#[test]
fn describe_is_open_and_names_a_capability_for_every_method() {
    // describe is unauthenticated on the local socket by design. What it must never do is
    // advertise a method without saying what a caller needs for it.
    let h = Harness::start("describe");
    let out = Client::connect(&h.wallet_socket)
        .unwrap()
        .call("describe", json!({}))
        .unwrap()
        .expect("describe is public");
    assert_eq!(out["service"], json!("otwono-walletd"));

    let methods = out["methods"].as_array().unwrap();
    assert!(!methods.is_empty());
    for m in methods {
        let name = m["name"].as_str().unwrap();
        assert!(
            m["capability"].as_str().is_some_and(|c| c.starts_with("wallet.")),
            "{name} does not name a wallet capability"
        );
    }

    // wallet.sign is deliberately absent: nothing can be signed until a chain is chosen,
    // and a method that existed but always refused would be a worse answer than one that is
    // honestly not here.
    assert!(!methods.iter().any(|m| m["name"] == json!("wallet.sign")));
}

#[test]
fn an_unknown_method_is_refused() {
    let h = Harness::start("unknown");
    let err = h
        .call("wallet.drain", json!({}), "wallet.read")
        .expect_err("unknown methods are refused");
    assert!(err.message.contains("wallet.drain"), "{}", err.message);
}

#[test]
fn no_capability_token_means_no_answer() {
    let h = Harness::start("notoken");
    let err = Client::connect(&h.wallet_socket)
        .unwrap()
        .call("wallet.status", json!({}))
        .unwrap()
        .expect_err("an unauthenticated caller gets nothing");
    assert!(err.message.contains("capability token"), "{}", err.message);
}

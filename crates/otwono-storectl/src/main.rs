//! `otwono-storectl` — drive the content store from a terminal.
//!
//! Every guarded method needs a capability, so this asks `otwono-permd` for one and presents
//! it, exactly as any other client must. That is the point of it being a real client and not
//! a back door: if policy does not grant the action, this fails the same way anything else
//! would.
//!
//! Two audiences, as with `otwono-aictl`. A person, who otherwise has no way to put a file
//! in the store without hand-writing JSON-RPC into a socket; and the boot-time content
//! check, which needs to do exactly that in a shell script and would otherwise need `socat`
//! in the base image.
//!
//! # Small and large are different commands on purpose
//!
//! `put` and `get` carry bytes inline and are capped at `MAX_INLINE_BYTES` by the control
//! plane's line limit. `import` and `export` move a file and put only a path on the socket
//! (ADR-0018). This tool does **not** hide the difference behind one command that guesses:
//! an export leaves plaintext on disk that the caller has to unlink, and a command that
//! silently sometimes does that is a command that surprises somebody.
//!
//! ```text
//! otwono-storectl put --file notes.md --visibility private
//! otwono-storectl get <CONTENT_ID> --out notes.md
//! otwono-storectl import --file film.mkv --visibility public
//! otwono-storectl export <CONTENT_ID>
//! otwono-storectl stat <CONTENT_ID>
//! otwono-storectl demote <CONTENT_ID> --visibility private
//! otwono-storectl cache-status
//! otwono-storectl cache-purge
//! ```

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use otwono_proto::Client;
use serde_json::{json, Value};

const USAGE: &str = "\
otwono-storectl — command-line access to the OTWONO content store

USAGE:
    otwono-storectl <COMMAND> [OPTIONS]

COMMANDS:
    put                       Store a file's bytes inline (small objects)
    get <CONTENT_ID>          Read an object inline (small objects)
    import                    Store a large file by path; the bytes never touch the socket
    export <CONTENT_ID>       Write an object out as a file this user owns
    stat <CONTENT_ID>         Size, chunk count, label, and whether every chunk is present
    demote <CONTENT_ID>       Make an object more restrictive. Widening needs a person.
    share                     Encrypt a file to named recipients and store it (ADR-0019)
    open <CONTENT_ID>         Open an object shared with this node, writing the plaintext
    grant <CONTENT_ID>        Add recipients to an object this node can open
    revoke <CONTENT_ID>       Delete recipients' copies of the key. Recalls nothing.
    cache-status              The cluster cache's budget, usage and contents
    cache-purge               Empty the cluster cache. The node's own store is untouched.

OPTIONS:
    --socket <PATH>           Store daemon socket (default $OTWONO_SOCKET_DIR/store.sock)
    --perm-socket <PATH>      Permission broker socket (default $OTWONO_SOCKET_DIR/perm.sock)
    --file <PATH>             Input file, for put and import
    --out <PATH>              Where get writes its bytes; stdout is never used for binary
    --visibility <LABEL>      private (default), shared, public or replicated
    --derived-from <ID>       An object this content was derived from. Repeatable.
    --recipient <SELF|PATH>   Who may open a shared object. `self` asks this node's identity
                              daemon for its own binding; a path reads one from a file.
                              Repeatable; share and grant need at least one.
    --node <NODE_ID>          Who to revoke. Repeatable; revoke needs at least one.
    --id-socket <PATH>        Identity daemon socket (default $OTWONO_SOCKET_DIR/id.sock),
                              used only to resolve `--recipient self`
    --json                    Print the daemon's reply verbatim
    -h, --help                Show this message

SHARING:
    A shared object is encrypted before it is chunked, so its bytes on the wire and on any
    other node are ciphertext, and its content id is over that ciphertext. It therefore does
    not deduplicate, and sharing one file with two recipient sets makes two unrelated
    objects. A recipient is named by a *signed* binding: a bare key says nothing about whose
    it is. Removing a recipient later deletes their copy of the key and nothing else -- they
    may already hold both the ciphertext and their key.

INLINE VERSUS FILE:
    put and get carry the bytes on the control plane and are capped at its line limit.
    import and export move a file and send only a path (ADR-0018). An exported file is
    PLAINTEXT even though the store is encrypted at rest: read it and unlink it.

EXIT CODES:
    0  the call succeeded
    1  usage error
    2  the daemon refused, or could not be reached
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(Error::Usage(m)) => {
            eprintln!("otwono-storectl: {m}\n\n{USAGE}");
            ExitCode::from(1)
        }
        Err(Error::Runtime(m)) => {
            eprintln!("otwono-storectl: {m}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug)]
enum Error {
    Usage(String),
    Runtime(String),
}

#[derive(Debug, Default, PartialEq)]
struct Options {
    command: String,
    target: Option<String>,
    socket: Option<PathBuf>,
    perm_socket: Option<PathBuf>,
    file: Option<PathBuf>,
    out: Option<PathBuf>,
    visibility: Option<String>,
    derived_from: Vec<String>,
    recipients: Vec<String>,
    nodes: Vec<String>,
    id_socket: Option<PathBuf>,
    json: bool,
}

fn run(args: &[String]) -> Result<String, Error> {
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(USAGE.to_string());
    }
    let opts = parse_args(args)?;
    let store_socket = opts
        .socket
        .clone()
        .unwrap_or_else(|| otwono_proto::socket_path("store"));
    let perm_socket = opts
        .perm_socket
        .clone()
        .unwrap_or_else(|| otwono_proto::socket_path("perm"));

    let recipients = resolve_recipients(&opts)?;
    let (method, params, action) = build_call(&opts, &recipients)?;
    let value = call(&store_socket, &perm_socket, &method, params, action)?;

    // `get` returns base64 on the wire. Writing it to a named file rather than stdout is
    // deliberate: a terminal is not a place to put arbitrary bytes, and a shell script that
    // captured them would have to decode them itself.
    // `open` returns the plaintext base64, like `get`, and for the same reason is written
    // to a file rather than a terminal.
    if opts.command == "get" || opts.command == "open" {
        let out = opts
            .out
            .clone()
            .ok_or_else(|| Error::Usage(format!("{} needs --out", opts.command)))?;
        let data = value
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Runtime("the daemon returned no data".into()))?;
        let bytes = data_encoding::BASE64
            .decode(data.as_bytes())
            .map_err(|e| Error::Runtime(format!("the daemon's data is not base64: {e}")))?;
        std::fs::write(&out, &bytes).map_err(|e| Error::Runtime(format!("{}: {e}", out.display())))?;
        if opts.json {
            return render_json(&value);
        }
        return Ok(format!(
            "{} {} bytes -> {}\n",
            value.get("content_id").and_then(Value::as_str).unwrap_or("?"),
            bytes.len(),
            out.display()
        ));
    }

    if opts.json {
        return render_json(&value);
    }
    Ok(render(&opts.command, &value))
}

fn render_json(value: &Value) -> Result<String, Error> {
    serde_json::to_string_pretty(value)
        .map(|s| s + "\n")
        .map_err(|e| Error::Runtime(e.to_string()))
}

/// Turn each `--recipient` into a signed binding the daemon will accept.
///
/// `self` asks this node's identity daemon for its own binding; anything else is a path to
/// one somebody handed over. Either way what reaches `otwono-stored` is a **signed** binding,
/// which it verifies for itself — this resolves a name, it does not vouch for a key.
fn resolve_recipients(opts: &Options) -> Result<Vec<Value>, Error> {
    let mut out = Vec::with_capacity(opts.recipients.len());
    for name in &opts.recipients {
        if name == "self" {
            let socket = opts
                .id_socket
                .clone()
                .unwrap_or_else(|| otwono_proto::socket_path("id"));
            let mut client = otwono_proto::Client::connect(&socket).map_err(|e| {
                Error::Runtime(format!(
                    "cannot reach the identity daemon at {}: {e}",
                    socket.display()
                ))
            })?;
            let value = client
                .call("id.sharing_binding", json!({}))
                .map_err(|e| Error::Runtime(format!("id.sharing_binding failed: {e}")))?
                .map_err(|e| Error::Runtime(format!("id.sharing_binding refused: {}", e.message)))?;
            out.push(value);
        } else {
            let text = std::fs::read_to_string(name).map_err(|e| Error::Runtime(format!("{name}: {e}")))?;
            out.push(
                serde_json::from_str(&text)
                    .map_err(|e| Error::Runtime(format!("{name} is not a sharing binding: {e}")))?,
            );
        }
    }
    Ok(out)
}

/// Turn parsed options into the call to make: method, params, and the capability it needs.
///
/// Separated from the socket work so every command's request shape is unit-testable without
/// a daemon anywhere.
fn build_call(opts: &Options, recipients: &[Value]) -> Result<(String, Value, Option<&'static str>), Error> {
    let need_target = |what: &str| -> Result<String, Error> {
        opts.target
            .clone()
            .ok_or_else(|| Error::Usage(format!("{what} needs a content id")))
    };
    let need_file = |what: &str| -> Result<PathBuf, Error> {
        opts.file
            .clone()
            .ok_or_else(|| Error::Usage(format!("{what} needs --file")))
    };
    let visibility = opts.visibility.clone().unwrap_or_else(|| "private".into());

    Ok(match opts.command.as_str() {
        "put" => {
            let path = need_file("put")?;
            let bytes =
                std::fs::read(&path).map_err(|e| Error::Runtime(format!("{}: {e}", path.display())))?;
            // Checked here as well as in the daemon, so the answer is a sentence rather
            // than a refused socket line, and so the file is not read twice to find out.
            if bytes.len() > otwono_stored::MAX_INLINE_BYTES {
                return Err(Error::Usage(format!(
                    "{} is {} bytes, over the {}-byte inline cap; use `import`, which sends \
                     the path instead of the bytes",
                    path.display(),
                    bytes.len(),
                    otwono_stored::MAX_INLINE_BYTES
                )));
            }
            (
                "store.put".into(),
                json!({
                    "data": data_encoding::BASE64.encode(&bytes),
                    "visibility": visibility,
                    "derived_from": opts.derived_from,
                }),
                Some("store.write"),
            )
        }
        "get" => (
            "store.get".into(),
            json!({ "content_id": need_target("get")? }),
            Some("store.read"),
        ),
        "import" => {
            let path = need_file("import")?;
            // Made absolute here rather than in the daemon: the daemon's working directory
            // is not the caller's, and a relative path would silently mean something else.
            let path = std::fs::canonicalize(&path)
                .map_err(|e| Error::Runtime(format!("{}: {e}", path.display())))?;
            (
                "store.import".into(),
                json!({
                    "path": path.display().to_string(),
                    "visibility": visibility,
                    "derived_from": opts.derived_from,
                }),
                Some("store.write"),
            )
        }
        "export" => (
            "store.export".into(),
            json!({ "content_id": need_target("export")? }),
            Some("store.read"),
        ),
        "stat" => (
            "store.stat".into(),
            json!({ "content_id": need_target("stat")? }),
            Some("store.read"),
        ),
        "demote" => (
            "store.demote".into(),
            json!({
                "content_id": need_target("demote")?,
                "visibility": opts
                    .visibility
                    .clone()
                    .ok_or_else(|| Error::Usage("demote needs --visibility".into()))?,
            }),
            Some("store.write"),
        ),
        "share" => {
            let path = need_file("share")?;
            let bytes =
                std::fs::read(&path).map_err(|e| Error::Runtime(format!("{}: {e}", path.display())))?;
            if bytes.len() > otwono_stored::MAX_INLINE_BYTES {
                return Err(Error::Usage(format!(
                    "{} is {} bytes, over the {}-byte inline cap; there is no file variant \
                     of share on this CLI yet",
                    path.display(),
                    bytes.len(),
                    otwono_stored::MAX_INLINE_BYTES
                )));
            }
            if recipients.is_empty() {
                return Err(Error::Usage(
                    "share needs at least one --recipient; an object nobody can open is not \
                     a shared object"
                        .into(),
                ));
            }
            (
                "store.share".into(),
                json!({
                    "data": data_encoding::BASE64.encode(&bytes),
                    "recipients": recipients,
                }),
                Some("store.share"),
            )
        }
        // Guarded by id.unwrap_shared, not store.read: opening what somebody shared with
        // this node *is* the unwrap, and otwono-stored forwards this same token to
        // otwono-idd. See ADR-0019 §3.
        "open" => (
            "store.open_shared".into(),
            json!({ "content_id": need_target("open")? }),
            Some("id.unwrap_shared"),
        ),
        // Also id.unwrap_shared: adding a recipient means re-wrapping the content key, and
        // the only way to have it is to open the object first (ADR-0019 §5).
        "grant" => {
            let target = need_target("grant")?;
            if recipients.is_empty() {
                return Err(Error::Usage("grant needs at least one --recipient".into()));
            }
            (
                "store.add_recipients".into(),
                json!({ "content_id": target, "recipients": recipients }),
                Some("id.unwrap_shared"),
            )
        }
        "revoke" => {
            let target = need_target("revoke")?;
            if opts.nodes.is_empty() {
                return Err(Error::Usage("revoke needs at least one --node".into()));
            }
            (
                "store.remove_recipients".into(),
                json!({ "content_id": target, "node_ids": opts.nodes }),
                Some("store.write"),
            )
        }
        "cache-status" => ("cache.status".into(), json!({}), Some("cache.read")),
        "cache-purge" => ("cache.purge".into(), json!({}), Some("cache.write")),
        other => return Err(Error::Usage(format!("unknown command {other:?}"))),
    })
}

fn call(
    store_socket: &Path,
    perm_socket: &Path,
    method: &str,
    params: Value,
    action: Option<&str>,
) -> Result<Value, Error> {
    // Generous, because this runs at boot where the daemons may still be starting.
    let wait = Duration::from_secs(30);

    let token = match action {
        None => None,
        Some(action) => {
            let mut perm = Client::connect_waiting(perm_socket, wait).map_err(|e| {
                Error::Runtime(format!(
                    "cannot reach the permission broker at {}: {e}",
                    perm_socket.display()
                ))
            })?;
            let granted = perm
                .call(
                    "perm.request",
                    json!({ "action": action, "reason": format!("otwono-storectl {method}") }),
                )
                .map_err(|e| Error::Runtime(format!("perm.request transport failure: {e}")))?
                .map_err(|e| Error::Runtime(format!("policy refuses {action}: {}", e.message)))?;
            Some(
                granted
                    .get("token")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| Error::Runtime("broker returned no token".into()))?
                    .to_string(),
            )
        }
    };

    let mut client = Client::connect_waiting(store_socket, wait).map_err(|e| {
        Error::Runtime(format!(
            "cannot reach the content store at {}: {e}",
            store_socket.display()
        ))
    })?;
    let outcome = match &token {
        Some(t) => client.call_with_capability(method, params, t),
        None => client.call(method, params),
    };
    outcome
        .map_err(|e| Error::Runtime(format!("{method} transport failure: {e}")))?
        .map_err(|e| Error::Runtime(format!("{method} refused: {}", e.message)))
}

fn render(command: &str, v: &Value) -> String {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("?").to_string();
    let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    match command {
        "put" | "import" => {
            let mut out = format!(
                "{} {} bytes {}\n",
                s("content_id"),
                n("size_bytes"),
                s("visibility")
            );
            // Said plainly, because a caller that asked for public over a private input got
            // private and needs to know rather than assume.
            if let Some(asked) = v.get("requested_visibility").and_then(Value::as_str) {
                if asked != s("visibility") {
                    out.push_str(&format!(
                        "note: asked for {asked}, stored as {} — a derived object cannot be \
                         less restrictive than what it came from\n",
                        s("visibility")
                    ));
                }
            }
            out
        }
        "share" => {
            let who: Vec<&str> = v["sharing"]["authorized"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            format!(
                "{} {} bytes shared\nplaintext {} bytes, sealed to {}\nnote: removing a \
                 recipient later deletes their copy of the key and nothing else\n",
                s("content_id"),
                n("size_bytes"),
                v["sharing"]["plaintext_size_bytes"].as_u64().unwrap_or(0),
                if who.is_empty() {
                    "nobody".to_string()
                } else {
                    who.join(", ")
                }
            )
        }
        "grant" => {
            let who: Vec<&str> = v["sharing"]["authorized"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            format!("{} now sealed to {}\n", s("content_id"), who.join(", "))
        }
        "revoke" => {
            let removed: Vec<&str> = v["removed"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let remaining: Vec<&str> = v["sharing"]["authorized"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if removed.is_empty() {
                format!("{} unchanged; none of those were recipients\n", s("content_id"))
            } else {
                format!(
                    "{} removed {}\nstill sealed to {}\nnote: {}\n",
                    s("content_id"),
                    removed.join(", "),
                    remaining.join(", "),
                    v["note"].as_str().unwrap_or("")
                )
            }
        }
        "export" => format!(
            "{} {} bytes -> {}\nnote: that file is plaintext and yours; read it and unlink it\n",
            s("content_id"),
            n("exported_bytes"),
            s("path")
        ),
        "stat" => format!(
            "{} {} bytes {} chunks {} complete={}\n",
            s("content_id"),
            n("size_bytes"),
            n("chunks"),
            s("visibility"),
            v.get("complete").and_then(Value::as_bool).unwrap_or(false)
        ),
        "demote" => format!(
            "{} now {}\nnote: {}\n",
            s("content_id"),
            s("visibility"),
            s("note")
        ),
        "cache-status" => {
            let mut out = format!(
                "budget {} bytes, used {}, {} object(s), {} replica(s) held\n",
                n("budget_bytes"),
                n("used_bytes"),
                n("objects"),
                n("replicas_held")
            );
            for e in v.get("entries").and_then(Value::as_array).into_iter().flatten() {
                // Two independent flags, shown as two words. A pinned entry the operator
                // chose to keep and a replica this node promised a peer are different
                // commitments with different ends, and an entry can be both.
                let mut flags = String::new();
                if e.get("pinned").and_then(Value::as_bool).unwrap_or(false) {
                    flags.push_str(" pinned");
                }
                if e.get("holds_a_replica").and_then(Value::as_bool).unwrap_or(false) {
                    flags.push_str(" replica");
                }
                out.push_str(&format!(
                    "  {} {} bytes{}\n",
                    e.get("content_id").and_then(Value::as_str).unwrap_or("?"),
                    e.get("size_bytes").and_then(Value::as_u64).unwrap_or(0),
                    flags
                ));
            }
            out.push_str(&format!("note: {}\n", s("note")));
            out
        }
        "cache-purge" => format!("freed {} bytes\nnote: {}\n", n("freed_bytes"), s("note")),
        _ => format!("{v}\n"),
    }
}

fn parse_args(args: &[String]) -> Result<Options, Error> {
    let mut opts = Options {
        command: args[0].clone(),
        ..Default::default()
    };
    if opts.command.starts_with('-') {
        return Err(Error::Usage(format!("expected a command, got {}", opts.command)));
    }
    let mut it = args[1..].iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => opts.socket = Some(next(&mut it, "--socket")?.into()),
            "--perm-socket" => opts.perm_socket = Some(next(&mut it, "--perm-socket")?.into()),
            "--file" => opts.file = Some(next(&mut it, "--file")?.into()),
            "--out" => opts.out = Some(next(&mut it, "--out")?.into()),
            "--visibility" => opts.visibility = Some(next(&mut it, "--visibility")?),
            "--derived-from" => opts.derived_from.push(next(&mut it, "--derived-from")?),
            "--recipient" => opts.recipients.push(next(&mut it, "--recipient")?),
            "--node" => opts.nodes.push(next(&mut it, "--node")?),
            "--id-socket" => opts.id_socket = Some(next(&mut it, "--id-socket")?.into()),
            "--json" => opts.json = true,
            other if other.starts_with('-') => return Err(Error::Usage(format!("unknown option {other}"))),
            positional => {
                if opts.target.is_some() {
                    return Err(Error::Usage(format!("unexpected argument {positional}")));
                }
                opts.target = Some(positional.to_string());
            }
        }
    }
    Ok(opts)
}

fn next<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<String, Error> {
    it.next()
        .cloned()
        .ok_or_else(|| Error::Usage(format!("{flag} needs a value")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> Options {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).expect("parses")
    }

    #[test]
    fn each_command_asks_for_the_capability_it_needs() {
        // The table that matters: a command that asked for a wider capability than it needs
        // would quietly widen what a person has to be granted to use this tool.
        for (args, method, action) in [
            (vec!["get", "ab"], "store.get", Some("store.read")),
            (vec!["stat", "ab"], "store.stat", Some("store.read")),
            (vec!["export", "ab"], "store.export", Some("store.read")),
            (vec!["cache-status"], "cache.status", Some("cache.read")),
            (vec!["cache-purge"], "cache.purge", Some("cache.write")),
            // Opening asks for the unwrap capability, not store.read. otwono-stored
            // forwards this very token to otwono-idd, and a token names one action -- so
            // asking for store.read here would make every open fail, and "fixing" that by
            // having the store request its own token would let anyone with store.read open
            // everything shared with this node.
            (vec!["open", "ab"], "store.open_shared", Some("id.unwrap_shared")),
        ] {
            let (m, _, a) = build_call(&opts(&args), &[]).expect("builds");
            assert_eq!(m, method);
            assert_eq!(a, action, "{args:?}");
        }
    }

    #[test]
    fn reading_never_needs_a_write_capability() {
        for args in [vec!["get", "ab"], vec!["stat", "ab"], vec!["export", "ab"]] {
            let (_, _, action) = build_call(&opts(&args), &[]).unwrap();
            assert_eq!(action, Some("store.read"), "{args:?}");
        }
    }

    #[test]
    fn the_default_label_is_private() {
        // Fail closed, here as everywhere else: a person who forgets --visibility gets the
        // label that does not leave the node.
        let dir = std::env::temp_dir().join(format!("otwono-storectl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("f");
        std::fs::write(&f, b"x").unwrap();
        let (_, params, _) = build_call(&opts(&["put", "--file", f.to_str().unwrap()]), &[]).unwrap();
        assert_eq!(params["visibility"], "private");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn put_refuses_a_file_too_large_for_the_socket_and_names_import() {
        let dir = std::env::temp_dir().join(format!("otwono-storectl-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("big");
        std::fs::write(&f, vec![0u8; otwono_stored::MAX_INLINE_BYTES + 1]).unwrap();
        let err = build_call(&opts(&["put", "--file", f.to_str().unwrap()]), &[]).unwrap_err();
        match err {
            Error::Usage(m) => assert!(m.contains("import"), "{m}"),
            other => panic!("expected a usage error naming import, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sharing_with_nobody_is_a_usage_error_not_an_unopenable_object() {
        let dir = std::env::temp_dir().join(format!("otwono-storectl-share-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("f");
        std::fs::write(&f, b"x").unwrap();
        let err = build_call(&opts(&["share", "--file", f.to_str().unwrap()]), &[]).unwrap_err();
        match err {
            Error::Usage(m) => assert!(m.contains("--recipient"), "{m}"),
            other => panic!("expected a usage error naming --recipient, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn share_sends_the_bindings_it_was_given_and_asks_for_store_share() {
        let dir = std::env::temp_dir().join(format!("otwono-storectl-shb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("f");
        std::fs::write(&f, b"hello").unwrap();
        let binding = json!({ "node_id": "otw1someone", "public_key": "a", "sharing_public_key": "b", "signature": "c" });
        let (method, params, action) = build_call(
            &opts(&["share", "--file", f.to_str().unwrap()]),
            std::slice::from_ref(&binding),
        )
        .unwrap();
        assert_eq!(method, "store.share");
        assert_eq!(action, Some("store.share"));
        // Verbatim. This CLI resolves a name to a signed binding; it does not vouch for a
        // key, and the daemon checks the signature for itself.
        assert_eq!(params["recipients"], json!([binding]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn granting_to_nobody_is_a_usage_error() {
        let err = build_call(&opts(&["grant", "abc"]), &[]).unwrap_err();
        match err {
            Error::Usage(m) => assert!(m.contains("--recipient"), "{m}"),
            other => panic!("expected a usage error naming --recipient, got {other:?}"),
        }
    }

    #[test]
    fn revoking_nobody_is_a_usage_error_not_a_no_op() {
        // A `revoke` that quietly did nothing would read, in a shell history, exactly like a
        // revoke that worked.
        let err = build_call(&opts(&["revoke", "abc"]), &[]).unwrap_err();
        match err {
            Error::Usage(m) => assert!(m.contains("--node"), "{m}"),
            other => panic!("expected a usage error naming --node, got {other:?}"),
        }
    }

    #[test]
    fn grant_asks_for_the_unwrap_capability_and_revoke_does_not() {
        // Not symmetry for its own sake: widening needs the content key, which needs
        // opening the object; narrowing needs no key at all (ADR-0019 §5b).
        let binding = json!({ "node_id": "otw1someone", "public_key": "a", "sharing_public_key": "b", "signature": "c" });
        let (method, params, action) =
            build_call(&opts(&["grant", "abc"]), std::slice::from_ref(&binding)).unwrap();
        assert_eq!(method, "store.add_recipients");
        assert_eq!(action, Some("id.unwrap_shared"));
        assert_eq!(params["recipients"], json!([binding]));

        let (method, params, action) =
            build_call(&opts(&["revoke", "abc", "--node", "otw1someone"]), &[]).unwrap();
        assert_eq!(method, "store.remove_recipients");
        assert_eq!(action, Some("store.write"));
        assert_eq!(params["node_ids"], json!(["otw1someone"]));
    }

    #[test]
    fn demote_without_a_label_is_a_usage_error_not_a_guess() {
        // Guessing here would mean picking how restrictive to make somebody's data.
        assert!(matches!(
            build_call(&opts(&["demote", "abc"]), &[]),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn an_unknown_command_is_refused() {
        assert!(matches!(
            build_call(&opts(&["frobnicate"]), &[]),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn derived_from_is_repeatable() {
        let dir = std::env::temp_dir().join(format!("otwono-storectl-d-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("f");
        std::fs::write(&f, b"x").unwrap();
        let (_, params, _) = build_call(
            &opts(&[
                "put",
                "--file",
                f.to_str().unwrap(),
                "--derived-from",
                "aa",
                "--derived-from",
                "bb",
            ]),
            &[],
        )
        .unwrap();
        assert_eq!(params["derived_from"], json!(["aa", "bb"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_usage_text_says_an_export_is_plaintext() {
        // The one thing a person must not learn by surprise.
        assert!(USAGE.contains("PLAINTEXT"), "{USAGE}");
    }
}

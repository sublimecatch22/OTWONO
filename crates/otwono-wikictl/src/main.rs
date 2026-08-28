//! `otwono-wikictl` — write and read wiki pages from a terminal.
//!
//! The service half of ADR-0032. The crate next door holds the record and its rules and
//! touches nothing; this is what turns a file into a chain and a chain back into a file, over
//! the control plane, asking `otwono-permd` for a capability like any other client.
//!
//! ```text
//! otwono-wikictl write Getting-Started --file page.md
//! otwono-wikictl read  Getting-Started --out page.md
//! otwono-wikictl history Getting-Started
//! ```
//!
//! # Writing is four calls, and cannot be three shell invocations
//!
//! Put the body, build the revision, have `otwono-idd` sign it, put the revision, move the
//! pointer. Splitting that across commands would put an unsigned record on a command line
//! between them — the same reason `otwono-storectl pointer-publish` is one command and not
//! three (ADR-0027).
//!
//! # Reading someone else's page is not here
//!
//! This reads pages in *this node's* namespace, from this node's store. Fetching a peer's
//! page is `content.pointer` plus the fetch path and belongs to `otwono-netd`; WIKI.md §6
//! says so and this file does not pretend otherwise.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use otwono_proto::Client;
use serde_json::{json, Value};

/// The service namespace a wiki page's pointer lives in (ADR-0032).
const SERVICE: &str = "wiki";

/// How far back `history` walks unless told otherwise.
///
/// A bound, not a preference: how long a page's history is is decided by whoever wrote it,
/// and a walk with no limit follows it for as long as the store keeps answering.
const DEFAULT_HISTORY: usize = 64;

const USAGE: &str = "\
otwono-wikictl — wiki pages as signed chains of revisions

  otwono-wikictl write <PAGE> --file <PATH> [--visibility public|private]
  otwono-wikictl read <PAGE> --out <PATH>
  otwono-wikictl history <PAGE> [--limit N]

Options:
  --socket PATH        otwono-stored's socket
  --perm-socket PATH   otwono-permd's socket
  --id-socket PATH     otwono-idd's socket
  --json               machine-readable output
";

fn main() -> ExitCode {
    match run() {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(Error::Usage(why)) => {
            eprintln!("otwono-wikictl: {why}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Error::Runtime(why)) => {
            eprintln!("otwono-wikictl: {why}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug)]
enum Error {
    Usage(String),
    Runtime(String),
}

#[derive(Debug, Default)]
struct Options {
    command: String,
    page: Option<String>,
    file: Option<PathBuf>,
    out: Option<PathBuf>,
    visibility: String,
    limit: Option<usize>,
    socket: Option<PathBuf>,
    perm_socket: Option<PathBuf>,
    id_socket: Option<PathBuf>,
    json: bool,
}

fn parse(args: &mut dyn Iterator<Item = String>) -> Result<Options, Error> {
    let mut o = Options {
        visibility: "public".into(),
        ..Default::default()
    };
    let mut it = args.peekable();
    o.command = it.next().ok_or_else(|| Error::Usage("no command".into()))?;
    if o.command == "-h" || o.command == "--help" {
        return Err(Error::Usage("".into()));
    }
    // The page name is positional and comes before the flags, so `write --file x Page` is a
    // usage error rather than a page called "--file".
    if let Some(next) = it.peek() {
        if !next.starts_with("--") {
            o.page = it.next();
        }
    }
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| -> Result<String, Error> {
            it.next()
                .ok_or_else(|| Error::Usage(format!("{flag} needs a value")))
        };
        match arg.as_str() {
            "--file" => o.file = Some(value("--file")?.into()),
            "--out" => o.out = Some(value("--out")?.into()),
            "--visibility" => o.visibility = value("--visibility")?,
            "--limit" => {
                o.limit = Some(
                    value("--limit")?
                        .parse()
                        .map_err(|_| Error::Usage("--limit needs a number".into()))?,
                )
            }
            "--socket" => o.socket = Some(value("--socket")?.into()),
            "--perm-socket" => o.perm_socket = Some(value("--perm-socket")?.into()),
            "--id-socket" => o.id_socket = Some(value("--id-socket")?.into()),
            "--json" => o.json = true,
            "-h" | "--help" => return Err(Error::Usage("".into())),
            other => return Err(Error::Usage(format!("unknown option {other}"))),
        }
    }
    Ok(o)
}

fn run() -> Result<String, Error> {
    let mut args = std::env::args().skip(1);
    let opts = parse(&mut args)?;
    let store = opts
        .socket
        .clone()
        .unwrap_or_else(|| otwono_proto::socket_path("store"));
    let perm = opts
        .perm_socket
        .clone()
        .unwrap_or_else(|| otwono_proto::socket_path("perm"));

    match opts.command.as_str() {
        "write" => write_page(&opts, &store, &perm),
        "read" => read_page(&opts, &store, &perm),
        "history" => show_history(&opts, &store, &perm),
        other => Err(Error::Usage(format!("unknown command {other}"))),
    }
}

fn page_of(opts: &Options) -> Result<String, Error> {
    opts.page
        .clone()
        .ok_or_else(|| Error::Usage(format!("{} needs a page name", opts.command)))
}

/// Put the body, sign a revision naming the old head as its parent, move the pointer.
///
/// The order matters and is not the obvious one. The pointer moves **last**, because it is
/// what anyone else reads: a pointer advanced before its revision was stored would name an id
/// this node cannot serve, and a reader would see a page that exists and cannot be opened.
/// Every step before it is invisible to everyone until that last call.
fn write_page(opts: &Options, store: &Path, perm: &Path) -> Result<String, Error> {
    let page = page_of(opts)?;
    let path = opts
        .file
        .clone()
        .ok_or_else(|| Error::Usage("write needs --file".into()))?;
    let body_bytes = std::fs::read(&path).map_err(|e| Error::Runtime(format!("{}: {e}", path.display())))?;

    let body = put_bytes(store, perm, &body_bytes, &opts.visibility)?;

    // Whatever the pointer names now becomes this revision's parent. A page that does not
    // exist yet resolves to nothing, which is a first revision rather than an error.
    let parent = current_head(opts, store, perm, &page)?;

    let author = node_id_of_this_node(opts)?;
    let mut revision = otwono_wiki::Revision::new(
        &author,
        page.clone(),
        body.clone(),
        parent.clone(),
        otwono_identity::now_unix_ms(),
    );
    // Checked before it is signed, so a name a reader would refuse cannot be committed to.
    revision.check_shape().map_err(|e| Error::Usage(e.to_string()))?;
    revision.signature = sign(opts, perm, &revision.payload_for_id_sign().map_err(runtime)?)?;

    let encoded = serde_json::to_vec(&revision).map_err(runtime)?;
    let head = put_bytes(store, perm, &encoded, &opts.visibility)?;

    publish(opts, store, perm, &page, Some(head.clone()))?;

    if opts.json {
        return Ok(format!(
            "{}\n",
            serde_json::to_string(&json!({
                "page": page, "revision": head, "body": body, "parent": parent
            }))
            .map_err(runtime)?
        ));
    }
    Ok(format!(
        "{page} {head}\n  body {body}\n  parent {}\n",
        parent.unwrap_or_else(|| "none (first revision)".into())
    ))
}

/// Resolve the page, fetch the head revision, fetch its body.
fn read_page(opts: &Options, store: &Path, perm: &Path) -> Result<String, Error> {
    let page = page_of(opts)?;
    let out = opts
        .out
        .clone()
        .ok_or_else(|| Error::Usage("read needs --out".into()))?;
    let head = current_head(opts, store, perm, &page)?
        .ok_or_else(|| Error::Runtime(format!("no page called {page:?} on this node")))?;
    let revision = revision_at(store, perm, &head)?;
    if revision.page != page {
        return Err(Error::Runtime(format!(
            "the pointer for {page:?} names a revision of {:?}",
            revision.page
        )));
    }
    let body = get_bytes(store, perm, &revision.body)?;
    std::fs::write(&out, &body).map_err(|e| Error::Runtime(format!("{}: {e}", out.display())))?;

    if opts.json {
        return Ok(format!(
            "{}\n",
            serde_json::to_string(&json!({
                "page": page, "revision": head, "body": revision.body, "bytes": body.len()
            }))
            .map_err(runtime)?
        ));
    }
    Ok(format!(
        "{page} {head} -> {} ({} bytes)\n",
        out.display(),
        body.len()
    ))
}

/// Walk the chain from the head, verifying every step.
fn show_history(opts: &Options, store: &Path, perm: &Path) -> Result<String, Error> {
    let page = page_of(opts)?;
    let head = current_head(opts, store, perm, &page)?
        .ok_or_else(|| Error::Runtime(format!("no page called {page:?} on this node")))?;

    // This node's own pages, so this node's own key answers for every author. A revision
    // authored by somebody else — a page copied from a peer — is refused rather than shown
    // unverified, which is the right answer until there is somewhere to look their key up.
    let me = node_id_of_this_node(opts)?;
    let my_key = public_key_of_this_node(opts)?;
    let mine = me.to_text();

    let shelf = |id: &str| revision_at(store, perm, id).ok();
    let history = otwono_wiki::walk(
        &shelf,
        &head,
        &page,
        |author| (author == mine).then_some(my_key),
        opts.limit.unwrap_or(DEFAULT_HISTORY),
    )
    .map_err(|e| Error::Runtime(e.to_string()))?;

    if opts.json {
        let steps: Vec<Value> = history
            .steps
            .iter()
            .map(|s| json!({ "revision": s.content_id, "body": s.revision.body, "written_at_ms": s.revision.written_at_ms }))
            .collect();
        return Ok(format!(
            "{}\n",
            serde_json::to_string(
                &json!({ "page": page, "steps": steps, "end": format!("{:?}", history.end) })
            )
            .map_err(runtime)?
        ));
    }
    let mut out = String::new();
    for step in &history.steps {
        out.push_str(&format!(
            "{} body={} written_at_ms={}\n",
            step.content_id, step.revision.body, step.revision.written_at_ms
        ));
    }
    // How the walk ended is part of the answer, not a footnote: a list on its own cannot
    // tell a whole history from as much of one as this node happens to hold.
    out.push_str(&match &history.end {
        otwono_wiki::WalkEnd::Complete => "end: complete\n".to_string(),
        otwono_wiki::WalkEnd::Truncated { missing } => {
            format!("end: truncated; this node does not have {missing}\n")
        }
        otwono_wiki::WalkEnd::Limited => "end: stopped at the limit; there may be more\n".to_string(),
    });
    Ok(out)
}

// --- the control plane, and nothing clever -------------------------------------------------

fn runtime<E: std::fmt::Display>(e: E) -> Error {
    Error::Runtime(e.to_string())
}

fn put_bytes(store: &Path, perm: &Path, bytes: &[u8], visibility: &str) -> Result<String, Error> {
    let out = call(
        store,
        perm,
        "store.put",
        json!({
            "data": data_encoding::BASE64.encode(bytes),
            "visibility": visibility,
            "derived_from": Vec::<String>::new(),
        }),
        Some("store.write"),
    )?;
    out.get("content_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::Runtime("the store returned no content id".into()))
}

fn get_bytes(store: &Path, perm: &Path, content_id: &str) -> Result<Vec<u8>, Error> {
    let out = call(
        store,
        perm,
        "store.get",
        json!({ "content_id": content_id }),
        Some("store.read"),
    )?;
    let data = out
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Runtime(format!("the store returned no data for {content_id}")))?;
    data_encoding::BASE64
        .decode(data.as_bytes())
        .map_err(|e| Error::Runtime(format!("the store's data is not base64: {e}")))
}

fn revision_at(store: &Path, perm: &Path, content_id: &str) -> Result<otwono_wiki::Revision, Error> {
    let bytes = get_bytes(store, perm, content_id)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| Error::Runtime(format!("{content_id} is not a wiki revision: {e}")))
}

/// What this node's pointer for `page` names, or `None` if there is no page.
///
/// A tombstone reads as `None` too, and that is right: the owner said the page is gone, so
/// writing again starts a new chain rather than silently resurrecting the old one. The old
/// revisions are still content-addressed and reachable to anyone who kept an id, which
/// ADR-0027 §4 requires be said out loud rather than implied.
fn current_head(opts: &Options, store: &Path, perm: &Path, page: &str) -> Result<Option<String>, Error> {
    let _ = opts;
    let out = call(
        store,
        perm,
        "pointer.mine",
        json!({ "service": SERVICE, "name": page }),
        Some("store.serve"),
    )?;
    let Some(record) = out.get("record") else {
        return Ok(None);
    };
    if record.is_null() {
        return Ok(None);
    }
    let pointer: otwono_pointer::Pointer = serde_json::from_value(record.clone())
        .map_err(|e| Error::Runtime(format!("this node's pointer for {page:?} does not parse: {e}")))?;
    Ok(pointer.content_id)
}

/// Move the pointer: ask for the next sequence, sign, publish.
fn publish(
    opts: &Options,
    store: &Path,
    perm: &Path,
    page: &str,
    content_id: Option<String>,
) -> Result<(), Error> {
    let next = call(
        store,
        perm,
        "pointer.next_sequence",
        json!({ "service": SERVICE, "name": page }),
        Some("pointer.read"),
    )?
    .get("next_sequence")
    .and_then(Value::as_u64)
    .ok_or_else(|| Error::Runtime("the store returned no next_sequence".into()))?;

    let node_id = node_id_of_this_node(opts)?;
    let mut pointer = otwono_pointer::Pointer::new(
        &node_id,
        SERVICE,
        page,
        next,
        content_id,
        otwono_identity::now_unix_ms(),
    );
    pointer.signature = sign(opts, perm, &pointer.payload_for_id_sign().map_err(runtime)?)?;
    call(
        store,
        perm,
        "pointer.publish",
        json!({ "record": pointer }),
        Some("pointer.publish"),
    )?;
    Ok(())
}

fn sign(opts: &Options, perm: &Path, payload: &[u8]) -> Result<String, Error> {
    let id_socket = opts
        .id_socket
        .clone()
        .unwrap_or_else(|| otwono_proto::socket_path("id"));
    let out = call(
        &id_socket,
        perm,
        "id.sign",
        json!({ "payload": data_encoding::BASE64.encode(payload) }),
        Some("id.sign"),
    )?;
    out.get("signature")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::Runtime("otwono-idd returned no signature".into()))
}

fn node_id_of_this_node(opts: &Options) -> Result<otwono_identity::NodeId, Error> {
    let id_socket = opts
        .id_socket
        .clone()
        .unwrap_or_else(|| otwono_proto::socket_path("id"));
    let mut client = Client::connect(&id_socket)
        .map_err(|e| Error::Runtime(format!("cannot reach {}: {e}", id_socket.display())))?;
    let value = client
        .call("id.fingerprint", json!({}))
        .map_err(|e| Error::Runtime(format!("id.fingerprint: {e}")))?
        .map_err(|e| Error::Runtime(format!("id.fingerprint refused: {}", e.message)))?;
    let text = value
        .get("node_id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Runtime("id.fingerprint returned no node_id".into()))?;
    otwono_identity::NodeId::parse(text).map_err(runtime)
}

/// This node's Ed25519 public key, for verifying its own revisions.
///
/// `id.public_key` and not `id.node`: the latter builds a *publishable* identity and needs an
/// agreement key bound, which only `otwono-netd` does at startup — so reading your own wiki's
/// history would have depended on the mesh coming up, which `DISTRIBUTED-SERVICES.md` §4.1
/// refuses. Found by this failing in a test with no netd in it.
///
/// Over the control plane and not off `otwono-idd`'s disk, for the reason every other client
/// reads nothing off it either.
fn public_key_of_this_node(opts: &Options) -> Result<[u8; 32], Error> {
    let id_socket = opts
        .id_socket
        .clone()
        .unwrap_or_else(|| otwono_proto::socket_path("id"));
    let mut client = Client::connect(&id_socket)
        .map_err(|e| Error::Runtime(format!("cannot reach {}: {e}", id_socket.display())))?;
    let value = client
        .call("id.public_key", json!({}))
        .map_err(|e| Error::Runtime(format!("id.public_key: {e}")))?
        .map_err(|e| Error::Runtime(format!("id.public_key refused: {}", e.message)))?;
    let key = value
        .get("public_key")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Runtime("id.public_key returned no public_key".into()))?;
    let bytes = data_encoding::BASE64
        .decode(key.as_bytes())
        .map_err(|e| Error::Runtime(format!("id.public_key's answer is not base64: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| Error::Runtime("id.public_key's answer is not 32 bytes".into()))
}

fn call(
    socket: &Path,
    perm_socket: &Path,
    method: &str,
    params: Value,
    action: Option<&str>,
) -> Result<Value, Error> {
    // Generous, because this may run at boot while the daemons are still starting.
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
                    json!({ "action": action, "reason": format!("otwono-wikictl {method}") }),
                )
                .map_err(|e| Error::Runtime(format!("perm.request: {e}")))?
                .map_err(|e| Error::Runtime(format!("{action} refused: {}", e.message)))?;
            Some(
                granted
                    .get("token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Runtime("the broker granted no token".into()))?
                    .to_string(),
            )
        }
    };

    let mut client = Client::connect_waiting(socket, wait)
        .map_err(|e| Error::Runtime(format!("cannot reach {}: {e}", socket.display())))?;
    let reply = match token {
        Some(token) => client.call_with_capability(method, params, &token),
        None => client.call(method, params),
    }
    .map_err(|e| Error::Runtime(format!("{method}: {e}")))?;
    reply.map_err(|e| Error::Runtime(format!("{method} refused: {}", e.message)))
}

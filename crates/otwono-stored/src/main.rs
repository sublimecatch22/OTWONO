//! `otwono-stored` — the OTWONO content store daemon.

#![forbid(unsafe_code)]

use otwono_proto::{Server, Shutdown};
use otwono_store::{
    Cache, Handoff, StorageKey, Store, DEFAULT_CACHE_DIR, DEFAULT_EXPORT_DIR, DEFAULT_KEY_PATH,
    DEFAULT_STORE_DIR, EXPORT_MAX_AGE,
};
use otwono_stored::StoreService;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "\
otwono-stored — OTWONO content store daemon

USAGE:
    otwono-stored [OPTIONS]

OPTIONS:
    --socket <PATH>        Control-plane socket (default $OTWONO_SOCKET_DIR/store.sock)
    --perm-socket <PATH>   Permission broker socket (default $OTWONO_SOCKET_DIR/perm.sock)
    --id-socket <PATH>     Identity daemon socket (default $OTWONO_SOCKET_DIR/id.sock)
    --store-dir <PATH>     Where objects and chunks live (default /var/lib/otwono/store)
    --key <PATH>           Storage key, generated on first use (default /var/lib/otwono/storage.key)
    --cache-dir <PATH>     Neighbourhood cache (default /var/lib/otwono/cache)
    --cache-bytes <N>      Override the cache budget the capability profile chose
    --no-cache             Contribute no neighbourhood cache at all
    --export-dir <PATH>    Where large objects are handed over (default /var/lib/otwono/export)
    -h, --help             Show this message

THE NEIGHBOURHOOD CACHE:
    A bounded, encrypted slice of disk holding PUBLIC and REPLICATED content fetched from
    peers, so neighbours can serve each other instead of each fetching from origin
    (ADR-0015). PRIVATE and SHARED never enter it, by any path.

    Its size comes from the capability profile published by otwono-hwd and from nowhere
    else. If otwono-hwd cannot be reached the cache is disabled rather than guessed at, and
    the node runs without one. --cache-bytes overrides the default; --no-cache skips the
    lookup entirely.

    Holding is publishing: serving a chunk tells your neighbours you hold it.

LARGE OBJECTS:
    The control plane is newline-delimited JSON with a 1 MiB line cap, so objects above
    640 KiB move as files instead (ADR-0018). store.export writes one into the export
    directory and gives it to the calling uid; store.import reads one back from a path the
    caller owns.

    An exported file is PLAINTEXT, even though the store is encrypted at rest. Read it and
    unlink it. Anything left behind is reaped after an hour.

EXIT CODES:
    0  clean shutdown
    1  usage error
    2  startup failure (unusable key, socket in use)
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(msg) => {
            if !msg.is_empty() {
                print!("{msg}");
            }
            ExitCode::SUCCESS
        }
        Err(Error::Usage(m)) => {
            eprintln!("otwono-stored: {m}\n\n{USAGE}");
            ExitCode::from(1)
        }
        Err(Error::Startup(m)) => {
            eprintln!("otwono-stored: {m}");
            ExitCode::from(2)
        }
    }
}

enum Error {
    Usage(String),
    Startup(String),
}

fn run(args: &[String]) -> Result<String, Error> {
    let mut socket: Option<PathBuf> = None;
    let mut perm_socket: Option<PathBuf> = None;
    let mut id_socket: Option<PathBuf> = None;
    let mut store_dir = PathBuf::from(DEFAULT_STORE_DIR);
    let mut key_path = PathBuf::from(DEFAULT_KEY_PATH);
    let mut cache_dir = PathBuf::from(DEFAULT_CACHE_DIR);
    let mut cache_bytes: Option<u64> = None;
    let mut want_cache = true;
    let mut export_dir = PathBuf::from(DEFAULT_EXPORT_DIR);

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => socket = Some(next_path(&mut it, "--socket")?),
            "--perm-socket" => perm_socket = Some(next_path(&mut it, "--perm-socket")?),
            "--id-socket" => id_socket = Some(next_path(&mut it, "--id-socket")?),
            "--store-dir" => store_dir = next_path(&mut it, "--store-dir")?,
            "--key" => key_path = next_path(&mut it, "--key")?,
            "--cache-dir" => cache_dir = next_path(&mut it, "--cache-dir")?,
            "--cache-bytes" => {
                let raw = it
                    .next()
                    .ok_or_else(|| Error::Usage("--cache-bytes needs a number".into()))?;
                cache_bytes = Some(
                    raw.parse()
                        .map_err(|e| Error::Usage(format!("--cache-bytes {raw:?}: {e}")))?,
                );
            }
            "--no-cache" => want_cache = false,
            "--export-dir" => export_dir = next_path(&mut it, "--export-dir")?,
            "-h" | "--help" => return Ok(USAGE.to_string()),
            other => return Err(Error::Usage(format!("unknown option {other}"))),
        }
    }

    // Fails closed. A store the node cannot encrypt is a store that would sit on disk in
    // the clear, and starting anyway would mean the node believes its data is protected
    // when it is not.
    let (key, generated) =
        StorageKey::load_or_generate(&key_path).map_err(|e| Error::Startup(format!("storage key: {e}")))?;
    if generated {
        // Said once, loudly. From here the key is the only thing between a stolen disk and
        // its contents, and nothing backs it up.
        eprintln!(
            "otwono-stored: generated a storage key at {}\n  \
             Everything in {} is encrypted with it. It is not backed up and not \
             hardware-sealed: losing this file loses the store.",
            key_path.display(),
            store_dir.display()
        );
    }

    let store = Store::encrypted(&store_dir, key);
    store
        .ensure_layout()
        .map_err(|e| Error::Startup(format!("{}: {e}", store_dir.display())))?;

    let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("store"));
    let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
    let id_socket = id_socket.unwrap_or_else(|| otwono_proto::socket_path("id"));
    let server = Server::bind(&socket)
        .map_err(|e| Error::Startup(format!("cannot bind {}: {e}", socket.display())))?;
    eprintln!(
        "otwono-stored: listening on {} (store {}, encrypted, broker {})",
        socket.display(),
        store_dir.display(),
        perm_socket.display()
    );

    // Swept once at startup, before anything can add to it. A daemon that restarted while
    // a caller held an export left plaintext behind, and the caller is not coming back for
    // it -- its socket died with the old process.
    let handoff = Handoff::new(&export_dir);
    handoff
        .ensure_layout()
        .map_err(|e| Error::Startup(format!("export directory: {e}")))?;
    match handoff.reap(EXPORT_MAX_AGE) {
        Ok(0) => {}
        Ok(n) => eprintln!(
            "otwono-stored: reaped {n} abandoned export(s) from {}",
            export_dir.display()
        ),
        Err(e) => eprintln!("otwono-stored: could not sweep {}: {e}", export_dir.display()),
    }
    // And then on a timer, for the life of the process. A sweep that only happens at startup
    // is one that does not happen on a daemon that stays up for a month.
    otwono_store::handoff::spawn_reaper(
        Handoff::new(&export_dir),
        otwono_store::handoff::REAP_INTERVAL,
        EXPORT_MAX_AGE,
    );

    let mut service = StoreService::new(store, perm_socket.clone())
        .with_handoff(handoff)
        .with_identity(id_socket);
    if want_cache {
        // The budget is the capability policy engine's decision and nobody else's
        // (CLAUDE.md §2.6). An override is an operator saying so on purpose; absent one,
        // ask otwono-hwd, and run without a cache if it cannot be reached rather than
        // inventing a number.
        let budget = match cache_bytes {
            Some(n) => {
                eprintln!("otwono-stored: cache budget overridden to {n} bytes");
                Some(n)
            }
            None => match cache_budget_from_profile(&perm_socket) {
                Ok(n) => Some(n),
                Err(e) => {
                    eprintln!(
                        "otwono-stored: no neighbourhood cache — cannot read this machine's \
                         capability profile ({e}). The node runs normally; it serves peers \
                         only its own PUBLIC and REPLICATED content."
                    );
                    None
                }
            },
        };
        match budget {
            Some(0) => eprintln!(
                "otwono-stored: no neighbourhood cache — this machine's capability profile \
                 sets its budget to zero"
            ),
            Some(n) => match Cache::at(&cache_dir, cache_key(&key_path)?, n) {
                Ok(cache) => {
                    eprintln!(
                        "otwono-stored: neighbourhood cache at {} ({} bytes, {} used)",
                        cache_dir.display(),
                        n,
                        cache.used_bytes()
                    );
                    service = service.with_cache(cache);
                }
                Err(e) => {
                    return Err(Error::Startup(format!(
                        "cannot open the neighbourhood cache at {}: {e}",
                        cache_dir.display()
                    )))
                }
            },
            None => {}
        }
    } else {
        eprintln!("otwono-stored: no neighbourhood cache (--no-cache)");
    }
    let service = Arc::new(service);
    server
        .serve(service, Shutdown::new())
        .map_err(|e| Error::Startup(format!("serve failed: {e}")))?;
    Ok(String::new())
}

/// The cache is encrypted with the same storage key as the store.
///
/// A second key would be a second thing to lose, and the threat it would defend against —
/// someone who has the store key but not the cache key — is not one that exists: both files
/// sit in the same directory, on the same disk, under the same daemon.
fn cache_key(key_path: &std::path::Path) -> Result<StorageKey, Error> {
    StorageKey::load_or_generate(key_path)
        .map(|(k, _)| k)
        .map_err(|e| Error::Startup(format!("storage key: {e}")))
}

/// Ask otwono-hwd what this machine may contribute, through the broker.
fn cache_budget_from_profile(perm_socket: &std::path::Path) -> Result<u64, String> {
    use std::time::Duration;
    let mut broker = otwono_proto::Client::connect_waiting(perm_socket, Duration::from_secs(10))
        .map_err(|e| format!("{}: {e}", perm_socket.display()))?;
    let token = broker
        .call(
            "perm.request",
            serde_json::json!({
                "action": "hw.read",
                "reason": "otwono-stored sizes the neighbourhood cache from the capability profile",
            }),
        )
        .map_err(|e| format!("perm.request: {e}"))?
        .map_err(|e| format!("perm.request refused: {}", e.message))?;
    let token = token
        .get("token")
        .and_then(|t| t.as_str())
        .ok_or("perm.request returned no token")?
        .to_string();

    let hw_socket = otwono_proto::socket_path("hw");
    let mut hwd = otwono_proto::Client::connect_waiting(&hw_socket, Duration::from_secs(10))
        .map_err(|e| format!("{}: {e}", hw_socket.display()))?;
    let profile = hwd
        .call_with_capability("hw.profile", serde_json::json!({}), &token)
        .map_err(|e| format!("hw.profile: {e}"))?
        .map_err(|e| format!("hw.profile refused: {}", e.message))?;
    profile
        .get("features")
        .and_then(|f| f.get("neighbourhood_cache_bytes"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "the capability profile carries no neighbourhood_cache_bytes".to_string())
}

fn next_path<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<PathBuf, Error> {
    it.next()
        .map(PathBuf::from)
        .ok_or_else(|| Error::Usage(format!("{flag} needs a path")))
}

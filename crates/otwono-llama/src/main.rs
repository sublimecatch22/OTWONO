//! `otwono-llama-backend` — the llama.cpp AI backend adapter.
//!
//! Speaks newline-delimited JSON-RPC on stdin/stdout, per `otwono_llama::protocol`, and
//! drives one `llama-server` process. It is spawned by `otwono-aid` through
//! `otwono_ai::supervisor` and is not intended to be run by hand, though it is entirely
//! usable that way and that is deliberate — a backend you can drive from a terminal is a
//! backend you can debug:
//!
//! ```text
//! $ printf '%s\n' \
//!     '{"jsonrpc":"2.0","id":1,"method":"backend.load","params":{"model_path":"/m.gguf","context_tokens":512}}' \
//!     '{"jsonrpc":"2.0","id":2,"method":"backend.infer","params":{"prompt":"hello","max_tokens":16}}' \
//!   | otwono-llama-backend --engine /usr/lib/otwono/llama.cpp/cpu/bin/llama-server
//! ```

#![forbid(unsafe_code)]

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use otwono_ai::supervisor::{BackendHello, PROTOCOL_VERSION};
use otwono_llama::sandbox::{self, Enforcement, Policy};
use otwono_llama::{Adapter, EngineConfig, ENGINE_NAME};
use otwono_proto::message::{Request, RequestId, Response, RpcError};

const USAGE: &str = "\
otwono-llama-backend — llama.cpp as an OTWONO AI backend

USAGE:
    otwono-llama-backend --engine <llama-server> [OPTIONS] [-- <engine args>...]

OPTIONS:
    --engine <PATH>         llama-server binary  [env: OTWONO_LLAMA_SERVER]
    --model-dir <DIR>       the only directory models may be read from
    --runtime-dir <DIR>     directory for the engine socket (default: /run/otwono/ai)
    --startup-timeout <S>   seconds to wait for a model to load (default: 300)
    --infer-timeout <S>     seconds to wait for one completion (default: 600)
    --allow-unconfined      run without Landlock confinement (see below)
    --probe                 report whether this kernel can confine the engine, then exit
    -h, --help              print this

The adapter confines itself with Landlock before starting an engine, so the engine can
read the model store and nothing else of the node's (ADR-0012). On a kernel without
Landlock it refuses to start rather than silently running an untrusted-file parser
unconfined; --allow-unconfined overrides that, deliberately explicit and logged.
";

const DEFAULT_RUNTIME_DIR: &str = "/run/otwono/ai";
const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 300;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("otwono-llama-backend: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let (config, model_dir, allow_unconfined) = parse_args(std::env::args().skip(1))?;
    std::fs::create_dir_all(&config.runtime_dir)
        .map_err(|e| format!("cannot create {}: {e}", config.runtime_dir.display()))?;

    // Before anything else, and before any engine exists. Landlock is inherited and
    // irreversible, so confining here confines every engine this process will ever start
    // — there is no ordering mistake available later.
    let policy = Policy {
        engine: config.binary.clone(),
        model_dir: model_dir.clone(),
        runtime_dir: config.runtime_dir.clone(),
    };
    let enforcement = sandbox::restrict(&policy).map_err(|e| e.to_string())?;
    match enforcement {
        Enforcement::None if !allow_unconfined => {
            return Err(
                "this kernel does not enforce Landlock, so the inference engine cannot be \
                 confined. Refusing to run a parser of untrusted model files unconfined; \
                 pass --allow-unconfined to override. Check with --probe."
                    .to_string(),
            )
        }
        // Reported on stderr, which the supervisor captures: an operator who overrode the
        // refusal should see it in the journal every time, not once at install.
        Enforcement::None => {
            eprintln!("otwono-llama-backend: WARNING running unconfined, --allow-unconfined was given")
        }
        Enforcement::Partial => {
            eprintln!("otwono-llama-backend: Landlock only partially enforced on this kernel")
        }
        Enforcement::Full => {}
    }

    let infer_timeout = std::env::var("OTWONO_LLAMA_INFER_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(otwono_llama::DEFAULT_INFER_TIMEOUT);
    let mut adapter = Adapter::new(config)
        .with_infer_timeout(infer_timeout)
        .with_policy(policy);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // The hello goes out before any model is loaded, and that ordering is the point: model
    // loading can take minutes on a small machine, and a supervisor that could not tell
    // "still loading" from "never going to answer" would have to pick a hello timeout long
    // enough to cover the worst load, which is no timeout at all.
    let hello = BackendHello {
        protocol: PROTOCOL_VERSION,
        engine: ENGINE_NAME.to_string(),
        version: adapter.engine_version().to_string(),
    };
    let _ = enforcement;
    write_line(&mut out, &serde_json::to_value(&hello).expect("hello serializes"))?;

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| format!("cannot read stdin: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => adapter.handle(request),
            // A frame we cannot parse still gets an answer. Staying silent would leave the
            // supervisor waiting out its whole timeout for a reply that is never coming.
            Err(e) => Response::err(
                RequestId::Null,
                RpcError::parse_error(format!("cannot parse request: {e}")),
            ),
        };
        write_line(
            &mut out,
            &serde_json::to_value(&response).expect("response serializes"),
        )?;
    }

    // Stdin closed: the supervisor is done with us. Dropping the adapter stops the engine.
    Ok(())
}

fn write_line(out: &mut impl Write, value: &serde_json::Value) -> Result<(), String> {
    let mut line = serde_json::to_string(value).map_err(|e| e.to_string())?;
    line.push('\n');
    out.write_all(line.as_bytes())
        .and_then(|_| out.flush())
        .map_err(|e| format!("cannot write to stdout: {e}"))
}

type Parsed = (EngineConfig, PathBuf, bool);

fn parse_args(args: impl Iterator<Item = String>) -> Result<Parsed, String> {
    let mut binary = std::env::var("OTWONO_LLAMA_SERVER").ok().map(PathBuf::from);
    let mut model_dir: Option<PathBuf> = None;
    let mut runtime_dir = PathBuf::from(DEFAULT_RUNTIME_DIR);
    let mut startup_timeout = Duration::from_secs(DEFAULT_STARTUP_TIMEOUT_SECS);
    let mut allow_unconfined = false;
    let mut extra_args = Vec::new();

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--engine" => binary = Some(PathBuf::from(next(&mut args, "--engine")?)),
            "--model-dir" => model_dir = Some(PathBuf::from(next(&mut args, "--model-dir")?)),
            "--runtime-dir" => runtime_dir = PathBuf::from(next(&mut args, "--runtime-dir")?),
            "--allow-unconfined" => allow_unconfined = true,
            "--probe" => {
                // Answerable without an engine or a model, because the boot-time check runs
                // it on a node that has neither yet. It confines this process to find out,
                // which is why it exits immediately afterwards.
                let enforcement = sandbox::probe_by_restricting_this_process();
                println!("landlock={}", enforcement.as_str());
                std::process::exit(if enforcement.is_confined() { 0 } else { 1 });
            }
            "--startup-timeout" => {
                startup_timeout = Duration::from_secs(seconds(&mut args, "--startup-timeout")?)
            }
            "--infer-timeout" => {
                // Handled through the environment so `Adapter` keeps one source of truth
                // for it; accept the flag too because a flag nobody accepts is a trap.
                let secs = seconds(&mut args, "--infer-timeout")?;
                std::env::set_var("OTWONO_LLAMA_INFER_TIMEOUT", secs.to_string());
            }
            "--" => {
                extra_args.extend(args.by_ref());
                break;
            }
            other => return Err(format!("unknown argument {other:?}\n\n{USAGE}")),
        }
    }

    let binary = binary.ok_or_else(|| format!("--engine is required\n\n{USAGE}"))?;
    // Required, not defaulted: it is the boundary of what the engine may read, and a
    // default would be a boundary nobody chose.
    let model_dir = model_dir.ok_or_else(|| format!("--model-dir is required\n\n{USAGE}"))?;
    Ok((
        EngineConfig {
            binary,
            runtime_dir,
            startup_timeout,
            extra_args,
        },
        model_dir,
        allow_unconfined,
    ))
}

fn next(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn seconds(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64, String> {
    next(args, flag)?
        .parse()
        .map_err(|_| format!("{flag} needs a whole number of seconds"))
}

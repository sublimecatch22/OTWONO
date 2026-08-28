//! `otwono-aictl` — drive the AI subsystem from a terminal.
//!
//! Every guarded method needs a capability, so this asks `otwono-permd` for one and
//! presents it, exactly as any other client must. That is the point of it being a real
//! client and not a back door: if policy does not grant the action, this fails the same way
//! anything else would.
//!
//! It exists for two audiences. A person, who otherwise has no way to install a model or
//! run a prompt without hand-writing JSON-RPC into a socket; and the boot-time inference
//! check, which needs to do exactly that in a shell script and would otherwise need
//! `socat` in the base image.
//!
//! ```text
//! otwono-aictl capabilities
//! otwono-aictl models
//! otwono-aictl install --manifest m.json --blob weights.gguf
//! otwono-aictl verify qwen3-4b-instruct-q4_k_m
//! otwono-aictl admit qwen3-4b-instruct-q4_k_m --context 4096
//! otwono-aictl infer qwen3-4b-instruct-q4_k_m --prompt "hello" --max-tokens 32
//! ```

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use otwono_proto::Client;
use serde_json::{json, Value};

const USAGE: &str = "\
otwono-aictl — command-line access to the OTWONO AI daemon

USAGE:
    otwono-aictl <COMMAND> [OPTIONS]

COMMANDS:
    capabilities                  What this node can run. Needs no capability token.
    models                        List the catalog, with why each model would or would not run
    install                       Install a model from a local manifest and weights
    verify <MODEL_ID>             Re-hash an installed model against its manifest
    admit <MODEL_ID>              Dry run: would this model load, and at what cost
    infer <MODEL_ID>              Run a prompt through it

OPTIONS:
    --socket <PATH>          AI daemon socket (default $OTWONO_SOCKET_DIR/ai.sock)
    --perm-socket <PATH>     Permission broker socket (default $OTWONO_SOCKET_DIR/perm.sock)
    --json                   Print the daemon's reply verbatim
    -h, --help               Show this message

INSTALL OPTIONS:
    --manifest <PATH>        The model manifest, required
    --blob <PATH>            The weights the manifest describes, required
    --allow-unsigned         Accept an unsigned manifest, or one signed by a publisher this
                             node does not trust. Never accepts a broken signature.

ADMIT / INFER OPTIONS:
    --context <N>            Context window to reserve (default: the model's maximum)
    --prompt <TEXT>          The prompt, required for infer
    --max-tokens <N>         Upper bound on generated tokens (default 64)
    --seed <N>               Fixed sampling seed, for a reproducible answer
    --temperature <F>        Sampling temperature

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
            eprintln!("otwono-aictl: {m}\n\n{USAGE}");
            ExitCode::from(1)
        }
        Err(Error::Runtime(m)) => {
            eprintln!("otwono-aictl: {m}");
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
    manifest: Option<PathBuf>,
    blob: Option<PathBuf>,
    prompt: Option<String>,
    context: Option<u32>,
    max_tokens: Option<u32>,
    seed: Option<u64>,
    temperature: Option<f64>,
    allow_unsigned: bool,
    json: bool,
}

fn run(args: &[String]) -> Result<String, Error> {
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(USAGE.to_string());
    }
    let opts = parse_args(args)?;
    let ai_socket = opts
        .socket
        .clone()
        .unwrap_or_else(|| otwono_proto::socket_path("ai"));
    let perm_socket = opts
        .perm_socket
        .clone()
        .unwrap_or_else(|| otwono_proto::socket_path("perm"));

    let (method, params, action) = build_call(&opts)?;
    let value = call(&ai_socket, &perm_socket, &method, params, action)?;

    if opts.json {
        return serde_json::to_string_pretty(&value)
            .map(|s| s + "\n")
            .map_err(|e| Error::Runtime(e.to_string()));
    }
    Ok(render(&opts.command, &value))
}

/// Turn parsed options into the call to make: method, params, and the capability it needs.
///
/// Separated from the socket work so every command's request shape is unit-testable without
/// a daemon anywhere.
fn build_call(opts: &Options) -> Result<(String, Value, Option<&'static str>), Error> {
    let need_target = |what: &str| -> Result<String, Error> {
        opts.target
            .clone()
            .ok_or_else(|| Error::Usage(format!("{what} needs a model id")))
    };
    Ok(match opts.command.as_str() {
        // Open on the local socket: it describes the machine, not its contents.
        "capabilities" => ("ai.capabilities".into(), json!({}), None),
        "models" => ("ai.models.list".into(), json!({}), Some("ai.read")),
        "install" => {
            let manifest = opts
                .manifest
                .clone()
                .ok_or_else(|| Error::Usage("install needs --manifest".into()))?;
            let blob = opts
                .blob
                .clone()
                .ok_or_else(|| Error::Usage("install needs --blob".into()))?;
            (
                "ai.models.install".into(),
                json!({
                    "manifest_path": manifest.display().to_string(),
                    "blob_path": blob.display().to_string(),
                    "allow_unsigned": opts.allow_unsigned,
                }),
                Some("ai.admin"),
            )
        }
        "verify" => (
            "ai.models.verify".into(),
            json!({ "model_id": need_target("verify")? }),
            Some("ai.read"),
        ),
        "admit" => {
            let mut params = json!({
                "model_id": need_target("admit")?,
                "allow_unsigned": opts.allow_unsigned,
            });
            if let Some(context) = opts.context {
                params["context_tokens"] = json!(context);
            }
            ("ai.admit".into(), params, Some("ai.read"))
        }
        "infer" => {
            let prompt = opts
                .prompt
                .clone()
                .ok_or_else(|| Error::Usage("infer needs --prompt".into()))?;
            let mut params = json!({
                "model_id": need_target("infer")?,
                "prompt": prompt,
                "max_tokens": opts.max_tokens.unwrap_or(64),
                "allow_unsigned": opts.allow_unsigned,
            });
            for (key, value) in [
                ("context_tokens", opts.context.map(|v| json!(v))),
                ("seed", opts.seed.map(|v| json!(v))),
                ("temperature", opts.temperature.map(|v| json!(v))),
            ] {
                if let Some(value) = value {
                    params[key] = value;
                }
            }
            ("ai.infer".into(), params, Some("ai.infer"))
        }
        other => return Err(Error::Usage(format!("unknown command {other:?}"))),
    })
}

/// Ask the broker for a capability if the method needs one, then make the call.
fn call(
    ai_socket: &Path,
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
                    json!({ "action": action, "reason": format!("otwono-aictl {method}") }),
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

    let mut ai = Client::connect_waiting(ai_socket, wait).map_err(|e| {
        Error::Runtime(format!(
            "cannot reach the AI daemon at {}: {e}",
            ai_socket.display()
        ))
    })?;
    let response = match &token {
        Some(token) => ai.call_with_capability(method, params, token),
        None => ai.call(method, params),
    };
    response
        .map_err(|e| Error::Runtime(format!("{method} transport failure: {e}")))?
        .map_err(|e| Error::Runtime(format!("{method} refused: {}", e.message)))
}

/// Human-readable output. `--json` bypasses all of this.
fn render(command: &str, v: &Value) -> String {
    match command {
        "capabilities" => format!(
            "tier         {}\naccelerator  {}\nbackends     {}\nlocal infer  {}\npublishers   {}\n",
            text(&v["tier"]),
            text(&v["accelerator"]),
            join(&v["installed_backends"]),
            if v["local_inference_available"] == json!(true) {
                "available"
            } else {
                "unavailable"
            },
            v["trusted_publishers"],
        ),
        "models" => {
            let empty = Vec::new();
            let models = v["models"].as_array().unwrap_or(&empty);
            if models.is_empty() {
                return "no models in the catalog\n".to_string();
            }
            let mut out = String::new();
            for m in models {
                out.push_str(&format!(
                    "{:<40} {:>10}  {:<12} {}\n",
                    text(&m["id"]),
                    m["size_bytes"],
                    text(&m["provenance"]["status"]),
                    if m["admissible"] == json!(true) {
                        "runnable".to_string()
                    } else {
                        format!("cannot run: {}", text(&m["reason"]))
                    }
                ));
            }
            out
        }
        "install" => format!(
            "installed {} ({} bytes, {}){}\n",
            text(&v["model_id"]),
            v["size_bytes"],
            text(&v["provenance"]["status"]),
            if v["already_present"] == json!(true) {
                "; weights were already present"
            } else {
                ""
            }
        ),
        "verify" => format!(
            "{}: {}\n",
            text(&v["model_id"]),
            if v["digest_matches"] == json!(true) {
                "weights match the manifest".to_string()
            } else if v["weights_present"] == json!(false) {
                "no weights installed".to_string()
            } else {
                format!("MISMATCH, weights hash to {}", text(&v["blake3"]))
            }
        ),
        "admit" => {
            if v["admissible"] == json!(true) {
                format!(
                    "{} would load on {} using {} of a {} byte budget\n",
                    text(&v["model_id"]),
                    text(&v["backend"]),
                    v["required_bytes"],
                    v["budget_bytes"]
                )
            } else {
                format!(
                    "{} would not load: {}\n",
                    text(&v["model_id"]),
                    text(&v["reason"])
                )
            }
        }
        "infer" => format!(
            "{}\n\n[{} tokens from {}, stopped on {}]\n",
            text(&v["text"]),
            v["tokens_predicted"],
            text(&v["backend"]),
            text(&v["stop_reason"])
        ),
        _ => format!("{v}\n"),
    }
}

fn text(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

fn join(v: &Value) -> String {
    match v.as_array() {
        Some(a) if !a.is_empty() => a.iter().map(text).collect::<Vec<_>>().join(", "),
        _ => "none".to_string(),
    }
}

fn parse_args(args: &[String]) -> Result<Options, Error> {
    let mut opts = Options::default();
    let mut it = args.iter();
    opts.command = it
        .next()
        .cloned()
        .ok_or_else(|| Error::Usage("no command".into()))?;

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => opts.socket = Some(next_path(&mut it, "--socket")?),
            "--perm-socket" => opts.perm_socket = Some(next_path(&mut it, "--perm-socket")?),
            "--manifest" => opts.manifest = Some(next_path(&mut it, "--manifest")?),
            "--blob" => opts.blob = Some(next_path(&mut it, "--blob")?),
            "--prompt" => opts.prompt = Some(next_value(&mut it, "--prompt")?),
            "--context" => opts.context = Some(next_number(&mut it, "--context")?),
            "--max-tokens" => opts.max_tokens = Some(next_number(&mut it, "--max-tokens")?),
            "--seed" => opts.seed = Some(next_number(&mut it, "--seed")?),
            "--temperature" => {
                let raw = next_value(&mut it, "--temperature")?;
                opts.temperature = Some(
                    raw.parse()
                        .map_err(|_| Error::Usage(format!("--temperature needs a number, got {raw:?}")))?,
                );
            }
            "--allow-unsigned" => opts.allow_unsigned = true,
            "--json" => opts.json = true,
            // The first bare word after the command is the model id.
            other if !other.starts_with('-') && opts.target.is_none() => {
                opts.target = Some(other.to_string())
            }
            other => return Err(Error::Usage(format!("unknown option {other:?}"))),
        }
    }
    Ok(opts)
}

fn next_value<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<String, Error> {
    it.next()
        .cloned()
        .ok_or_else(|| Error::Usage(format!("{flag} needs a value")))
}

fn next_path<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<PathBuf, Error> {
    next_value(it, flag).map(PathBuf::from)
}

fn next_number<'a, T: std::str::FromStr>(
    it: &mut impl Iterator<Item = &'a String>,
    flag: &str,
) -> Result<T, Error> {
    let raw = next_value(it, flag)?;
    raw.parse()
        .map_err(|_| Error::Usage(format!("{flag} needs a whole number, got {raw:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn parsed(args: &[&str]) -> Options {
        parse_args(&argv(args)).unwrap()
    }

    #[test]
    fn no_arguments_prints_usage_rather_than_failing() {
        assert!(run(&[]).unwrap().contains("USAGE"));
    }

    #[test]
    fn capabilities_needs_no_capability_token() {
        // It describes the machine, not its contents, and the boot check reads it before
        // any policy has granted anything.
        let (method, _, action) = build_call(&parsed(&["capabilities"])).unwrap();
        assert_eq!(method, "ai.capabilities");
        assert_eq!(action, None);
    }

    #[test]
    fn each_command_asks_for_the_capability_its_method_is_guarded_by() {
        // Drift here would show up as a confusing "policy refuses" for the wrong action.
        for (args, expected) in [
            (vec!["models"], Some("ai.read")),
            (vec!["verify", "m"], Some("ai.read")),
            (vec!["admit", "m"], Some("ai.read")),
            (vec!["infer", "m", "--prompt", "hi"], Some("ai.infer")),
            (
                vec!["install", "--manifest", "m.json", "--blob", "w.gguf"],
                Some("ai.admin"),
            ),
        ] {
            let (_, _, action) = build_call(&parsed(&args)).unwrap();
            assert_eq!(action, expected, "for {args:?}");
        }
    }

    #[test]
    fn infer_defaults_to_a_bounded_number_of_tokens() {
        // Never unbounded: an unbounded request occupies the node's only engine for as long
        // as the model keeps talking.
        let (_, params, _) = build_call(&parsed(&["infer", "m", "--prompt", "hi"])).unwrap();
        assert_eq!(params["max_tokens"], 64);
    }

    #[test]
    fn optional_sampling_settings_are_omitted_rather_than_defaulted() {
        // Sending a default temperature would override whatever the daemon and engine
        // consider sensible, silently.
        let (_, params, _) = build_call(&parsed(&["infer", "m", "--prompt", "hi"])).unwrap();
        assert!(params.get("seed").is_none());
        assert!(params.get("temperature").is_none());
        assert!(params.get("context_tokens").is_none());

        let (_, params, _) = build_call(&parsed(&[
            "infer",
            "m",
            "--prompt",
            "hi",
            "--seed",
            "7",
            "--temperature",
            "0.5",
            "--context",
            "512",
        ]))
        .unwrap();
        assert_eq!(params["seed"], 7);
        assert_eq!(params["temperature"], 0.5);
        assert_eq!(params["context_tokens"], 512);
    }

    #[test]
    fn install_requires_both_halves_of_a_model() {
        assert!(matches!(
            build_call(&parsed(&["install", "--manifest", "m.json"])),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            build_call(&parsed(&["install", "--blob", "w.gguf"])),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn a_command_that_needs_a_model_id_says_so_rather_than_sending_an_empty_one() {
        assert!(matches!(build_call(&parsed(&["verify"])), Err(Error::Usage(_))));
        assert!(matches!(
            build_call(&parsed(&["infer", "--prompt", "hi"])),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn socket_overrides_are_parsed() {
        let o = parsed(&["models", "--socket", "/a.sock", "--perm-socket", "/p.sock"]);
        assert_eq!(o.socket.as_deref(), Some(Path::new("/a.sock")));
        assert_eq!(o.perm_socket.as_deref(), Some(Path::new("/p.sock")));
    }

    #[test]
    fn an_unknown_option_is_a_usage_error_not_a_silently_ignored_flag() {
        assert!(matches!(
            parse_args(&argv(&["models", "--wat"])),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn rendering_an_inference_reports_the_token_count_and_stop_reason() {
        // The boot check greps this, and a silent shape change would break it there rather
        // than here.
        let out = render(
            "infer",
            &json!({
                "text": "some words", "tokens_predicted": 8,
                "backend": "llama-cpp-cpu", "stop_reason": "token_limit"
            }),
        );
        assert!(out.contains("some words"), "{out}");
        assert!(out.contains("8 tokens from llama-cpp-cpu"), "{out}");
    }

    #[test]
    fn rendering_a_verify_mismatch_does_not_look_like_success() {
        let out = render(
            "verify",
            &json!({ "model_id": "m", "weights_present": true, "digest_matches": false, "blake3": "abc" }),
        );
        assert!(out.contains("MISMATCH"), "{out}");
    }

    #[test]
    fn an_empty_catalog_renders_as_a_sentence_not_a_blank_line() {
        assert_eq!(
            render("models", &json!({ "models": [] })),
            "no models in the catalog\n"
        );
    }
}

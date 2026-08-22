//! `otwono-hwctl` — inspect this machine's hardware and capability profile.
//!
//! Deliberately dependency-light: no argument-parsing crate, because this binary ships in
//! the smallest image we build and runs at first boot before anything else is up.

#![forbid(unsafe_code)]

use otwono_capability::{classify_with_overrides, CapabilityOverrides, CapabilityProfile};
use otwono_hal::SystemProbe;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
otwono-hwctl — OTWONO hardware and capability inspection

USAGE:
    otwono-hwctl <COMMAND> [OPTIONS]

COMMANDS:
    profile     Print the capability profile (tier, axes, feature gates)
    hardware    Print the raw hardware report only
    tier        Print just the tier identifier
    help        Show this message

OPTIONS:
    --json                  Machine-readable output (the contract; parse this, not the text)
    --root <PATH>           Probe a captured fixture tree instead of the live system
    --overrides <PATH>      Override file (default /etc/otwono/capability.override.toml)
    --no-overrides          Ignore the override file entirely

EXIT CODES:
    0  success
    1  usage error
    2  probe or override error
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(Error::Usage(msg)) => {
            eprintln!("otwono-hwctl: {msg}\n\n{USAGE}");
            ExitCode::from(1)
        }
        Err(Error::Runtime(msg)) => {
            eprintln!("otwono-hwctl: {msg}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug)]
enum Error {
    Usage(String),
    Runtime(String),
}

struct Options {
    command: String,
    json: bool,
    root: PathBuf,
    overrides: Option<PathBuf>,
    use_overrides: bool,
}

fn parse_args(args: &[String]) -> Result<Options, Error> {
    let mut opts = Options {
        command: String::new(),
        json: false,
        root: PathBuf::from("/"),
        overrides: None,
        use_overrides: true,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => opts.json = true,
            "--no-overrides" => opts.use_overrides = false,
            "--root" => {
                opts.root = it
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| Error::Usage("--root needs a path".into()))?
            }
            "--overrides" => {
                opts.overrides = Some(
                    it.next()
                        .map(PathBuf::from)
                        .ok_or_else(|| Error::Usage("--overrides needs a path".into()))?,
                )
            }
            "-h" | "--help" | "help" => opts.command = "help".into(),
            other if other.starts_with('-') => return Err(Error::Usage(format!("unknown option {other}"))),
            other if opts.command.is_empty() => opts.command = other.to_string(),
            other => return Err(Error::Usage(format!("unexpected argument {other}"))),
        }
    }
    if opts.command.is_empty() {
        opts.command = "help".into();
    }
    Ok(opts)
}

fn run(args: &[String]) -> Result<String, Error> {
    let opts = parse_args(args)?;
    if opts.command == "help" {
        return Ok(USAGE.to_string());
    }

    let probe = SystemProbe::from_root(&opts.root);
    let report = probe.probe();

    let overrides = if opts.use_overrides {
        let path = opts
            .overrides
            .clone()
            .unwrap_or_else(|| Path::new(otwono_capability::overrides::DEFAULT_OVERRIDE_PATH).to_path_buf());
        CapabilityOverrides::load(&path).map_err(|e| Error::Runtime(e.to_string()))?
    } else {
        CapabilityOverrides::default()
    };

    let profile = classify_with_overrides(&report, &overrides);

    match opts.command.as_str() {
        "profile" => {
            if opts.json {
                serde_json::to_string_pretty(&profile)
                    .map(|s| s + "\n")
                    .map_err(|e| Error::Runtime(e.to_string()))
            } else {
                Ok(render_profile(&profile))
            }
        }
        "hardware" => {
            if opts.json {
                serde_json::to_string_pretty(&report)
                    .map(|s| s + "\n")
                    .map_err(|e| Error::Runtime(e.to_string()))
            } else {
                Ok(render_hardware(&profile))
            }
        }
        "tier" => Ok(format!("{}\n", profile.tier.as_str())),
        other => Err(Error::Usage(format!("unknown command {other}"))),
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn render_hardware(p: &CapabilityProfile) -> String {
    let hw = &p.hardware;
    let mut s = String::new();
    s.push_str("HARDWARE\n");
    s.push_str(&format!(
        "  machine        {} ({})\n",
        hw.machine.model.as_deref().unwrap_or("unknown model"),
        hw.machine.architecture
    ));
    s.push_str(&format!(
        "  cpu            {} — {} logical / {} physical{}\n",
        hw.cpu
            .model_name
            .as_deref()
            .unwrap_or(hw.cpu.vendor.as_deref().unwrap_or("unknown")),
        hw.cpu.logical_cpus,
        hw.cpu.physical_cores,
        hw.cpu
            .max_frequency_mhz
            .map(|f| format!(" @ {f} MHz"))
            .unwrap_or_default()
    ));
    let mut isa = Vec::new();
    for (name, present) in [
        ("avx2", hw.cpu.features.avx2),
        ("avx512f", hw.cpu.features.avx512f),
        ("vnni", hw.cpu.features.avx_vnni),
        ("neon", hw.cpu.features.neon),
        ("dotprod", hw.cpu.features.dotprod),
        ("i8mm", hw.cpu.features.i8mm),
        ("sve", hw.cpu.features.sve),
    ] {
        if present {
            isa.push(name);
        }
    }
    s.push_str(&format!(
        "  isa            {}\n",
        if isa.is_empty() { "-".into() } else { isa.join(" ") }
    ));
    s.push_str(&format!(
        "  memory         {} total, {} available\n",
        gib(hw.memory.total_bytes),
        gib(hw.memory.available_bytes)
    ));
    if hw.accelerators.is_empty() {
        s.push_str("  accelerators   none\n");
    } else {
        for a in &hw.accelerators {
            s.push_str(&format!(
                "  accelerator    {:?} {} {} [{}]{}\n",
                a.kind,
                a.vendor,
                a.driver.as_deref().unwrap_or("-"),
                a.compute_apis.join(","),
                a.vram_bytes
                    .map(|v| format!(" {} VRAM", gib(v)))
                    .unwrap_or_default()
            ));
        }
    }
    s.push_str(&format!(
        "  storage        {} free of {} at {}\n",
        gib(hw.storage.data_free_bytes),
        gib(hw.storage.data_total_bytes),
        hw.storage.data_path
    ));
    let up: Vec<&str> = hw
        .network
        .interfaces
        .iter()
        .filter(|i| i.operstate == "up")
        .map(|i| i.name.as_str())
        .collect();
    s.push_str(&format!(
        "  network        {} up{}{}\n",
        if up.is_empty() {
            "none".to_string()
        } else {
            up.join(",")
        },
        if hw.network.has_default_route {
            ", default route"
        } else {
            ""
        },
        if hw.network.mesh_radio_present {
            ", mesh radio"
        } else {
            ""
        }
    ));
    s.push_str(&format!(
        "  power          {}\n",
        if hw.power.on_battery {
            "on battery"
        } else if hw.power.has_battery {
            "battery present, on mains"
        } else {
            "mains / no battery"
        }
    ));
    s
}

fn render_profile(p: &CapabilityProfile) -> String {
    let mut s = render_hardware(p);
    s.push_str("\nCAPABILITY\n");
    s.push_str(&format!(
        "  tier           {}{}\n",
        p.tier.as_str(),
        if p.overridden { " (overridden)" } else { "" }
    ));
    if let Some(reason) = &p.limiting_factor {
        s.push_str(&format!("  limited by     {reason}\n"));
    }
    s.push_str(&format!(
        "  axes           compute={} memory={} accelerator={} storage={} network={} power={}\n",
        p.axes.compute.as_str(),
        p.axes.memory.as_str(),
        p.axes.accelerator.as_str(),
        p.axes.storage.as_str(),
        p.axes.network.as_str(),
        p.axes.power.as_str()
    ));

    let f = &p.features;
    s.push_str("\nFEATURES\n");
    s.push_str(&format!(
        "  local llm      {}\n",
        match f.max_model_parameters {
            Some(n) if f.local_llm => format!(
                "yes, up to {}B params ({})",
                n / 1_000_000_000,
                f.recommended_quantization.as_deref().unwrap_or("-")
            ),
            _ => "no — this machine delegates or uses the command grammar".to_string(),
        }
    ));
    s.push_str(&format!("  local rag      {}\n", yesno(f.local_rag)));
    s.push_str(&format!(
        "  speech         stt={} tts={}\n",
        yesno(f.speech_to_text),
        yesno(f.text_to_speech)
    ));
    s.push_str(&format!("  image gen      {}\n", yesno(f.image_generation)));
    s.push_str(&format!("  max agents     {}\n", f.max_concurrent_agents));
    s.push_str(&format!("  desktop        {:?}\n", f.desktop));
    s.push_str(&format!(
        "  node roles     {}\n",
        f.eligible_node_roles.join(", ")
    ));
    s.push_str(&format!("  serve ai       {}\n", yesno(f.serve_ai_to_peers)));
    s.push_str(&format!("  replication    {}\n", yesno(f.content_replication)));
    s.push_str("\n  (node roles are eligibility only; every one requires the user to opt in)\n");

    if !p.warnings.is_empty() {
        s.push_str("\nWARNINGS\n");
        for w in &p.warnings {
            s.push_str(&format!("  - {w}\n"));
        }
    }
    s
}

fn yesno(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_arguments_shows_help() {
        assert!(run(&[]).unwrap().contains("USAGE"));
    }

    #[test]
    fn unknown_option_is_a_usage_error() {
        assert!(matches!(run(&argv(&["profile", "--nope"])), Err(Error::Usage(_))));
    }

    #[test]
    fn unknown_command_is_a_usage_error() {
        assert!(matches!(run(&argv(&["frobnicate"])), Err(Error::Usage(_))));
    }

    #[test]
    fn tier_command_emits_one_bare_token() {
        let out = run(&argv(&["tier", "--root", "/nonexistent", "--no-overrides"])).unwrap();
        assert_eq!(out.trim(), "T0_MICRO");
        assert_eq!(out.lines().count(), 1);
    }

    #[test]
    fn json_profile_is_valid_json_with_the_contract_fields() {
        let out = run(&argv(&[
            "profile",
            "--json",
            "--root",
            "/nonexistent",
            "--no-overrides",
        ]))
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        for field in [
            "schema_version",
            "tier",
            "axes",
            "features",
            "hardware",
            "warnings",
        ] {
            assert!(v.get(field).is_some(), "missing contract field {field}");
        }
        assert_eq!(v["schema_version"], "1.0.0");
    }

    #[test]
    fn a_malformed_override_file_is_a_runtime_error_not_a_silent_default() {
        let dir = std::env::temp_dir().join(format!("otwono-hwctl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.toml");
        std::fs::write(&p, "tier = \"NOPE\"\n").unwrap();
        let r = run(&argv(&[
            "tier",
            "--root",
            "/nonexistent",
            "--overrides",
            p.to_str().unwrap(),
        ]));
        assert!(matches!(r, Err(Error::Runtime(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn human_output_names_the_limiting_axis() {
        // The live machine is whatever CI is; we only assert the section is rendered.
        let out = run(&argv(&["profile", "--root", "/nonexistent", "--no-overrides"])).unwrap();
        assert!(out.contains("CAPABILITY"));
        assert!(out.contains("FEATURES"));
        assert!(
            out.contains("WARNINGS"),
            "a nonexistent root must produce warnings"
        );
    }
}

# Application Integration Strategy

**Status:** `SPECIFIED`. No adapters implemented yet.

## 1. Rule

The OS drives mature open-source applications. We write adapters, not replacements.
Replacing an existing mature application requires an ADR with a technical justification.

## 2. Adapter manifest

Each adapter is a declarative manifest plus a small driver binary or script:

```toml
[adapter]
id = "libreoffice-writer"
app = "LibreOffice Writer"
detect = { command = "soffice", version_flag = "--version", min_version = "7.4" }
channel = "uno"                     # uno | cli | file | atspi | synthetic
tier_min = "T1_EDGE"
sandbox  = "flatpak"

[[action]]
id = "document.open"
schema = "schemas/actions/document.open.schema.json"
capabilities = ["fs.read:{path}"]
reversible = true

[[action]]
id = "document.replace_text"
schema = "schemas/actions/document.replace_text.schema.json"
capabilities = ["fs.write:{path}"]
reversible = true
snapshot_before = true

[[action]]
id = "document.export_pdf"
capabilities = ["fs.read:{path}", "fs.write:{out}"]
reversible = true
```

The manifest is the contract. The agent's tool registry is generated from installed
manifests, so the agent can only ever offer actions that actually exist on this machine at
this tier.

## 3. Control channels, in order of preference

| # | Channel | Examples | Why the ranking |
|---|---|---|---|
| 1 | Documented API | LibreOffice UNO, GIMP Python-Fu, Inkscape actions, mpv IPC, Krita Python | Verifiable, stable, scriptable, returns errors |
| 2 | CLI | ffmpeg, ImageMagick, pandoc, `rg`, `fd`, git, systemctl, apt | Verifiable exit codes, easy to sandbox |
| 3 | File format | Edit ODF/SVG/Kdenlive XML directly, then reload | No app needed; brittle across versions |
| 4 | AT-SPI accessibility | Apps with no other surface | Slow, fragile, but genuinely introspectable |
| 5 | Screen/pointer synthesis | — | **Discouraged. Requires an ADR.** Unverifiable, unauditable, breaks on a theme change |

## 4. Initial target set

| Domain | App | Channel |
|---|---|---|
| Documents | LibreOffice | UNO |
| Markdown / conversion | pandoc | CLI |
| Raster images | GIMP | Python-Fu |
| Batch images | ImageMagick | CLI |
| Vector | Inkscape | actions CLI |
| Audio | Audacity (mod-script-pipe), ffmpeg | pipe / CLI |
| Video | Kdenlive (project XML), ffmpeg | file / CLI |
| Playback | mpv | IPC socket |
| Browser | Firefox / Chromium | CDP or WebDriver, sandboxed |
| Files | `fd`, `rg`, coreutils | CLI |
| Packages | apt, flatpak | CLI, brokered |
| System | systemctl, journalctl | CLI, brokered, read-mostly |

## 5. Safety requirements

Every adapter action:

1. Goes through `otwono-permd` with declared capabilities.
2. Snapshots before any destructive change (copy-on-write where the filesystem supports it,
   a copy otherwise), or refuses.
3. Verifies its effect — re-read the file, check the exit code, confirm the pixel dimensions
   — and reports failure honestly rather than assuming success.
4. Has a timeout and a kill path.
5. Runs sandboxed where the app supports it.

## 6. Tier awareness

On T0/T1, GUI applications may not be installed at all. The adapter registry reflects what
is actually present, refreshed on package changes. An agent that offers to edit a video on
a Pi Zero because the manifest exists in the repo is a bug.

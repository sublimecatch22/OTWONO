# Building a release

What a release contains, how each part is built, and what has to happen on
which machine.

---

## What a release folder holds

```
releases/<version>/
  OTWONO AI_<version>_amd64.deb          Linux (built on Linux)
  OTWONO AI_<version>_x64-setup.exe      Windows NSIS (built on Windows)
  OTWONO AI_<version>_x64_en-US.msi      Windows MSI  (built on Windows)
  OTWONO AI_<version>_x64.dmg            macOS (built on macOS)
  otwono-ai-connector.zip                The WordPress plugin (any platform)
  SHA256SUMS                             Checksums for everything above
  RELEASE_NOTES.md                       What changed, and what is known
```

**Desktop installers cannot be cross-built.** Tauri's bundlers use each
platform's own tooling — NSIS and WiX on Windows, `hdiutil` on macOS, `dpkg` on
Linux. This is recorded as decision D-003; the answer is a build job per
platform, not a workaround.

## Before building

```bash
./scripts/verify.sh
```

Runs everything CI runs: formatting, types, lints, the Rust suite, the frontend
suite, the WordPress suite, the WordPress suite against a live relay, and the
end-to-end suite against the real service. **Do not tag a release until this
passes.**

## Linux and macOS

```bash
./scripts/build-release.sh
```

It runs the checks, builds the web assets, packages the plugin, builds the
desktop bundle for the platform it is on, and writes `SHA256SUMS`. If the
desktop bundle fails, it says so and the rest of the folder is still valid.

> **AppImage on a minimal container** fails for want of `xdg-open`. The `.deb`
> is unaffected. Build AppImages on a desktop Linux machine, or install
> `xdg-utils` in the image.

## Windows

On a Windows machine with Node.js 20+ and Rust:

```powershell
pwsh -File scripts/build-windows.ps1
```

It checks its prerequisites first and says what is missing rather than failing
part-way through a long build. Output goes to `releases/windows/` with a
`.sha256` beside each installer.

### Code signing

**The builds are unsigned.** Windows SmartScreen will warn about an unknown
publisher, and users will have to click through it.

To sign, you need a code-signing certificate (an EV certificate builds
SmartScreen reputation immediately; an OV one takes time and downloads).
Tauri's `bundle.windows.certificateThumbprint` setting will sign during the
build. **This needs a certificate the project owner must buy — it is one of the
things listed as outstanding in the handoff.** Nothing here should be signed
with a certificate borrowed from elsewhere.

### macOS notarisation

Same shape: an Apple Developer account, a Developer ID certificate, and
notarisation, or users must right-click → Open on first launch. Also the
owner's to arrange.

## The WordPress plugin

```bash
./scripts/package-wordpress-plugin.sh releases/<version>
```

Builds the ZIP from the plugin directory, excluding tests and development
files, and writes its SHA-256. The plugin has no build step — blocks are
server-rendered — so the ZIP is exactly what runs.

## The GitHub Actions workflow

`.github/workflows/release.yml` runs on a tag matching `v*`:

| Job | Runner | Produces |
|---|---|---|
| `verify` | Ubuntu | The whole check suite; everything else waits on it. |
| `linux` | Ubuntu | The `.deb` and the plugin ZIP. |
| `windows` | Windows | The `.exe` and `.msi`. |
| `macos` | macOS | The `.dmg`. |
| `collect` | Ubuntu | One release folder, `SHA256SUMS`, and a draft GitHub release. |

The release is left as a **draft**. Publishing it is a person's decision.

## Version numbers

Three files must agree:

| | |
|---|---|
| `Cargo.toml` | `[workspace.package] version` |
| `apps/desktop/src-tauri/tauri.conf.json` | `version` |
| `wordpress/otwono-ai-connector/otwono-ai-connector.php` | `Version:` header |

`scripts/build-release.sh` takes the version from `Cargo.toml` and names the
folder after it.

## Release notes

Say what changed, what is known to be limited, and anything a user must do by
hand when upgrading. If a migration runs, say so and say that a backup is taken
first. Do not describe as finished anything that has not been run.

## The checklist

- [ ] `./scripts/verify.sh` passes
- [ ] Versions agree in all three files
- [ ] `RELEASE_NOTES.md` written
- [ ] Linux `.deb` built and installed once on a clean machine
- [ ] Windows `.exe` and `.msi` built and installed once
- [ ] macOS `.dmg` built and opened once
- [ ] Plugin ZIP installs into a real WordPress and activates
- [ ] `SHA256SUMS` present and correct
- [ ] An upgrade over an existing data directory keeps its data
- [ ] Known limitations written down rather than left out

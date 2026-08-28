# Status

What is built, what is not, and what is known to be limited. Updated at the end
of each phase.

**Version 0.1.0** · Last updated at the end of Phase 6.

---

## Test results

Every number here comes from a run, not an estimate.

| Suite | Command | Result |
|---|---|---|
| Rust — 9 crates | `cargo test --workspace` | **495 passing** |
| Frontend | `npm run test` | **18 passing** |
| WordPress plugin | `php wordpress/tests/run-tests.php` | **28 passing** |
| WordPress against a live relay | `./scripts/run-wordpress-live-tests.sh` | **6 passing** |
| End to end | `npx playwright test` | **15 passing** |
| Lints | `cargo clippy --workspace --all-targets -- -D warnings` | Clean |
| Formatting | `cargo fmt --check`, `npm run format:check` | Clean |
| Types | `npm run typecheck` | Clean |

`./scripts/verify.sh` runs all of it.

## Phases

| Phase | State |
|---|---|
| 0 — Discovery, architecture, scaffold | **Done** |
| 1 — Shell, database, providers, streaming chat | **Done** |
| 2 — Agents and knowledge | **Done** |
| 3 — Projects, orchestration, workspaces | **Done** |
| 4 — Accounts, relay, WordPress plugin | **Done** |
| 5 — Human task marketplace (development preview) | **Done** |
| 6 — Hardening, tests, packaging, release | **Done** |

## Built and working

### The application

- Tauri 2 desktop shell: single instance, tray, window state, **opt-in**
  autostart, a narrow capability allow-list and a strict CSP.
- A local service on loopback, on an OS-chosen port, with a per-start bearer
  token and an origin allow-list.
- SQLite with ordered migrations, a backup before every schema change, and a
  refusal to downgrade a newer schema.
- Secrets in the operating system's credential store, with a visible
  encrypted-file fallback and an equally visible in-memory last resort.

### What a person can do

- Connect Ollama, LM Studio, or any OpenAI-compatible endpoint — including one
  on a non-default port, choosing which runtime is listening.
- See, for every model, **how** each capability was established: reported,
  probed, or guessed from the name.
- Hold a streaming conversation that can be stopped, titles itself, and
  survives a restart.
- Authorise folders, index them locally, search them, and get answers that cite
  the file and the place in it. Revoking deletes the index immediately.
- Create, edit, version, test, export and import agents. **An export can never
  contain a credential.**
- Plan a project, read the plan, approve it, run it inside a step budget, have
  the work verified, and export a completion report.
- Run offices, labs, boardrooms and think tanks; a session reports the
  synthesis *and* the dissent.
- Post and take on marketplace work with simulated payments, moderation that
  names what it refused, and a route to a person.
- Link an account, send the metadata of projects they ticked, and read a
  receipt of exactly what left the machine.
- Pair a WordPress site with a single-use code; members sign in, edit a
  profile, and see what they published.
- Stop everything at once with the emergency stop.

### Built artefacts

| | |
|---|---|
| Linux `.deb` | Built here. |
| WordPress plugin ZIP | Built here. |
| Windows `.exe` / `.msi` | Scripted and wired into CI; **built on Windows**. |
| macOS `.dmg` | Scripted and wired into CI; **built on macOS**. |

## Known limitations

Recorded rather than omitted.

| | |
|---|---|
| **Installers are unsigned.** | Windows SmartScreen warns about an unknown publisher; macOS needs right-click → Open. Signing needs certificates the project owner must buy. |
| **Windows and macOS packages are not built in this environment.** | Tauri's bundlers need each platform's own tooling. The scripts and CI jobs exist; they have not been run on those platforms from here. |
| **AppImage fails in a minimal container** for want of `xdg-open`. | The `.deb` is unaffected. Build AppImages on a desktop Linux machine. |
| **The relay has not been deployed to a public address.** | It runs, is tested over real HTTP, and is tested against the WordPress plugin — but nothing claims it is deployed, and no default points at one. |
| **The relay does not send email.** | Registration returns the verification token in the response instead. This is marked in the code and must change before real users. |
| **Without an embedding model, search matches words, not meaning.** | The interface says so where it matters. |
| **The marketplace is a development preview.** | Payments are simulated. Nothing holds or moves money. |
| **One machine, one user.** | There is no multi-user desktop mode; each operating-system account is its own installation. |
| **No mobile or web-hosted client.** | The interface is responsive, but it is delivered by the desktop shell. |

## Not started, and deliberately so

- Speech input or output.
- Image generation.
- Automatic background synchronisation. Synchronisation happens when a person
  presses the button, so that the receipt means something.
- Plugin or extension APIs for third-party code.
- Any form of analytics.

## What the owner still has to provide

| | Why |
|---|---|
| A Windows code-signing certificate | Otherwise every Windows user sees a SmartScreen warning. |
| An Apple Developer ID | Otherwise every macOS user has to bypass Gatekeeper. |
| A relay host and TLS certificate | Only if you want WordPress sign-in. The application is complete without it. |
| A mail service for the relay | Only if you deploy one; account verification needs it. |

None of these can be bought, registered or configured without the owner's
consent and money, so none of them has been.

# Handoff — OTWONO AI 0.1.0

Everything the owner needs to take this on: what was built, what was not, how
to build and test it, where the artefacts are, and what still needs a person
with a credit card or a domain name.

**Artefacts built from:** commit `602f4e8` on `claude/new-session-zhnbkz`
**Version:** 0.1.0

---

## 1. What is implemented and working

Each item has been run, not just written. Where a test covers it, the file is
named so you can check the claim rather than take it.

### The platform

| | |
|---|---|
| **Desktop application** | Tauri 2. Single instance, tray, remembered window state, **opt-in** autostart, a narrow capability allow-list and a strict CSP. |
| **Local service** | axum on `127.0.0.1`, port chosen by the operating system, a bearer token minted per start, an origin allow-list, a 16 MiB body limit, SSE streaming. |
| **Database** | SQLite, `STRICT` tables, WAL, foreign keys on, ordered migrations, a backup before any schema change, a refusal to downgrade a newer schema. |
| **Secrets** | The operating system's credential store, an AES-256-GCM encrypted file when there is none, in-memory as a last resort — and the interface says which is in use. |
| **Providers** | Ollama, LM Studio, any OpenAI-compatible endpoint, including one on a non-default port. |

### The features

| | Covered by |
|---|---|
| Streaming chat, stoppable, persistent, self-titling | `e2e/01-first-run-and-chat.spec.ts` |
| Capability discovery labelled reported / probed / inferred | `packages/provider-adapters/src/capability.rs` |
| Knowledge: authorise, index, search, cite, revoke | `e2e/02-knowledge-and-citations.spec.ts`, `packages/knowledge/` |
| Prompt-injection boundary | `packages/knowledge/src/injection.rs` |
| Agents: CRUD, versions, test console, export, import | `packages/shared-types/src/agent.rs` |
| Projects: plan, approve, run, verify, report | `e2e/03-office-project-and-report.spec.ts`, `packages/agent-core/` |
| Offices, Labs, Boardrooms, Think Tanks | `e2e/04-boardroom-session.spec.ts`, `packages/agent-core/tests/sessions.rs` |
| Marketplace round trip with simulated payments and moderation | `e2e/05-marketplace-round-trip.spec.ts` |
| Permissions, one-shot grants, emergency stop | `packages/permissions/src/lib.rs` |
| Upgrade over an existing data directory | `e2e/06-upgrade-preserves-data.spec.ts` |
| Relay: accounts, profiles, pairing, scoped tokens | `apps/relay-api/tests/relay_http.rs` |
| Desktop → relay synchronisation, with a receipt | `apps/local-service/tests/http_api.rs` |
| WordPress: pairing, sign-in, profile, synced metadata | `wordpress/tests/run-live-tests.php` |

## 2. What is deferred, and why

| | Why |
|---|---|
| **Signed installers** | Needs certificates only the owner can buy. The build wiring is in place. |
| **A deployed relay** | Needs a host, a domain and TLS. It runs and is tested; nothing pretends it is deployed. |
| **Relay email** | Registration returns the verification token in the response because no mail service is configured. Marked in the code. |
| **Real payments** | Out of scope by design. The marketplace is a simulator and says so everywhere. |
| **Background synchronisation** | Deliberate: a push the user triggers can produce a receipt they can check. |
| **Speech, image generation, plugin APIs, analytics** | Not in scope for the MVP. |

## 3. Artefacts

Built here, on Linux, from commit `602f4e8` (documentation commits after it
change no code):

```
releases/0.1.0/
```

| File | Size | SHA-256 |
|---|---|---|
| `OTWONO AI_0.1.0_amd64.deb` | 6,166,898 bytes | `3c35bc1ff8a2fcd55b64842c22cd88a1faddb943b482a57fd063964fc1f34256` |
| `otwono-ai-connector.zip` | 27,469 bytes | `ce45fc8d5a4791d4f93081f4bdee01ec34313cf50fd90e84f9e30ebf28b7d969` |
| `RELEASE_NOTES.md` | 3,693 bytes | `ed30d18627cfb54edc11fb8a8cf3f01e6442464c762d7d0bb1f0b29b2e720516` |

`SHA256SUMS` in that folder carries the same values.

**Windows and macOS installers were not built here.** Tauri's bundlers need each
platform's own tooling — this is decision D-003, not an oversight. Both are
scripted (`scripts/build-windows.ps1`) and wired into
`.github/workflows/release.yml`, which builds all three on a `v*` tag and leaves
the GitHub release as a draft.

## 4. Repository structure

```
apps/
  desktop/         Tauri shell (Rust + config)
  local-service/   The local HTTP service
  relay-api/       The optional hosted service
  web/             React + TypeScript interface
packages/
  shared-types/    The shared vocabulary
  store/           SQLite, migrations, repositories, secrets
  permissions/     The permission engine and path policy
  provider-adapters/ Ollama, LM Studio, OpenAI-compatible
  knowledge/       Parse, chunk, embed, retrieve, cite, wrap
  agent-core/      Prompts, orchestration, verification, sessions
  ui/              Design tokens and the theme
wordpress/         The plugin and both of its test suites
e2e/               Playwright against the real service
scripts/           verify, build-release, build-windows, packaging, live tests
.github/workflows/ verify.yml, release.yml
```

90 Rust source files across 9 crates; 33 TypeScript/TSX files in the interface
and design system.

## 5. Building and testing

```bash
npm install                    # once

./scripts/verify.sh            # everything CI runs

npm run desktop:dev            # the application, with hot reload
npm run service                # the service alone
npm run dev                    # the interface alone, against the service

./scripts/build-release.sh     # the release folder for this platform
pwsh -File scripts/build-windows.ps1    # on Windows
```

Individual suites:

```bash
cargo test --workspace
npm run test
php wordpress/tests/run-tests.php
./scripts/run-wordpress-live-tests.sh
npx playwright test
```

The end-to-end suite needs the service binary and the built web assets:
`cargo build -p otwono-local-service && npm run build`.

## 6. Test results at this commit

| Suite | Result |
|---|---|
| Rust, 9 crates | **495 passing**, 0 failing |
| Frontend (vitest) | **18 passing** |
| WordPress plugin | **28 passing** |
| WordPress against a live relay | **6 passing** |
| End to end (Playwright, real service, real web build) | **15 passing** |
| `cargo clippy --workspace --all-targets -- -D warnings` | Clean |
| `cargo fmt --check`, `npm run format:check` | Clean |
| `npm run typecheck` | Clean |

Nothing is skipped, ignored or quarantined.

## 7. Known limitations

Full list in `STATUS.md`. The ones that matter to a user:

1. **Installers are unsigned.** Windows warns; macOS needs right-click → Open.
2. **Windows and macOS packages are not built in this environment.** The jobs
   exist and have not been run on those platforms from here.
3. **AppImage fails in a minimal container** for want of `xdg-open`. The `.deb`
   is unaffected.
4. **No relay is deployed**, so WordPress sign-in needs you to deploy one.
5. **The relay does not send email.** Registration returns the verification
   token in the response. Change this before real users.
6. **Without an embedding model**, search matches words rather than meaning —
   said plainly on the screen.
7. **The marketplace is a development preview** with simulated payments.
8. **One operating-system account per installation.** No multi-user desktop
   mode.

## 8. Clean install

**Windows.** Run the `.exe` or `.msi`. SmartScreen will warn; verify the
SHA-256 first. Data goes to `%APPDATA%\OTWONO AI`.

**macOS.** Open the `.dmg`, drag to Applications, then right-click → *Open* on
first launch. Data goes to `~/Library/Application Support/OTWONO AI`.

**Linux.** `sudo dpkg -i "OTWONO AI_0.1.0_amd64.deb"` then
`sudo apt-get install -f`. Data goes to `~/.local/share/otwono-ai`.

**Then:** install Ollama, `ollama pull llama3.1` and `ollama pull
nomic-embed-text`, open OTWONO, Connections → *Find local runtimes* → *Use
this* → *Test* → choose models → tick *Use this connection*.

Full instructions: `docs/INSTALL.md`.

## 9. Upgrade

Install over the top. A backup is taken before any schema change, migrations
run in a transaction, and a newer schema is refused rather than downgraded.
Nothing is lost — tested by `e2e/06-upgrade-preserves-data.spec.ts`.

Full instructions: `docs/UPGRADE.md`.

## 10. WordPress

1. Deploy the relay behind TLS (`docs/ADMIN_GUIDE.md` §3).
2. Upload `otwono-ai-connector.zip` through Plugins → Add New → Upload.
3. Settings → OTWONO AI → set the relay URL. It must be `https` and not a
   private host.
4. In OTWONO: Settings → *Show a pairing code*. Paste it into the plugin. It is
   single use.
5. Put `[otwono_login]`, `[otwono_profile]` and `[otwono_dashboard]` on a page.

Full instructions: `docs/WORDPRESS.md`.

## 11. Ollama and LM Studio

**Ollama.** Install from ollama.com, `ollama pull llama3.1`, `ollama pull
nomic-embed-text`. OTWONO finds it on `127.0.0.1:11434`.

**LM Studio.** Install from lmstudio.ai, download a model, start the local
server. OTWONO finds it on `127.0.0.1:1234`.

**Either on a different port, or anything else.** Connections → *Add a
connection by hand*, choose which runtime is listening, give the address.

## 12. Backup and restore

Everything is in one folder; the path is on Settings → *Your data*.

- **Back up:** Settings → *Your data* → *Back up now* (safe while running), or
  copy the folder while OTWONO is closed.
- **Restore:** close OTWONO, copy the backup over `otwono.sqlite3`, delete the
  `-wal` and `-shm` files, start it, re-enter API keys.

Full instructions: `docs/BACKUP.md`.

## 13. Security and privacy

Full detail in `SECURITY.md` and `docs/THREAT_MODEL.md`. The short version:

- The service is loopback-only, token-authenticated and origin-checked.
- Secrets are never in the database; the interface says which store holds them.
- An agent package can never carry a credential.
- Permissions are deny-by-default; a narrower deny beats a broader allow; the
  emergency stop overrides everything.
- There is **no shell capability** and no path that runs a model-written
  command.
- Retrieved text passes through one wrapper that marks it as data.
- No telemetry, no training on user data, no upload of your files.
- The three ways anything can leave the machine: a provider connection you
  pointed off-device, an `http_fetch` grant limited to hosts you approved, and
  a synchronisation you triggered, which answers with a receipt.

## 14. What the owner still has to provide

None of these could be obtained without your consent and money, so none has
been.

| | Needed for | Roughly |
|---|---|---|
| **A Windows code-signing certificate** | Removing the SmartScreen warning. An EV certificate builds reputation immediately. | A few hundred per year |
| **An Apple Developer Program membership** | Signing and notarising the macOS build. | $99/year |
| **A host, domain and TLS certificate** | Deploying a relay, only if you want WordPress sign-in. | Small VPS + a domain |
| **A transactional mail service** | Account verification on a deployed relay. | Free tier is enough to start |
| **A payment provider** | Only if you ever want the marketplace to be real. That is a much larger piece of work — money movement, KYC, disputes, tax — and it is deliberately not started. | — |

## 15. Where to look first

| If you want to… | Read |
|---|---|
| Understand the product | `PRODUCT_SPEC.md` |
| Understand the code | `ARCHITECTURE.md`, then `DECISIONS.md` |
| Check a security claim | `SECURITY.md`, `docs/THREAT_MODEL.md` |
| Use it | `docs/USER_GUIDE.md` |
| Deploy it for others | `docs/ADMIN_GUIDE.md` |
| Fix something | `docs/TROUBLESHOOTING.md` |
| Ship it | `docs/RELEASE.md` |
| Show it to someone | `DEMO_SCRIPT.md` |
| Know what is not done | `STATUS.md` |

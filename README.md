# OTWONO AI

**On The Work Of No One.** A local-first AI work platform: agents, projects and
knowledge that run on your own machine, against models you choose, with your
files staying where they are.

Nothing here needs an account. There is no cloud service to sign up for, no
telemetry, and no model training on your data. Point OTWONO at a runtime you
are already running — Ollama or LM Studio — and it works. Everything it knows
lives in one folder you can copy, back up or delete.

## What it does

| | |
|---|---|
| **Chat** | Streaming conversations against a local model, with citations when an answer used your files. |
| **Agents** | Ten shipped templates you can copy and edit. Each is a set of instructions, a model and a narrow list of things it is allowed to do. |
| **Knowledge** | Folders *you* authorise are indexed on this machine. Nothing is uploaded. Revoking a folder deletes what was indexed from it, immediately. |
| **Projects** | Describe an outcome; read the plan before it runs; approve it; watch each task be done and checked. |
| **Workspaces** | Offices (standing teams), Labs (compare configurations), Boardrooms and Think Tanks (structured sessions that end in a synthesis *and* the dissent). |
| **Marketplace** | Post work for a person to do. Payments are simulated: no money moves, and the interface says so on every screen. |
| **WordPress** | An optional plugin so members of your site can sign in and see what you chose to publish. |

## Getting started

```bash
git clone https://github.com/sublimecatch22/otwono.git
cd otwono
npm install
npm run desktop:dev    # the desktop application, with hot reload
# or
npm run dev            # the interface alone, against `npm run service`
```

See **[docs/INSTALL.md](docs/INSTALL.md)** to install a built copy, and
**[docs/USER_GUIDE.md](docs/USER_GUIDE.md)** for a tour.

## Repository layout

```
apps/
  desktop/        Tauri 2 shell: window, tray, single instance, autostart
  local-service/  The local HTTP service: loopback only, token-authenticated
  relay-api/      Optional hosted service for accounts and published metadata
  web/            The interface, React + TypeScript
packages/
  shared-types/   The vocabulary every crate agrees on
  store/          SQLite, migrations, repositories, the secret vault
  permissions/    Deny-by-default capability checks
  provider-adapters/ Ollama, LM Studio, OpenAI-compatible
  knowledge/      Parse, chunk, embed, retrieve, cite
  agent-core/     Prompts, orchestration, verification, sessions
  ui/             Design tokens and the theme
wordpress/        The plugin, and its tests
e2e/              End-to-end tests against the real service
scripts/          Verification, packaging and release
```

## Checks

```bash
./scripts/verify.sh    # everything CI runs
```

## Documentation

| | |
|---|---|
| [PRODUCT_SPEC.md](PRODUCT_SPEC.md) | What it does, and what it deliberately does not |
| [ARCHITECTURE.md](ARCHITECTURE.md) | How the pieces fit |
| [DATA_MODEL.md](DATA_MODEL.md) | Every table and why it exists |
| [API_SPEC.md](API_SPEC.md) | The local service and relay APIs |
| [SECURITY.md](SECURITY.md) | The security and privacy rules, and how they are enforced |
| [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md) | What we defend against, and what we do not |
| [DECISIONS.md](DECISIONS.md) | Decisions taken, with their reasons |
| [STATUS.md](STATUS.md) | What is built, what is not, and what is known to be limited |
| [docs/USER_GUIDE.md](docs/USER_GUIDE.md) | Using it |
| [docs/ADMIN_GUIDE.md](docs/ADMIN_GUIDE.md) | Running it for other people |
| [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) | When something is wrong |
| [docs/WORDPRESS.md](docs/WORDPRESS.md) | The plugin, start to finish |
| [docs/RELEASE.md](docs/RELEASE.md) | Building the installers |
| [DEMO_SCRIPT.md](DEMO_SCRIPT.md) | A ten-minute demonstration |

## Licence

See [LICENSE](LICENSE).

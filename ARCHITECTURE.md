# OTWONO AI — Architecture

`OTWONO` = **On The Work Of No One**.

This document records the architecture actually implemented in this repository.
Deviations from the controlling specification (`docs/MASTER_BUILD_PROMPT.md`) are
recorded in `DECISIONS.md`.

## 1. Shape of the system

```
┌──────────────────────────────────────────────────────────────────┐
│  Desktop application (Tauri 2)                                   │
│                                                                  │
│  ┌────────────────────────┐        ┌──────────────────────────┐  │
│  │  Web UI (React + TS)   │  HTTP  │  Local service (Rust)    │  │
│  │  apps/web              │◀──────▶│  apps/local-service      │  │
│  │  bundled as app assets │  SSE   │  axum, loopback only     │  │
│  └────────────────────────┘        └────────────┬─────────────┘  │
│                                                 │                │
└─────────────────────────────────────────────────┼────────────────┘
                                                  │
        ┌─────────────────────────────┬───────────┴──────────┬─────────────────┐
        ▼                             ▼                      ▼                 ▼
   SQLite database             Local AI runtimes       Authorised files   OS credential
   (app data dir)              Ollama / LM Studio      (user-selected)    vault
                               OpenAI-compatible
```

The desktop shell owns the process lifecycle. The local service is compiled
**into** the desktop binary (it is a library crate with an optional standalone
`otwono-local-service` binary used for development, tests and headless
operation). There is no separate sidecar process to install, no bundled Python,
and no requirement for the user to install Node.js, Rust or Python.

An optional **relay API** (`apps/relay-api`) is a separate, self-hostable
service. It is the only component that is ever reachable from the public
internet, and it stores only approved account/profile/marketplace metadata.
The WordPress plugin talks to the relay, never to a user's localhost.

## 2. Crate and package layout

Rust workspace (root `Cargo.toml`):

| Path | Crate | Responsibility |
|---|---|---|
| `packages/shared-types` | `otwono-types` | Domain types, state machines, transition rules, agent package schema. No I/O. |
| `packages/store` | `otwono-store` | SQLite connection pool, versioned migrations, pre-migration backups, repositories, audit log. |
| `packages/permissions` | `otwono-permissions` | Deny-by-default policy engine, grants, scopes, emergency stop, redaction. |
| `packages/provider-adapters` | `otwono-providers` | Provider trait, capability discovery, Ollama / LM Studio / OpenAI-compatible adapters, detection. |
| `packages/knowledge` | `otwono-knowledge` | Source authorisation, parsing, chunking, embeddings, vector index, hybrid retrieval, citations. |
| `packages/agent-core` | `otwono-agent-core` | Agent schema + templates, import/export, orchestration engine, verification, workspaces, budget ledger, marketplace rules. |
| `apps/local-service` | `otwono-local-service` | axum HTTP API, SSE streaming, auth middleware, request validation. Library + `otwono-local-service` binary. |
| `apps/relay-api` | `otwono-relay` | Accounts, pairing, scoped tokens, profiles, synchronised project/task metadata, marketplace. |
| `apps/desktop/src-tauri` | `otwono-desktop` | Tauri 2 shell: window, tray, autostart, single instance, service supervision. |

TypeScript workspaces (root `package.json`):

| Path | Package | Responsibility |
|---|---|---|
| `packages/ui` | `@otwono/ui` | Design tokens, theme engine, accessible primitives shared by app surfaces. |
| `apps/web` | `@otwono/web` | The application UI. Built by Vite; output embedded in the desktop app. |

Other trees: `wordpress/otwono-ai-connector` (PHP plugin), `infrastructure/docker`
(relay + WordPress development stack), `scripts` (build/release/test),
`installers` (packaging inputs), `releases` (release artefacts), `docs`.

Business logic lives in Rust crates once. The web UI is a presentation layer;
the WordPress plugin is a presentation + transport layer over the relay API.

## 3. Local service security model

* Binds `127.0.0.1` only, on an OS-allocated ephemeral port.
* On start it writes a **runtime handshake file** into the application data
  directory (`runtime.json`, mode `0600` on Unix) containing the port and a
  256-bit random bearer token. The desktop shell reads it and injects it into
  the web view; no other local process can read it without the user's file
  permissions.
* Every request except `GET /health` requires `Authorization: Bearer <token>`
  (constant-time compared).
* `Origin` is validated against an allow-list (`tauri://localhost`,
  `http://localhost:<vite port>` in development). Requests carrying a
  disallowed `Origin` are rejected before routing — this blocks CSRF and
  DNS-rebinding style attacks from a browser the user happens to have open.
* Request bodies are size-limited; long-running work is bounded by step and
  timeout budgets.
* Secrets never enter the database: provider API keys and relay tokens go to the
  OS credential vault (Windows Credential Manager / macOS Keychain / Secret
  Service). Where no vault is available the service falls back to an
  AES-256-GCM encrypted vault file with a `0600` key file and **says so in the
  UI**; it never silently downgrades.

## 4. Data

SQLite (bundled — no system SQLite requirement) in the platform application data
directory:

* Windows: `%APPDATA%\OTWONO AI\`
* macOS: `~/Library/Application Support/OTWONO AI/`
* Linux: `~/.local/share/otwono-ai/`

Overridable with `OTWONO_DATA_DIR` for development and tests.

Migrations are ordered, embedded in the binary, applied in a transaction, and
recorded in `schema_migrations`. A timestamped copy of the database is taken
into `backups/` **before** any migration that would change the schema version.
User data and application code are strictly separated: upgrading the app never
touches the data directory except through a migration.

See `DATA_MODEL.md`.

## 5. Providers

`otwono-providers` exposes a `Provider` trait with explicit capability
discovery (`chat`, `streaming`, `tool_calling`, `structured_output`, `vision`,
`embeddings`, `context_length`). The UI disables or adapts features whose
capability is absent rather than failing at request time.

Implemented adapters:

* **Ollama** — native `/api/chat`, `/api/tags`, `/api/embeddings`.
* **LM Studio** — OpenAI-compatible `/v1/*` on the LM Studio default port.
* **OpenAI-compatible** — any base URL, optional API key; covers llama.cpp
  server, vLLM, LocalAI, and hosted OpenAI/Anthropic-compatible gateways.

Online providers stay disabled until the user supplies credentials, and the
credential is stored in the vault, never in SQLite.

## 6. Knowledge and retrieval

Only user-selected folders/files are readable, recorded as explicit grants that
can be revoked. Ingestion parses TXT, Markdown, PDF, DOCX, CSV and common source
files; chunks with overlap; embeds with the selected provider's embedding model
when one is available; otherwise uses a clearly-labelled deterministic lexical
vector so that retrieval still works offline. Retrieval is hybrid (cosine +
BM25-style lexical scoring) over a brute-force index — the smallest reliable
local implementation for MVP-scale corpora. Every retrieved chunk carries file
path, chunk ordinal and page/line locator, and answers cite them.

Ingestion status is truthful: a source is only `indexed` after parsing *and*
indexing succeed; failures are recorded with the error.

Retrieved content is wrapped in untrusted-content delimiters and the system
prompt states that retrieved text is data, never instructions. See
`docs/THREAT_MODEL.md`.

## 7. Orchestration

`otwono-agent-core` implements a bounded engine:

objective → project (state machine) → plan of tasks (dependency DAG) → agent
recommendation → execution (respecting dependencies, step limits, timeouts,
permission gates) → verification agent → rework within limits → deliverable +
completion report.

The engine is a durable state machine persisted after every transition, so an
interrupted run recovers on restart (running tasks are returned to `ready`,
`awaiting_approval` is preserved). Illegal transitions are rejected by
`otwono-types` and covered by tests. Tools are an explicit allow-list; every
tool call is written to the activity log; arbitrary shell execution is not a
tool in this MVP.

## 8. Desktop shell

Tauri 2 with: single-instance guard, system tray, optional launch at sign-in
(`tauri-plugin-autostart`), window state persistence, and an in-process local
service started before the web view loads. Windows packaging targets NSIS and
MSI. Cross-building Windows installers from Linux is not supported by the
bundler, so `scripts/build-windows.ps1` and a GitHub Actions workflow perform
the Windows build on Windows; Linux `.deb`/AppImage bundles are produced here.

## 9. WordPress bridge

Three transport modes, selected in plugin settings:

1. **Local development** — WordPress and desktop on the same machine/network,
   direct documented URLs.
2. **Hosted relay (MVP)** — WordPress ⇄ relay API. Only approved account,
   profile, project/task metadata and marketplace records synchronise. Prompts,
   files, knowledge and models never leave the device unless explicitly marked
   for synchronisation per project.
3. **Future** — device-to-device encrypted connectivity.

Pairing: the desktop shows a short-lived one-time pairing code; WordPress
exchanges it for a scoped, revocable token pair. The user's localhost service is
never exposed to the internet.

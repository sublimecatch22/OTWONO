# Decision log

Each entry records a decision that deviates from, resolves an ambiguity in, or
materially interprets `docs/MASTER_BUILD_PROMPT.md`. Newest last.

---

## D-001 — Rust local service inside Tauri, no Python sidecar
**Spec reference:** §3 "Prefer a Rust service within Tauri when practical."
**Decision:** All application logic is Rust, compiled into the desktop binary.
**Why:** A bundled Python runtime roughly triples installer size, adds a second
process lifecycle to supervise, and adds an unsigned-interpreter attack surface.
Retrieval, chunking, embeddings and orchestration were all implementable in Rust
without a meaningful loss of speed. `rusqlite` is used with `bundled` SQLite so
no system database is required on any platform.
**Reversible:** Yes — the provider and knowledge crates sit behind traits.

## D-002 — Single business-logic home, thin presentation layers
**Spec reference:** §17 "Avoid duplicating business logic."
**Decision:** State machines, permissions, agent schema and orchestration live
in Rust crates. The web UI and the WordPress plugin hold no duplicated rules;
the plugin validates input and renders, and defers all authority to the relay.

## D-003 — Windows installer is built on Windows, not cross-compiled
**Spec reference:** §2.1, §20 "A non-developer can install and launch the
Windows application."
**Decision:** `tauri.conf.json` targets NSIS + MSI. The build is driven by
`scripts/build-windows.ps1` and `.github/workflows/release-windows.yml`, which
run on a Windows runner. This repository's development container is Linux, so
the Linux `.deb`/AppImage bundle is what is produced and smoke-tested here.
**Why:** Tauri's NSIS/MSI bundlers require Windows tooling (WiX, makensis with
Windows resource compilation). Producing a fake `.exe` on Linux would violate
§22 ("do not pretend").
**Consequence:** The Windows installer artefact and its checksum are produced by
the release workflow, not by this container. This is stated plainly in the
handoff rather than claimed as done.

## D-004 — Lexical fallback embeddings, explicitly labelled
**Spec reference:** §3 "Local embeddings when available"; §20 "no unexplained
mock success messages."
**Decision:** When the selected provider exposes an embeddings model, real
embeddings are used. When none is available, the knowledge index uses a
deterministic hashed lexical vector so that retrieval and citations still work
offline. The UI and the API label such sources `embedding_model: "lexical-fallback"`
and the Knowledge screen shows a persistent "lexical retrieval (no embedding
model)" badge.
**Why:** The application must remain useful with no model connected (§8) while
never overstating capability.

## D-005 — Credential vault fallback is visible, not silent
**Spec reference:** §3 "Use Windows Credential Manager or an equivalent OS
credential vault."
**Decision:** `SecretStore` has two backends: the OS vault (`keyring`), and an
AES-256-GCM encrypted file vault used only when no OS vault exists (headless
Linux, containers). `GET /api/system/status` reports which backend is live and
the Settings screen shows it. Secrets are never written to SQLite in either case.

## D-006 — Tests exercise real adapters against a fake upstream, not mock providers
**Spec reference:** §19; §20 "no unexplained mock success messages."
**Decision:** The shipped application contains no mock/echo provider. Automated
tests start an in-process HTTP server that speaks the real Ollama and
OpenAI-compatible wire protocols, and the real adapters talk to it.
**Why:** Keeps test coverage honest and keeps the shipped surface free of fake
success paths.

## D-007 — Payments are a simulated ledger behind a real interface
**Spec reference:** §12, §13.
**Decision:** `PaymentAdapter` is a trait with exactly one implementation,
`SimulatedPaymentAdapter`. Every ledger row records `simulated: true`, every
marketplace money figure in the UI is prefixed "Simulated", and no adapter
capable of moving real funds exists in the codebase.

## D-008 — WordPress talks to the relay, never to localhost
**Spec reference:** §14 "Do not expose a user's localhost service directly to
the public internet."
**Decision:** Hosted mode is WordPress ⇄ relay ⇄ (desktop pull/push). The plugin
has no code path that dials a private address in hosted mode, and the relay
refuses to store prompt, file or knowledge content.

## D-009 — Blocks are server-rendered, no JS build step in the plugin
**Spec reference:** §14 "Shortcodes and/or blocks."
**Decision:** Blocks are registered with `register_block_type` +
`render_callback`, sharing the same renderers as the shortcodes.
**Why:** The plugin ZIP installs and runs with zero build artefacts, so it can
never ship stale compiled JS, and there is one renderer per surface.

## D-010 — Autostart is opt-in and off by default
**Spec reference:** §2.1 "can optionally launch at user sign-in."
**Decision:** `tauri-plugin-autostart` is wired but disabled until the user
enables it in Settings. Local-first software should not install a startup entry
without being asked.

## D-011 — Repository root is the monorepo root
**Spec reference:** §17 shows `otwono-ai/` as the top directory.
**Decision:** The existing repository root *is* `otwono-ai/`; the tree below it
matches the specified layout. Nesting an `otwono-ai/` directory inside the
repository would add a level with no benefit.

## D-012 — `packages/store` added to the specified package list
**Spec reference:** §17 package list.
**Decision:** Persistence is its own crate rather than being folded into
`agent-core`, so that `knowledge`, `permissions` and the relay can depend on
migrations and repositories without depending on the orchestrator.

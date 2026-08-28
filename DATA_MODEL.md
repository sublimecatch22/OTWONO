# Data model

Everything OTWONO knows is in one SQLite database:

```
<data directory>/otwono.sqlite3
```

| Platform | Data directory |
|---|---|
| Windows | `%APPDATA%\OTWONO AI` |
| macOS | `~/Library/Application Support/OTWONO AI` |
| Linux | `~/.local/share/otwono-ai` |

`OTWONO_DATA_DIR` overrides it, which is how the tests and portable
installations work. Alongside the database the directory holds `backups/`,
`attachments/`, `projects/<id>/` for artefacts, and — only when the operating
system has no credential vault — the encrypted secret file.

## How the database is opened

| Setting | Value | Why |
|---|---|---|
| `foreign_keys` | `ON` | A row that references a deleted parent is a bug, not a state to tolerate. |
| `journal_mode` | `WAL` | Reads do not block the write that is streaming a reply. |
| `synchronous` | `NORMAL` | Safe with WAL, and much faster than `FULL` for the write rate here. |
| Table definitions | `STRICT` | SQLite would otherwise store `"seven"` in an INTEGER column. |

Every write that touches more than one row is a transaction. Timestamps are
RFC 3339 strings in UTC, so they sort as text and read as dates.

## Migrations

Migrations are embedded in the binary, ordered, and applied inside a
transaction. Two rules matter:

1. **A backup is taken before any schema change**, through SQLite's own backup
   API rather than a file copy, into `backups/`. It is named for the version it
   was taken at.
2. **A database from a newer version is refused, not downgraded.** If you open
   data written by a later release, OTWONO stops and says so rather than
   dropping the columns it does not recognise.

| File | What it adds |
|---|---|
| `0001_core.sql` | settings, providers, workspaces, agents, conversations, knowledge, projects, permissions, activity, budgets, sessions, lab experiments |
| `0002_marketplace_and_account.sql` | the relay link, pairing codes, worker profiles, listings and everything around them, rate limits |

## The tables

### Settings and providers

| Table | Holds | Notes |
|---|---|---|
| `settings` | Key/value preferences | Interface choices, the telemetry opt-in (off), onboarding state. |
| `provider_connections` | A runtime you connected | Kind, label, endpoint, chosen models, enabled. **Never the API key** — only whether one exists. |

### Agents and workspaces

| Table | Holds | Notes |
|---|---|---|
| `agents` | Instructions, model, capabilities, limits | Capabilities are a list, not a wildcard. |
| `agent_versions` | A snapshot per edit | So a change can be undone. |
| `workspaces` | Office, Lab, Boardroom, Think Tank | Shared instructions live here. |
| `workspace_members` | Who is in a workspace | Including which member is the coordinator. |

### Conversations

| Table | Holds | Notes |
|---|---|---|
| `conversations` | Title, model, chosen knowledge sources | Deleting one deletes its messages. |
| `messages` | Role, content, citations, token estimate | `ordinal` gives a stable order that does not depend on timestamps. |

### Knowledge

| Table | Holds | Notes |
|---|---|---|
| `knowledge_sources` | A folder you authorised | Including whether authorisation is current. |
| `documents` | One row per file | State — indexed, skipped, failed — and the reason, so nothing disappears silently. |
| `chunks` | The text passages | With the locator that makes a citation checkable. |
| `chunk_vectors` | The embedding for a chunk | Separate from `chunks` so re-embedding does not rewrite the text. |

Revoking a source deletes its chunks and vectors in the same transaction that
records the revocation.

### Projects

| Table | Holds | Notes |
|---|---|---|
| `projects` | Objective, acceptance criteria, budget, step limits, `sync_enabled` | The orchestrator and verifier are agent references. |
| `tasks` | Instructions, criteria, state, attempt count, output, verification notes | The state machine is enforced in code, not by convention. |
| `task_dependencies` | The edges of the plan | Cycles are refused when the plan is built. |
| `artifacts` | Files a project produced | Under `projects/<id>/`, the only place `file_write` may target. |

### Permission and audit

| Table | Holds | Notes |
|---|---|---|
| `permission_grants` | What is allowed, to whom, over what scope | With expiry and revocation. A one-shot grant is consumed on use. |
| `permission_requests` | What an agent asked for and how it was answered | Open requests are surfaced in the interface. |
| `activity_log` | Who did what, when, and how it ended | Details are redacted before they are written. |

### Money, simulated

| Table | Holds | Notes |
|---|---|---|
| `budgets` | A limit and an approval threshold | A simulator and an approval ledger. Not banking. |
| `expenses` | Estimated, approved, recorded | Nothing is ever paid. |

### Sessions and labs

| Table | Holds |
|---|---|
| `sessions` | A boardroom or think-tank question, its stage, the synthesis, the dissent, what is unresolved |
| `session_contributions` | Each agent's turn, its stage, and whether the claim was sourced or speculation |
| `lab_experiments` | A prompt, its variants and their results |

### Account and marketplace

| Table | Holds | Notes |
|---|---|---|
| `relay_links` | The relay you linked, the account, the scopes | **Not the token** — that is in the credential vault. |
| `pairing_codes` | Only the hash of a code | Single use, short lived. |
| `worker_profiles` | The local half of a marketplace identity | |
| `listings`, `applications`, `submissions`, `marketplace_messages` | The round trip | |
| `marketplace_ledger` | Simulated payouts | Every row carries `simulated`, and one function writes them. |
| `moderation_reports` | What someone reported, and the outcome | |
| `rate_limits` | Fixed-window counters | Applications, messages, reports. |

## What is *not* in the database

| | Where it is instead |
|---|---|
| API keys, relay tokens, the vault passphrase | The operating system's credential store, or an encrypted file when there is none. The interface says which. |
| The service's own bearer token | Minted per start, in memory, handed to the window through an owner-readable handshake file. |
| Your documents | Left where they are. Only extracted text and vectors are stored, and only for folders you authorised. |
| Anything sent to a relay | Only what you explicitly synchronised: a project's id, title, state and task counts. |

## Backup, export and restore

- **Back up now** in Settings takes a consistent copy through the SQLite backup
  API, so it is safe while the application is running.
- **Export** is per thing, so you take what you want and nothing else:
  interface settings (Settings screen), an agent as a package (Agents), a
  conversation as Markdown (Chat), a project's completion report (Projects),
  and the activity log as a plain-text report (Activity).
- **Restore** is a file copy: stop OTWONO, put the backup at
  `<data directory>/otwono.sqlite3`, start it again.

Full instructions are in [docs/BACKUP.md](docs/BACKUP.md).

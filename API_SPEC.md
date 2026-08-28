# API specification

Two HTTP APIs. They are not the same thing and do not trust each other.

| | The local service | The relay |
|---|---|---|
| Runs | On your machine, started by the desktop shell | On a server, only if you deploy one |
| Listens on | `127.0.0.1`, port chosen by the operating system | Whatever you bind it to |
| Authenticates with | A bearer token minted at start-up | Per-account bearer tokens with scopes |
| Knows about | Everything | Accounts, profiles, and metadata you chose to send |

---

## Part 1 — the local service

### How a caller is trusted

Every request under `/api` must satisfy all of:

1. **A bearer token.** Minted afresh each start, never written to the database.
   It is handed to the window through `<data directory>/runtime.json`, which is
   written owner-readable only.
2. **A known `Origin`, if one is present.** The packaged window
   (`tauri://localhost`, `https://tauri.localhost`) and the development server
   (`http://localhost:1420`, `http://127.0.0.1:1420`). A request with no
   `Origin` is not a browser cross-site request and still needs the token; a
   request with an unknown one is refused whatever token it carries.
3. **A body within the limit** — 16 MiB. A larger body is refused rather than
   buffered.

`GET /health` is the one exception: it needs no token, so a supervisor can
check the service is alive without holding a credential. It reveals nothing but
liveness.

An unknown path under `/api` is a JSON `404`. A `CORS` preflight is answered by
the guard itself, so an unknown path preflights as `404` rather than `405`.

### Errors

```json
{ "error": { "code": "forbidden", "message": "…", "retryable": false } }
```

`message` is written to be shown to a person. `retryable` says whether trying
again could plausibly help.

### Streaming

`POST /api/conversations/{id}/messages` answers with `text/event-stream`. The
event types are `start`, `citations`, `delta`, `done` and `error`. A dropped
connection stops generation; the partial reply is kept, marked with why it
stopped.

### The routes

#### System

| | |
|---|---|
| `GET /health` | Liveness. No token needed. |
| `GET /api/system/status` | Version, schema version, data directory, which secret backend is in use, whether the emergency stop is on. |
| `POST /api/system/emergency-stop` | Turn the stop on or off. While it is on, every permission check fails. |
| `POST /api/system/backup` | Take a consistent copy now. |
| `GET /api/system/backups` | List the copies taken. |

#### Settings

| | |
|---|---|
| `GET`/`PUT /api/settings/preferences` | Interface preferences. Values are validated against a fixed list, not stored as free text. |
| `POST /api/settings/preferences/reset` | Back to the defaults. |
| `GET /api/settings/export`, `POST /api/settings/import` | A settings file, refused if it is not one. |

#### Connections

| | |
|---|---|
| `GET`/`POST /api/connections` | List, or add. The list says whether chat is ready and what to do if not. |
| `POST /api/connections/detect` | Probe the documented local ports for Ollama and LM Studio. Nothing off this machine is contacted. |
| `PUT`/`DELETE /api/connections/{id}` | Update or remove. An API key sent here goes to the credential vault, never the database. |
| `POST /api/connections/{id}/test` | Health, latency, and the models with **how each capability was established**: reported by the runtime, proved by a probe, or inferred from the name. |

#### Agents

| | |
|---|---|
| `GET`/`POST /api/agents` | List or create. |
| `GET`/`PUT`/`DELETE /api/agents/{id}` | Read, edit (records a version), delete. |
| `GET /api/agents/templates`, `POST /api/agents/templates/seed` | The ten shipped templates; restore any that are missing. |
| `GET /api/agents/{id}/versions`, `POST /api/agents/{id}/versions/{version}/restore` | History, and going back. |
| `GET /api/agents/{id}/export`, `POST /api/agents/import` | A portable package. **The exporter refuses to include a credential.** |
| `POST /api/agents/{id}/test` | One turn, no tools, nothing saved. Shows the exact instructions the model was given. |

#### Chat

| | |
|---|---|
| `GET`/`POST /api/conversations` | List or start. |
| `GET`/`PUT`/`DELETE /api/conversations/{id}` | Read, retitle or set sources, delete. |
| `POST /api/conversations/{id}/messages` | Send, and stream the reply. |
| `POST /api/conversations/{id}/preview` | The exact prompt that would be sent, before sending it. |
| `POST /api/conversations/{id}/truncate` | Cut the conversation back to a message, for edit-and-resend. |
| `GET /api/conversations/{id}/export` | Markdown. |

#### Knowledge

| | |
|---|---|
| `GET`/`POST /api/knowledge/sources` | List, or authorise a folder. |
| `PUT /api/knowledge/sources/{id}/authorisation` | Revoke or restore. **Revoking deletes the index for that folder in the same transaction.** |
| `POST /api/knowledge/sources/{id}/index` | Index it. The answer says how many files were indexed, unchanged, skipped and failed, and whether the fallback embedding was used. |
| `GET /api/knowledge/sources/{id}/documents` | Per-file state and the reason for anything not indexed. |
| `POST /api/knowledge/search` | Search sources you name. Hits carry file name, locator and score. |
| `GET /api/knowledge/browse` | Browse folders to choose one, marking what can be read. |

#### Projects

| | |
|---|---|
| `GET`/`POST /api/projects` | List or start. |
| `GET`/`PUT`/`DELETE /api/projects/{id}` | The project, its tasks and its artefacts. |
| `POST /api/projects/{id}/plan` | Turn the objective into tasks. **This does not run anything.** |
| `POST /api/projects/{id}/run` | Run within the step budget, stopping at any approval gate. |
| `POST /api/projects/{id}/tasks/{task_id}/decision` | Approve or decline a waiting task. |
| `POST /api/projects/{id}/state`, `POST /api/projects/{id}/tasks` | Move state by hand; add a task by hand. |
| `GET /api/projects/{id}/report` | The completion report, as Markdown. |

#### Workspaces

| | |
|---|---|
| `GET /api/workspaces/kinds` | The four kinds and what each is for. |
| `GET`/`POST /api/workspaces`, `GET`/`PUT`/`DELETE /api/workspaces/{id}` | The usual. |
| `POST /api/workspaces/{id}/duplicate` | Copy a workspace and its team. |
| `POST /api/workspaces/{id}/members`, `DELETE …/members/{agent_id}` | Staffing. |
| `POST /api/workspaces/{id}/sessions`, `GET …/{session_id}`, `POST …/run` | A boardroom or think-tank session and its transcript. |
| `POST /api/workspaces/{id}/experiments`, `POST …/run`, `POST …/promote` | Lab experiments, and promoting a winning variant onto an agent. |

#### Permissions

| | |
|---|---|
| `GET /api/permissions` | Current grants and open requests. |
| `GET /api/permissions/history` | What was asked and how it was answered. |
| `POST /api/permissions/grants`, `POST /api/permissions/grants/{id}/revoke` | Grant and revoke. |
| `POST /api/permissions/revoke-all` | Revoke everything, now. |
| `POST /api/permissions/requests/{id}/resolve` | Answer a request. |
| `POST /api/permissions/check` | Ask what would happen, without doing it. |

#### Budget — a simulator

| | |
|---|---|
| `GET`/`POST /api/budgets`, `GET /api/budgets/{id}` | Limits and approval thresholds. |
| `POST /api/budgets/{id}/expenses` | Record an intent. Over the threshold it waits for approval. |
| `POST …/expenses/{expense_id}/decision`, `…/receipt` | Approve or decline; attach a receipt. |

No endpoint here moves money. There is none to move.

#### Marketplace — simulated payments

| | |
|---|---|
| `GET /api/marketplace/listings` | Published work. |
| `POST /api/marketplace/listings` | Create. Moderation runs first and can refuse, naming the phrase that matched. |
| `GET /api/marketplace/my-listings`, `GET /api/marketplace/my-work` | What you posted; what you took on. |
| `POST …/{id}/state`, `…/apply`, `…/assign`, `…/submit`, `…/review` | The round trip. Accepting records a **simulated** payout. |
| `POST …/{id}/messages`, `…/report` | Talk to the other party; report a listing. |
| `GET /api/marketplace/ledger` | Simulated earnings. Every row is labelled. |
| `GET`/`PUT /api/marketplace/worker-profile` | The local half of a worker identity. |

#### Account

| | |
|---|---|
| `GET /api/account` | Whether an account is linked, its scopes, and a plain statement of what synchronisation sends. |
| `POST /api/account/link`, `POST /api/account/unlink` | Link, or unlink and delete the token. |
| `POST /api/account/pairing-code` | Mint a single-use code for a WordPress site. Only its hash is stored. |
| `POST /api/account/pairing-code/redeem` | Redeem one locally. |
| `POST /api/account/sync` | Send the metadata of projects you ticked. Answers with a receipt naming every title that left and the exact fields sent. |

#### Activity

| | |
|---|---|
| `GET /api/activity` | The log, filterable. |
| `GET /api/activity/export` | A plain-text report. |

---

## Part 2 — the relay

Optional. It exists so a WordPress site can show what you chose to publish
without ever reaching your machine.

### What it can and cannot hold

It has columns for accounts, profiles, pairings, and project **metadata**:
identifier, title, state, task counts. It has no column that could hold a
conversation, a file, a knowledge index or a model. A title over 300 characters
is refused with a message saying why.

### Authentication

Bearer tokens, stored only as SHA-256 hashes, each carrying a scope list:

| Scope | Allows |
|---|---|
| `profile.read` | Read your own profile |
| `profile.write` | Change it |
| `projects.read` | Read synchronised project metadata |
| `projects.write` | **Send** project metadata. A paired site never asks for this. |
| `marketplace.read`, `marketplace.write` | The marketplace surface |
| `tasks.read` | Task metadata |

Passwords are hashed with Argon2id. Sign-in answers the same way whether the
account is unknown or the password is wrong.

### Routes

| | |
|---|---|
| `GET /health` | Liveness. |
| `POST /v1/accounts` | Register. Returns the verification token in the response because no mail service is configured — deploying with mail should change this. |
| `POST /v1/accounts/verify`, `/sign-in`, `/sign-out`, `/reset`, `/reset/complete` | The account lifecycle. |
| `GET /v1/sessions`, `DELETE /v1/sessions/{id}` | See where you are signed in; revoke one. |
| `GET`/`PUT /v1/profile` | Your profile. Every field is private unless you say otherwise. |
| `GET /v1/profiles/{account_id}` | What another person may see: only fields marked public, plus an unmissable notice if the profile is an AI identity. |
| `POST /v1/pairings`, `POST /v1/pairings/redeem` | Single-use pairing codes, stored hashed. |
| `GET`/`POST /v1/projects` | Read metadata; send it (needs `projects.write`). |

### Limits

Registration, sign-in and pairing are rate limited per fixed window. The audit
log records a coarse IP prefix, not the address.

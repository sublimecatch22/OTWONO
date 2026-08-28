# Security and privacy

Every rule here is enforced somewhere in the code, and the place is named. If a
rule is only aspiration, it says so.

---

## 1. The shape of the thing

There is no server. The application is a desktop window talking to a service on
the same machine, and that service talks to a model runtime on the same machine.
Nothing leaves unless you ask it to, and the only paths that can send anything
off the device are:

1. A provider connection **you** pointed at an off-device endpoint.
2. `http_fetch`, which needs a grant and only reaches hosts on the project's
   approved list.
3. `POST /api/account/sync`, which you trigger by pressing a button, and which
   answers with a receipt naming exactly what left.

There is no fourth path.

## 2. The local service

**Loopback only.** It binds `127.0.0.1` on a port the operating system chooses,
so nothing on the network can reach it and nothing collides with a fixed port.
*(`apps/local-service/src/runtime.rs`, with a test that the bound address is
loopback.)*

**A token per start.** The bearer token is minted at start-up and never written
to the database. It reaches the window through `<data directory>/runtime.json`,
written owner-readable only. *(`apps/local-service/src/lib.rs`,
`packages/store/src/paths.rs::restrict_to_owner`.)*

**An origin allow-list.** A request carrying an `Origin` we do not know is
refused whatever token it presents, so a page in the user's browser cannot use
the service even if it somehow learned the token.
*(`apps/local-service/src/auth.rs`, with a test that a hostile origin is refused
*with* the right token.)*

**A body limit.** 16 MiB, refused rather than buffered.

**`/health` is the only unauthenticated route**, and answers nothing but
liveness.

## 3. Secrets

**Never in the database.** API keys, relay tokens and the vault key live
outside it. The database records only *whether* a credential exists.

Three backends, in preference order, and the interface tells you which is in
use:

| Backend | What it is |
|---|---|
| **Operating system** | Windows Credential Manager, macOS Keychain, the Secret Service on Linux. |
| **Encrypted file** | AES-256-GCM. The 256-bit key is in its own `0600` file, so a backup of the vault alone is useless. Used when there is no OS vault; the interface says so plainly rather than pretending. |
| **Ephemeral** | In memory for this session only. Used when neither of the above can be opened, and labelled as such. |

*(`packages/store/src/secrets.rs`.)*

**An agent package can never carry a credential.** The exporter refuses fields
whose normalised name is a credential word or contains a high-signal fragment.
The check ignores punctuation and case, and is deliberately *not* a substring
match, so `max_output_tokens` is not mistaken for a token.
*(`packages/shared-types/src/agent.rs`, with tests for both the dangerous and
the benign cases.)*

## 4. Permissions

Three rules, in order:

1. **The emergency stop overrides everything.** While it is engaged nothing is
   allowed, including a capability with a standing grant.
2. **The most specific matching grant wins**, and a deny at the same
   specificity as an allow wins — refusing wrongly is cheaper than permitting
   wrongly.
3. **No matching grant means "ask the human"**, never "yes".

A grant may be one-shot, in which case it is consumed on use, and every grant
can carry an expiry. `POST /api/permissions/revoke-all` withdraws everything at
once. *(`packages/permissions/src/lib.rs`, tested rule by rule.)*

**Capabilities are a closed list.** File read inside an authorised source, file
write inside the project's own artefact directory, knowledge search, HTTP GET
against an approved host, artefact creation, budget recording, marketplace
publishing, relay sync. **There is no shell capability, and no code path that
executes a model-written command.**

## 5. Prompt injection

Every piece of retrieved or fetched text passes through one function that wraps
it in markers the content cannot forge, and tells the model that everything
inside is data, not instructions. There is one wrapper, so it cannot be
forgotten at one call site.

Text containing classic injection phrasing is **flagged, not censored** — the
user sees that it was flagged and can read it. Silently altering a user's own
document would be worse than the risk.
*(`packages/knowledge/src/injection.rs`.)*

## 6. Knowledge and files

- **Nothing is indexed until you authorise the folder**, and an unauthorised
  source is refused before anything is read.
- **Revoking deletes what was indexed, in the same transaction** that records
  the revocation. Not on the next run.
- **Path policy**: `file_read` cannot escape an authorised source and
  `file_write` cannot escape the project's own artefact directory. Traversal
  attempts are refused on the resolved path, not the string.
  *(`packages/permissions/src/path_policy.rs`.)*
- Files that cannot be read are reported with the reason, distinguishing
  "nothing here to index" from "this broke while being read".

## 7. Honesty rules

These are security properties too, because a false claim on a screen is a
failure of the same kind.

| Rule | How it is kept |
|---|---|
| A capability is never claimed unless it was established | Every model capability carries whether it was reported by the runtime, proved by a probe, or inferred from the name — and the interface shows which. |
| Unverified work is never reported as passed | Without a verifier, finished work is reported as unchecked. |
| An AI identity is never presented as a person | The public profile carries an unmissable notice. |
| Simulated money is never presented as real | One function writes ledger rows; it always marks them simulated, and every screen says so. |
| Synchronisation is never silent | It happens when you press the button, and answers with a receipt naming what left. |

## 8. The relay

- Passwords: **Argon2id**. Tokens: stored as **SHA-256 hashes** only.
- Sign-in answers identically whether the account is unknown or the password is
  wrong.
- Tokens are **scoped and revocable**. Reading project metadata and writing it
  are different scopes, so a paired WordPress site cannot invent metadata in
  your account.
- Pairing codes are **single use**, short lived, and stored only as hashes.
- Profile fields are **private until you publish them**, field by field.
- Rate limits on registration, sign-in and pairing. The audit log stores a
  coarse IP prefix, not the address.
- **The relay has no column that could hold content.** A title over 300
  characters is refused with a message that says why.

## 9. The WordPress plugin

- Capability checks and nonces on every action; sanitise on the way in, escape
  on the way out.
- Each member's token is in their own user meta, never in a shared option, and
  **never returned to the browser**.
- The relay address must be `https` and must not be a private or loopback host,
  so the plugin cannot be pointed at something inside the network.
- Uninstalling **keeps member data by default**. Removing other people's data
  is a decision for a person, not a side effect of clicking uninstall.

## 10. What this does not protect against

Stated plainly rather than left implied. The full analysis is in
[docs/THREAT_MODEL.md](docs/THREAT_MODEL.md).

- **Another program running as you.** It can read your files, so it can read
  OTWONO's data directory and its handshake file. Nothing an application can do
  changes that.
- **A malicious model.** A model you connect can produce anything. The defences
  are the capability list, the approval gates and the sandboxed write path —
  not trust in the model.
- **A compromised machine.** Disk encryption and account hygiene are the
  operating system's job.
- **A relay you do not control.** If you point OTWONO at someone else's relay,
  what you synchronise is theirs to keep. The receipt tells you what that is.

## 11. Reporting a problem

Open an issue describing what you observed and how to reproduce it. If the
problem exposes user data, say so in the first line so it can be triaged first.

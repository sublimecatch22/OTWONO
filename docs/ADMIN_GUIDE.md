# Administrator guide

For someone deploying OTWONO for other people, or running the optional relay
and the WordPress plugin.

If you are one person on one machine, you do not need this document.

---

## 1. What is on the network, and what is not

**The desktop application is not a server.** Its service binds `127.0.0.1` on a
port the operating system chooses, refuses requests from origins it does not
know, and requires a bearer token minted at start-up. There is no configuration
that makes it listen on an interface other than loopback, and none should be
added: **do not put a reverse proxy in front of it.** If you need something on
the network, that is what the relay is for.

**The relay is the only component meant to be reachable.** It holds accounts,
profiles, and the project metadata a user chose to publish. It has no column
that could hold a conversation, a file or an index.

## 2. Deploying to workstations

The installers are per-user and need no administrator rights beyond installing
the package. Each user gets their own data directory; nothing is shared.

| | |
|---|---|
| **Model runtime** | Install Ollama or LM Studio too. OTWONO ships no model. |
| **Launch at sign-in** | Off by default. It is a per-user setting inside the application; deployment does not turn it on. |
| **Windows SmartScreen** | Unsigned builds warn about an unknown publisher. Sign them with your own certificate, or distribute through a channel that suppresses it. See [RELEASE.md](RELEASE.md). |
| **Backups** | Point your usual backup at each user's data directory. See [BACKUP.md](BACKUP.md). |

### Credential storage

OTWONO prefers the operating system's credential store. On a machine without
one — a bare Linux server, some locked-down images — it falls back to an
AES-256-GCM encrypted file whose key is in its own owner-only file, and **the
Settings screen says which is in use**. Check it after deployment; if it says
*Secrets are held in memory for this session only*, neither store could be
opened and keys will not survive a restart.

## 3. Running the relay

Optional. Do not deploy it unless people actually need to sign in from a
website.

```bash
cargo build --release -p otwono-relay
OTWONO_RELAY_DB=/var/lib/otwono/relay.sqlite3 \
OTWONO_RELAY_BIND=127.0.0.1:8788 \
OTWONO_RELAY_ORIGINS=https://your-site.example \
  ./target/release/otwono-relay
```

| Variable | Meaning |
|---|---|
| `OTWONO_RELAY_DB` | Path to its SQLite database. |
| `OTWONO_RELAY_BIND` | Address to listen on. Bind loopback and put TLS in front. |
| `OTWONO_RELAY_ORIGINS` | Comma-separated origins allowed to call it from a browser. |

**Put it behind TLS.** The WordPress plugin refuses a relay address that is not
`https`, and refuses private and loopback hosts, so a plain-HTTP relay simply
will not be accepted.

### What it stores

Accounts (email, Argon2id password hash, display name), profiles, session
tokens as SHA-256 hashes, hashed single-use pairing codes, and synchronised
project metadata: identifier, title, state, task counts. A title over 300
characters is refused with a message saying the relay stores titles and states,
not content.

### Email

**There is no mail service configured.** Registration returns the verification
token in the response instead of sending it. Before running this for real
users, wire up mail and stop returning that token — it is the one place the
relay is deliberately incomplete, and it is marked as such in the code.

### Backing it up

Copy the SQLite file. It is the whole state. Take the copy with
`sqlite3 relay.sqlite3 ".backup 'relay-backup.sqlite3'"` rather than `cp` while
it is running.

## 4. The WordPress plugin

Full instructions in [WORDPRESS.md](WORDPRESS.md). The short version:

1. Install the ZIP, activate it.
2. Settings → OTWONO AI → set the relay URL. It must be `https` and must not be
   a private address.
3. In the desktop application: Settings → *Show a pairing code*.
4. Paste the code into the plugin. It is single use.
5. Members sign in on the site with their OTWONO account.

The site holds one token for itself and one per member, each in that member's
own user meta. **No token is ever sent to the browser.**

**Uninstalling the plugin keeps member data by default.** Removing other
people's records is a decision for a person, not a side effect.

## 5. Multi-user machines

Each operating-system account is a separate installation as far as OTWONO is
concerned: separate data directory, separate credential store entries,
separate service on its own port. There is no shared state and no way for one
user's OTWONO to read another's.

Do not try to share a data directory between accounts. SQLite would allow it;
the credential store would not, and the result would be confusing.

## 6. Monitoring

| | |
|---|---|
| **Is it up?** | `GET http://127.0.0.1:<port>/health` — no token needed, and it reveals nothing but liveness. The port is in `<data directory>/runtime.json`. |
| **What happened?** | The Activity screen, or `GET /api/activity/export`. |
| **Something to stop right now?** | The emergency stop refuses every capability check until released. |

There is no telemetry. The opt-in setting exists and is off, and there is no
code that sends usage data anywhere, so switching it on would send nothing.

## 7. Things not to do

| | Why |
|---|---|
| Expose the local service to the network | It is designed for loopback. Use the relay. |
| Run the relay without TLS | The plugin refuses it, and passwords would cross in the clear. |
| Ship an agent package from a machine with keys | The exporter refuses to include them — but do not go looking for a way around it. |
| Share one data directory between users | The database would be shared and the credentials would not. |
| Present the marketplace as real payments | It is a simulator. Saying otherwise would be a false statement to the people doing the work. |

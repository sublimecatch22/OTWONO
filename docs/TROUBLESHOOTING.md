# Troubleshooting

Each entry says what you see, what is actually happening, and what to do.

---

## Starting up

### The window opens but says no model is connected

**Happening.** OTWONO started fine; it just has nothing to talk to. Everything
except chat still works.

**Do.** Connections → *Find local runtimes*. If nothing is found, check the
runtime is actually running:

```bash
curl http://127.0.0.1:11434/api/version   # Ollama
curl http://127.0.0.1:1234/v1/models      # LM Studio
```

If those answer but OTWONO does not find them, the runtime is on a different
port: *Add a connection by hand*, and **say which runtime it is** — Ollama and
an OpenAI-compatible server do not speak the same protocol, and choosing the
wrong one makes the test fail.

### Nothing happens when I launch it

**Happening.** Usually a second copy: OTWONO allows one instance and focuses
the existing window instead of opening another.

**Do.** Look for it in the tray or on another desktop. If there is genuinely no
window, close any `otwono` processes and start again.

### It says the database is from a newer version

**Happening.** Exactly what it says. You have opened data written by a later
release. OTWONO stops rather than dropping columns it does not understand.

**Do.** Install the newer version again, or restore a backup taken before the
upgrade — see [BACKUP.md](BACKUP.md).

### It says secrets are held in memory for this session only

**Happening.** Neither the operating system's credential store nor the
encrypted-file fallback could be opened. API keys will not survive a restart.

**Do.** On Linux, install and start a Secret Service provider
(`gnome-keyring`, `kwallet`). Otherwise check the data directory is writable —
the fallback needs to create its key file there.

---

## Chat

### The reply never arrives

**Do, in order:**

1. Connections → **Test**. Is the connection still reachable?
2. Is the model still installed? A model removed from Ollama disappears from
   OTWONO's list but stays selected on the conversation.
3. Is the runtime busy loading a large model? The first request after a start
   can take a while.
4. Open **Run details**. If it shows Started and nothing after, the request left
   OTWONO and the runtime has not answered.

### The reply stops part-way

**Happening.** Either you pressed Stop, or the runtime ended the stream. The
partial reply is kept and marked with why it stopped.

**Do.** Check the runtime's own log. A model that exceeds available memory is
often killed mid-generation.

### The answer ignored my files

**Do:**

1. Is the source ticked under **Knowledge for this chat**? Selection is per
   conversation.
2. Has the folder been indexed? The card shows the file count.
3. Try the same question in Knowledge → **Try a search**. If nothing comes back
   there, retrieval is the problem, not the model.
4. With no embedding model, search matches words rather than meaning. Ask using
   words that are actually in the document, or install `nomic-embed-text` and
   index again.

---

## Knowledge

### Indexing says files were skipped

**Happening.** Not an error. A file is *skipped* when there is nothing to index
— empty, whitespace only, binary, or over the 25 MB limit. Only a file that
broke while being read is *failed*.

**Do.** **Show files** for the reason against each one.

### A folder shows "Folder is missing"

**Happening.** The path no longer exists — moved, renamed, or an unmounted
drive. What was indexed is still searchable; it just cannot be refreshed.

**Do.** Restore the folder at the same path, or revoke the source and authorise
the new location.

### Search finds nothing at all

1. Has the source been indexed since it was authorised?
2. Is the source still authorised? Revoking deletes its index.
3. Are you searching sources you selected? A source you did not select is not
   searched.

### Search is matching words, not meaning

**Happening.** No embedding model is configured, so OTWONO is using its labelled
lexical fallback. It works; it just cannot match a paraphrase.

**Do.** `ollama pull nomic-embed-text`, choose it as the embedding model on the
connection, then **Index now** again.

---

## Projects

### Planning fails

**Do.** Planning needs a working model. Test the connection. If the model is
very small, it may not produce a usable plan — the orchestrator refuses a plan
it cannot parse rather than inventing one. Try a larger model.

### A project stops and does nothing

**Happening.** It is waiting for you, or it has run out of steps.

**Do.** Look for **Waiting for your decision** at the top of the project, or
the Tasks screen. Check *Last run*: `steps_used` at the budget means it stopped
because it was told to. Continue running, or raise the budget.

### A task keeps failing verification

**Happening.** The verifier will not pass work that does not meet the criteria.
That is the point.

**Do.** Read the *What needs to change* note. Often the acceptance criteria are
ambiguous — a criterion the verifier cannot check will never pass. Make it
concrete.

### Work is reported as unchecked

**Happening.** No verifier is chosen, so OTWONO will not claim the work was
checked.

**Do.** Choose a verifier under Settings on the project — the shipped
Verification Agent is fine — and run again.

---

## Permissions

### An agent cannot do something

**Do.** The Activity log records the refusal and the reason. Then:

1. Is the **emergency stop** engaged? While it is, nothing is allowed.
2. Does the agent hold the capability? They are a list on the agent, not a
   wildcard.
3. Is there a grant that covers this scope? A narrower deny beats a broader
   allow.
4. Was it a one-shot grant? Those are consumed on use.

### I granted something and it still refuses

Almost always a scope mismatch: a grant matches only if **every** scope it names
is present in the request. Use *Check* on the Permissions screen to see what
would happen without doing it.

---

## Account and WordPress

### Pairing says the code is invalid

Pairing codes are **single use** and short lived. If you pasted it twice, or
waited too long, mint a fresh one.

### The plugin will not accept my relay URL

It must be `https`, and it must not be a private or loopback host. That is
deliberate: it stops a site being pointed at something inside the network.

### Nothing appears on the WordPress site

1. Is the member signed in with their OTWONO account?
2. Did they mark those profile fields **public**? Everything is private by
   default, field by field.
3. For projects: has anything been synchronised? Marking a project for
   synchronisation is not the same as sending it. Press **Send project
   metadata** on the Settings screen; the receipt lists what left.

---

## Data

### How do I start completely fresh?

Delete the data directory. Everything OTWONO knows is in it.

| Platform | Path |
|---|---|
| Windows | `%APPDATA%\OTWONO AI` |
| macOS | `~/Library/Application Support/OTWONO AI` |
| Linux | `~/.local/share/otwono-ai` |

### Where do I find the log?

The Activity screen inside the application, and `GET /api/activity/export` for
a plain-text report. For lower-level detail, start the service from a terminal
with `RUST_LOG=debug`.

---

## Reporting a problem

Include: the version and schema version from Settings → *Your data*, what you
did, what happened, and the last few Activity entries. If it involves a model,
say which runtime and which model.

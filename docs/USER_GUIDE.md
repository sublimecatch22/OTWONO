# Using OTWONO AI

A tour of what each screen does, in the order most people meet them.

---

## The window

A tab bar down the left for the screens, a sidebar listing your chats,
workspaces and projects, and the work itself in the middle. The sidebar
searches across all three at once.

**The emergency stop** is always reachable. While it is engaged, every
permission check fails — including capabilities with a standing grant. Nothing
an agent asks for is allowed until you release it, and releasing asks you to
confirm.

---

## Connections

Before anything can answer you, OTWONO needs a model.

**Find local runtimes** probes the ports Ollama and LM Studio document. Nothing
off this machine is contacted. If your runtime is on a different port, use
**Add a connection by hand** and say which runtime is listening — the choice
matters, because Ollama and an OpenAI-compatible server do not speak the same
protocol.

Press **Test**. You will see the models it serves and, for each one, a column
called *How we know*:

| | |
|---|---|
| **reported** | The runtime told us. |
| **probed** | We asked it to do the thing and it worked. |
| **guessed from the name** | Neither of the above. Treat it as a hint. |

This exists so the interface never offers you a feature that fails the moment
you use it.

Choose a **default model**. If you have an embedding model — `nomic-embed-text`
is a good one — choose that too, then tick **Use this connection**.

An API key, if your endpoint needs one, goes to your operating system's
credential store. It is never written to the OTWONO database.

---

## Chat

**New chat**, type, send. `Ctrl`/`⌘` + `Enter` sends too. The reply streams;
**Stop** ends it and keeps what arrived, marked with why it stopped.

The conversation titles itself from your first message and survives a restart.

**Knowledge for this chat** appears once you have authorised a folder. Tick a
source and the model is given passages from it — and the answer says which
files they came from.

**Run details** opens a drawer showing what happened: when it started, whether
knowledge was retrieved, how many passages, when it finished. Useful when an
answer surprises you.

---

## Knowledge

**Authorise a folder** — browse to it, or type the path. Nothing is read until
you do this, and nothing is uploaded, ever.

**Index now** reads the files OTWONO understands (text, Markdown, code, CSV,
PDF, Word), splits them into passages, and stores them locally. The message
afterwards says how many files were indexed, unchanged, skipped and failed.

**Show files** lists every file with its state and, where something did not
index, the reason. A blank or unreadable file is *skipped* with an explanation;
only a file that broke while being read is *failed*.

**Try a search** shows exactly what OTWONO would retrieve and from where, before
you rely on it in a conversation.

**Revoke access** deletes everything indexed from that folder, immediately —
not on the next run.

> **Without an embedding model**, search matches words rather than meaning. The
> screen says so where it matters, rather than letting you assume otherwise.

---

## Agents

An agent is instructions, a model, and a **narrow list of things it is allowed
to do**. Ten ship with OTWONO. **Copy** one to get your own to edit.

Each edit records a version, and **Version history** puts one back.

**Test console** runs one turn with no tools and saves nothing. It shows the
exact instructions the model was given — worth reading when an agent behaves
oddly.

**Export** writes a portable package. **A package can never contain a
credential**: the exporter refuses keys, tokens, passwords and cookies by name,
so sharing an agent cannot share your key.

---

## Workspaces

Four kinds of place, each a different shape of work.

| | |
|---|---|
| **Office** | A standing team. Give it agents and shared instructions; projects filed here belong to it. |
| **Lab** | Run one prompt through different configurations, compare the answers, and promote the winner onto an agent. |
| **Boardroom** | A question, independent positions, critique, then the chair's synthesis. |
| **Think Tank** | The same shape, ending in a research brief that separates sourced findings from speculation. |

Add agents with **Add an agent**, and make one of them the coordinator — that
is who writes the synthesis.

A session's output is the synthesis, **the dissent**, what is still unresolved,
and a recommended decision. Agreement that was not reached is never reported as
if it were. Every contribution in the transcript is marked with its stage and
whether the claim was *sourced* or *speculation*.

---

## Projects

Describe an outcome and how you will know it is done — one criterion per line.
Optionally file it under an office.

**Plan the work** turns the objective into tasks with dependencies. **It runs
nothing.** Read the plan first.

**Approve and run** starts it, inside a step budget. A task marked as needing
approval stops and waits for you, with Approve and Decline beside it.

Each finished task is checked by the verifier against its criteria. Open
**Verification** on a task to read the reasoning. If a task fails, it is
reworked up to its limit and then reported as failed with what needs to change.

> If no verifier is chosen, finished work is reported as **unchecked** — never
> as passed.

**Completion report** produces a Markdown report you can read here or download.

**Synchronise to my account** marks a project for sending to a linked account:
its title and task states, never its content. Nothing is sent until you press
*Send project metadata* on the Settings screen.

---

## Tasks

Everything waiting for you, across every project, in one place. Approvals sit
here as well as on the project.

---

## Marketplace

For work a person should do rather than a model.

> **Payments are simulated.** No money moves, no worker is paid, and the ledger
> is a record of intent. Every screen says so.

**I need something done** — describe it, save it as a draft, review it, then
publish. Moderation runs before it is saved: prohibited work is refused, the
phrase that matched is named, and there is a route to a person if you think
that is wrong.

**I want to do work** — browse open tasks, apply, and once someone assigns you
the job it moves to *Work you have taken on*, where you submit it.

Accepting submitted work records a simulated payout on the ledger, labelled as
simulated.

---

## Activity

Everything that happened: who did it, when, and how it ended. Secrets are
redacted before anything is written. Export it as a plain-text report.

---

## Settings

**Appearance** — theme, accent, background, font, size, density. The choices
are a fixed list, not free-form CSS, so the interface cannot be made unreadable
or made to run something.

**Tabs and widgets** — hide the screens you do not use. Chat and Settings can
never be hidden, so you cannot lock yourself out of undoing it.

**Permissions** — every grant, with revoke beside it, and **Revoke everything**
for when you want it all gone at once.

**OTWONO account** — optional; everything works without one. If you link one,
*Send project metadata* pushes the titles and states of projects you ticked and
shows you a receipt of exactly what left the machine. *Show a pairing code*
mints a single-use code for a WordPress site.

**Your data** — the version, the schema version, **which credential store is in
use**, and where your data lives. **Back up now** takes a consistent copy while
OTWONO is running.

---

## Keyboard

| | |
|---|---|
| `Ctrl`/`⌘` + `Enter` | Send the message |
| `Tab` | Move between controls; everything is keyboard reachable |
| `Esc` | Close a dialog or drawer |

---

## Where your data is

| Platform | Path |
|---|---|
| Windows | `%APPDATA%\OTWONO AI` |
| macOS | `~/Library/Application Support/OTWONO AI` |
| Linux | `~/.local/share/otwono-ai` |

One folder. Copy it to back up; delete it to reset. See
[BACKUP.md](BACKUP.md).

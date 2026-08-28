# OTWONO AI — product specification

**On The Work Of No One.** A local-first, user-controlled AI work platform.

This document says what the product is for, who it is for, what it does, and —
just as importantly — what it deliberately refuses to do. Where a behaviour is
enforced by a test, the test is named, so a claim here can be checked rather
than believed.

---

## 1. The idea

Most AI products ask you to send your work somewhere else. OTWONO does the
opposite: the models run on your machine, your files stay in the folders they
are already in, and the application's entire memory is one directory you can
copy or delete.

The name is the promise. The work is done on nobody's behalf but yours, on
nobody's hardware but yours, and nobody else is watching.

## 2. Who it is for

| | |
|---|---|
| **Someone who wants an AI assistant that cannot leak their work.** | Lawyers, clinicians, accountants, journalists, researchers — anyone whose files are not theirs to upload. |
| **Someone running a small operation.** | One person coordinating several strands of work who wants a plan they can read before anything runs. |
| **Someone who wants to understand the machine.** | Every capability an agent holds is listed. Every off-device action is approved. Every run is in the log. |

It is **not** for someone who wants a hosted service with an account and a
monthly bill. There is no such thing here to sell.

## 3. What it does

### 3.1 Chat

Streaming conversations against a model on this machine. The reply can be
stopped mid-flight. A conversation titles itself from the first message and
survives a restart. When an answer used your files, it says which files, and
where in them.

*Covered by `e2e/01-first-run-and-chat.spec.ts` and
`e2e/02-knowledge-and-citations.spec.ts`.*

### 3.2 Agents

An agent is a name, a role, a set of instructions, a model and a **narrow list
of things it is allowed to do**. Ten templates ship. Copying one gives you your
own to edit; editing records a version you can go back to.

Agents can be exported as a package and imported on another machine. **A package
can never contain a credential**: the exporter refuses keys, tokens, passwords,
cookies and session identifiers by name, and the check is deliberately not a
substring match, so a field called `max_output_tokens` is not mistaken for a
secret.

*Covered by `packages/shared-types/src/agent.rs` tests.*

### 3.3 Knowledge

You choose folders. OTWONO indexes them **on this machine**: parse, chunk,
embed, store. Nothing is uploaded. Retrieval is hybrid — meaning and words
together — and every hit carries the file name and a human-meaningful location
(a page, a line range, a row range), so a citation points somewhere you can
actually look.

Two rules that matter more than the features:

- **Revoking a folder deletes what was indexed from it, immediately.** Not on
  the next run, not eventually.
- **Retrieved text can never issue instructions.** Everything retrieved is
  wrapped in a boundary the content cannot forge, and the model is told the
  material inside it is data.

*Covered by `packages/knowledge/src/{index,retrieve,injection}.rs` tests and
`e2e/02-knowledge-and-citations.spec.ts`.*

### 3.4 Projects

You describe an outcome and how you will know it is done. OTWONO turns it into
a plan of tasks with dependencies. **You read the plan before anything runs.**
Then each task runs within a step budget, and its output is checked against the
criteria by a verifier.

If there is no verifier, finished work is reported as **unchecked**, never as
passed. If verification fails, the task is reworked up to a limit and then
reported as failed with the reason.

*Covered by `packages/agent-core/src/{orchestrator,verify}.rs` tests and
`e2e/03-office-project-and-report.spec.ts`.*

### 3.5 Workspaces

Four kinds of place, each with a different shape:

| Kind | What it is |
|---|---|
| **Office** | A standing team that turns objectives into finished work. |
| **Lab** | A place to run the same prompt through different configurations and compare them, then promote the winner onto an agent. |
| **Boardroom** | A structured session: independent positions, then critique, then a chair's synthesis **and the dissent**. |
| **Think Tank** | The same shape, ending in a research brief that separates sourced findings from speculation. |

A session never reports agreement it did not find. The dissent and the
unresolved questions are part of the output, not an appendix.

*Covered by `packages/agent-core/tests/sessions.rs` and
`e2e/04-boardroom-session.spec.ts`.*

### 3.6 Human task marketplace (development preview)

Post work for a person to do; a worker applies; you assign, they submit, you
accept. **Payments are simulated.** The ledger is a record of intent. Every
screen says so, and the code has exactly one function that writes a ledger row
— it always marks it simulated.

Moderation refuses categories of work outright, names the phrase that matched,
and gives a route to a person for an appeal.

*Covered by `packages/shared-types/src/marketplace.rs` tests and
`e2e/05-marketplace-round-trip.spec.ts`.*

### 3.7 The optional account, and WordPress

You can link an OTWONO relay account. If you do, and only if you ask, the
desktop sends the **title, state and task counts** of projects you ticked —
nothing else. The response is a receipt naming what left the machine.

A WordPress site can be paired with a single-use code and given read-only
scopes, so members can sign in and see the profile fields you made public.

*Covered by `apps/local-service/tests/http_api.rs`,
`apps/relay-api/tests/relay_http.rs` and `wordpress/tests/run-live-tests.php`.*

## 4. What it deliberately does not do

These are not gaps. They are decisions, and most are enforced in code.

| It does not | Because |
|---|---|
| Send your files anywhere | Knowledge is indexed locally. There is no upload path. |
| Hold money, issue currency, or pay anybody | The marketplace ledger is simulated and says so everywhere. |
| Run model-written shell commands | There is no shell capability. Off-device actions need approval. |
| Store a secret in the database | Secrets go to the OS credential vault, or to an encrypted file the interface tells you about. |
| Listen on anything but loopback | The service binds `127.0.0.1` on an OS-chosen port and refuses unknown origins. |
| Collect telemetry | The opt-in setting exists and is off. There is no code anywhere that sends usage data, so switching it on would send nothing. |
| Train on your data | Nothing leaves the machine to train on. |
| Pretend to be a person | An AI identity is labelled wherever it appears, including in the public profile. |
| Send email | There is no mail path in the MVP. |

## 5. What "done" means for the MVP

- Installs from a built package, starts reliably, and can be upgraded over its
  own data without losing anything. *(`e2e/06-upgrade-preserves-data.spec.ts`)*
- Works with no account, no network and no cloud service.
- Connects to Ollama and LM Studio, and to any OpenAI-compatible endpoint.
- Every critical path in §19 of the build specification has an automated test
  that drives the real service, not a mock.
- Every claim on a screen is one the code can keep.

## 6. Deliberate limits of this version

Recorded honestly rather than omitted; see [STATUS.md](STATUS.md) for the
current state and [DECISIONS.md](DECISIONS.md) for why.

- Windows installers are built on Windows. This repository's Linux build
  produces the `.deb`; the Windows job is scripted and runs in CI.
- Without an embedding model, retrieval matches words rather than meaning, and
  the interface says so on the screen where it matters.
- The relay is a working service with tests, but it has not been deployed to a
  public address. Nothing in the product claims that it has.
- The marketplace is a development preview with simulated payments only.

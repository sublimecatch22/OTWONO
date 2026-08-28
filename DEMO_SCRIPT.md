# Ten-minute demonstration

A run through that shows what OTWONO is, in the order that makes the point.
Timings are the pace to aim for, not a limit.

## Before you start

- [ ] Ollama running with `llama3.1` and `nomic-embed-text` pulled
- [ ] A folder of a few real documents to index
- [ ] A **fresh data directory**, so the first-run screens are real
- [ ] Network **off**, if you can. It makes the point better than saying it.

---

## 0:00 — Open it (30 seconds)

> "This is OTWONO. It runs on this machine. There is no account, no sign-up,
> and — you will notice — no network."

Open the application. It goes straight to Chat and says no model is connected
yet.

> "It is honest about that. It also tells me what still works without one:
> agents, projects, indexing."

## 0:30 — Connect a model (1 minute)

Connections → **Find local runtimes** → **Use this** → **Test**.

Point at the *How we know* column.

> "For every model, it says how it knows what that model can do. **Reported** is
> the runtime telling us. **Probed** is us asking it to do the thing. **Guessed
> from the name** is a hint, and it says so. Most products would just show a
> tick."

Choose the default and embedding models, tick **Use this connection**.

## 1:30 — A conversation (1 minute)

Chat → **New chat** → ask something. Let it stream. Press **Stop** part-way, and
say the partial answer is kept and marked with why it stopped.

Reload the window.

> "Still here. It titled itself. Everything is in one folder on this disk."

## 2:30 — Your own files (2 minutes)

Knowledge → **Authorise a folder** → pick your folder.

> "Nothing was read until I said so, and nothing is uploaded. It reads the files
> here and stores the index here."

**Index now.** Read out the result: indexed, unchanged, skipped, failed.

**Show files.** Point at anything skipped.

> "It tells me *why* each file did not index. An empty file is skipped with a
> reason, not marked broken."

**Try a search** with a question your documents answer. Point at the file name
and locator on each hit.

> "That is where the answer would come from, before I trust it in a
> conversation."

Now Chat → new chat → tick the source → ask the same question.

> "And the answer says which file it used, and where. If it cannot find
> anything, it says that instead of guessing."

**The line to land:** point out that this ran with the network off.

## 4:30 — An agent (1 minute)

Agents → pick one → **Test console**.

> "One turn, no tools, nothing saved — and it shows the exact instructions the
> model was given. You can see what it was told."

Point at the capability list.

> "This is everything it is allowed to do. There is no shell. There is no way to
> add one. `file_write` cannot leave the project's own folder."

**Export.**

> "A portable package. It cannot contain a credential — the exporter refuses
> anything key-shaped, and the check is careful enough not to trip over a field
> called `max_output_tokens`."

## 5:30 — A project (2 minutes 30)

Projects → **Start a project**. Give an objective and two criteria.

**Plan the work.**

> "It has produced a plan. It has not done anything. This is the part most
> agent demos skip: I read the plan first."

Read a task out. Then **Approve and run**.

Watch it run. Open **Verification** on a finished task.

> "Each task was checked against the criteria by a different agent. If I had not
> chosen a verifier, this would say *unchecked* — never *passed*."

**Completion report** → **Download**.

## 8:00 — The boardroom (1 minute)

Workspaces → create a Boardroom → add three agents → make one the coordinator →
ask a question → **Run**.

> "Positions, then critique, then the chair's synthesis. And this —" *(point at
> Dissent)* "— is the part I care about. It reports the disagreement. It does
> not manufacture consensus."

## 9:00 — Control (45 seconds)

Settings → **Permissions**.

> "Every grant, with revoke beside it. Revoke everything is one button."

Press the **emergency stop**.

> "While that is on, nothing is allowed. Not even something with a standing
> grant. Releasing it asks me to confirm."

Release it.

Settings → **Your data**.

> "The version, which credential store is in use — that matters, and most
> software will not tell you — and the folder. One folder. Copy it to back up.
> Delete it to reset. That is the whole system."

## 9:45 — The point (15 seconds)

> "Everything you saw ran on this machine, against a model I chose, on files
> that never moved. It told me how it knew what it knew, showed me the plan
> before it acted, checked its own work, and reported the disagreement it found.
>
> That is the difference: not what it can do, but what it will tell you about
> what it did."

---

## If you have longer

| | |
|---|---|
| **Marketplace** | Post work, apply as the worker, assign, submit, accept. Point out that the simulated-payment notice is on every screen, and try posting something prohibited to see moderation name the phrase and offer a route to a person. |
| **Lab** | The same prompt through two configurations, side by side, then promote the winner onto an agent. |
| **Upgrade** | Show the backup in `backups/`, taken before a schema change. |
| **WordPress** | Pair with a code, sign in as a member, publish one profile field, and show that only that field appears publicly. |

## Questions you will get

**"Where does the data go?"** One folder on this machine. Point at Settings →
Your data.

**"Can it use GPT-4 or Claude?"** Yes — add an OpenAI-compatible endpoint with
a key. Then it is not local any more, and the connection screen is clear about
which connections point off the device.

**"How fast is it?"** Whatever your hardware and model give you. OTWONO adds
very little; the model is the cost.

**"Is the marketplace real money?"** No. It is a simulator and says so on every
screen. It cannot hold funds and there is no payment integration to enable.

**"What happens if the model makes something up?"** Same as anywhere — but the
verifier checks work against criteria you wrote, an unverified result is
reported as unchecked, and retrieved passages are cited so you can check them
yourself.

**"Does it phone home?"** No. There is no telemetry code to switch on. The one
outbound path is synchronisation, which happens when you press the button and
answers with a receipt of exactly what left.

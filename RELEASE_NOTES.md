# OTWONO AI 0.1.0

The first release. A local-first AI work platform that runs on your own
machine, against models you choose, with your files staying where they are.

---

## What is in it

**Chat** against a local model, streaming, stoppable, with citations when an
answer used your files. Conversations title themselves and survive a restart.

**Agents** — ten templates to copy and edit. Each is instructions, a model and
a narrow list of things it is allowed to do. Every edit records a version. An
exported package **can never contain a credential**.

**Knowledge** — folders you authorise are indexed on this machine. Nothing is
uploaded. Hybrid retrieval, and every hit carries the file name and a place in
it. Revoking a folder deletes its index immediately.

**Projects** — describe an outcome, read the plan before anything runs, approve
it, and have each task checked against your criteria. Without a verifier,
finished work is reported as *unchecked*, never as passed.

**Workspaces** — Offices, Labs, Boardrooms and Think Tanks. A session reports
the synthesis **and the dissent**.

**Marketplace (development preview)** — post work for a person to do.
**Payments are simulated**: no money moves, and every screen says so.
Moderation refuses prohibited work, names the phrase that matched, and offers a
route to a person.

**Optional account and WordPress plugin** — pair a site with a single-use code;
members sign in and see what you published. Synchronisation sends the title and
state of projects you ticked, when you press the button, and shows a receipt of
exactly what left the machine.

## Installing

See `docs/INSTALL.md`. In short: install Ollama or LM Studio, pull a model,
install OTWONO, and connect it on the Connections screen.

**Verify your download** against `SHA256SUMS` before running it.

## Please read

- **The installers are unsigned.** Windows SmartScreen will warn about an
  unknown publisher; macOS needs right-click → *Open* on first launch. Signing
  needs certificates the project owner must buy.
- **Marketplace payments are simulated.** Nothing holds or moves money, and
  there is no payment integration to enable.
- **No relay is deployed.** The relay runs and is tested, but no public
  instance exists and nothing in the product points at one. WordPress sign-in
  needs you to deploy your own.
- **Without an embedding model**, search matches words rather than meaning. The
  interface says so where it matters.

## Privacy

No telemetry. No training on your data. No account required, and no cloud
service behind it. Everything OTWONO knows is in one folder you can copy or
delete; the path is on the Settings screen.

The only ways anything leaves your machine are a provider connection you
pointed off-device, an `http_fetch` grant limited to hosts you approved, and a
synchronisation you triggered. See `SECURITY.md`.

## Upgrading

Install over the top. A backup is taken before any schema change, migrations
run in a transaction, and a database written by a newer version is refused
rather than downgraded. See `docs/UPGRADE.md`.

## Known limitations

Listed in full in `STATUS.md`. The main ones: unsigned installers; no deployed
relay; the relay does not send email, so registration returns the verification
token in the response instead; the marketplace is a preview; one operating-system
account per installation.

## Verified before release

| | |
|---|---|
| Rust, 9 crates | 495 tests |
| Frontend | 18 tests |
| WordPress plugin | 28 tests |
| WordPress against a live relay | 6 tests |
| End to end, against the real service | 15 tests |

`./scripts/verify.sh` runs all of it, plus formatting, types and lints.

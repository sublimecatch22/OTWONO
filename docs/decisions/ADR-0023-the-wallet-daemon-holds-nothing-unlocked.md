# ADR-0023 — `otwono-walletd` holds nothing unlocked: the passphrase per call, no session, and no public key in the clear

**Status:** accepted · **Date:** 2026-08-25 · **STATUS: SPECIFIED** — `crates/otwono-wallet`
exists (keys, derivation, vault); the daemon this describes does not.

## Context

ADR-0022 decided the wallet's key material, its curve, its daemon, and its capabilities. It
did not decide how a passphrase gets from a person to `otwono-walletd`, and that question
turns out to carry most of the daemon's security properties.

Three things forced it now rather than at implementation time:

1. **A passphrase has to cross a socket**, and where it rests afterwards is the whole design.
2. **`crates/otwono-wallet` measured the Argon2id cost at 0.42 s** (19 MiB, t=2, p=1 — see
   `docs/services/WALLET.md` §3). That number decides whether an unlock cache is worth having,
   and it turns out to decide it against.
3. **Showing a receive address needs the seed**, unless something public is stored — which is
   a privacy decision disguised as a convenience feature.

One property this rests on, checked rather than assumed: **the audit log does not record
call parameters.** `AuditRecord` carries `action`, `resource`, `outcome` and `reason`, and
`perm.request` only ever sees an action name — the target service's params never reach
`otwono-permd` at all. A passphrase in params therefore does not reach the audit chain. That
is load-bearing for everything below, so it gets a test rather than a sentence.

## Decision

### 1. The passphrase is supplied per call, and the daemon holds no unlocked seed

No `wallet.unlock`. No session. No timer. Each call that needs the seed takes the passphrase,
derives the key, uses it, and drops it — the seed lives in `Zeroizing` for the duration of one
call and never outlives it.

The obvious alternative is the one every desktop wallet ships: unlock once, hold the seed for
fifteen minutes, lock on idle. It is rejected here, and the measurement is why.

**An unlock cache exists to amortise a cost.** At 0.42 s a call, on a key ADR-0022 says
"should be used rarely", there is nothing worth amortising. What the cache would buy is
convenience measured in fractions of a second; what it would cost is a window during which
spendable key material sits in a live process, reachable by anything that can reach that
process — which is precisely the thing ADR-0022 §1 chose this key's storage to avoid when it
said rarely-used keys can be protected in ways constantly-used ones cannot.

It also deletes a whole category of design: how long the window is, whether it extends on
use, what closes it, whether a lock survives a crash, and who else can spend inside it while
it is open. None of those questions have to be answered well, because none of them exist.

**The cost, stated:** every operation pays the KDF. A UI that derives ten addresses in a loop
pays it ten times, and should instead ask for one call that returns ten. That is a shape
constraint on the API and it is written down here so it is met by design rather than
discovered as slowness.

### 2. No extended public key is stored in the clear

The tempting feature: keep the account-level `xpub` outside the vault, so the finance surface
can show a receive address, a balance, and a history without ever asking for the passphrase.
Every consumer wallet that shows a balance on launch does something like this.

Rejected, because an `xpub` in the clear **is** the household's transaction graph. Anyone who
reads that file derives every address the account will ever use, past and future, and can
then watch all of them on a public chain forever. That is the exact harm ADR-0022 §5 chose
fresh-address-per-purpose to prevent, handed over in one file by a feature meant to save a
passphrase prompt.

So: **deriving any public key requires the passphrase.** The vault holds the seed and nothing
else stands beside it.

**The cost, stated plainly because a UI will feel it:** this node cannot display a receive
address, cannot show a balance, and cannot render history without the person unlocking. There
is no "at a glance" wallet screen. That is a real loss of convenience and it is the correct
trade — but the finance surface must be designed knowing it, not discover it late and reach
for an `xpub` cache to fix it.

If that becomes intolerable, the answer is an explicit, confirmed, audited decision to store a
watch-only key, with the linkability spelled out to the user in those words. It is not a
default and it is not this ADR.

### 3. Creating a wallet is its own capability, and it always confirms

ADR-0022 §3 named three actions — `wallet.read`, `wallet.sign`, `wallet.export_seed` — and did
not name creation. Creation is not any of them:

| Action | Blast radius | `always_confirm` |
|---|---|---|
| `wallet.read` | Read | no |
| **`wallet.create`** | **Irreversible** | **yes** |
| `wallet.sign` | Irreversible | yes |
| `wallet.export_seed` | Irreversible | yes |

`Irreversible` and confirmed for two independent reasons, either sufficient:

- **It returns the recovery phrase**, once, because the person must write it down. A caller
  that can invoke this unattended learns the seed of a wallet that is about to be funded. The
  disclosure is identical to `wallet.export_seed`; only the timing differs.
- **It mints the key that will hold money.** There is no undo, and the failure is silent: a
  wallet created by something other than its owner looks exactly like one created by its
  owner.

**Creating over an existing vault is refused outright**, not confirmed. A confirmation dialog
is the wrong instrument for "this destroys the key to your funds": the answer is no, and a
prompt invites a yes. Replacing a wallet means removing the file deliberately, by hand, having
read what that means.

### 4. The consequence nobody should discover later: the daemon is mostly unreachable until Phase 7

`policy.rs` turns `Allow` into `Ask` for any `always_confirm` action, no confirmation channel
exists, and `otwono-permd` answers `confirmation_required`. Three of the four actions above
are `always_confirm`.

So on a booted node today: **a wallet cannot be created, cannot sign, and cannot export.**
`wallet.read` works and reports that there is no wallet. ADR-0022 accepted this shape for
signing and called it correct rather than inconvenient; this ADR records that it extends to
almost the whole daemon, which ADR-0022 did not say and a reader would otherwise meet as a
surprise.

This is not an argument for weakening any of the three. It is an argument that **Phase 7's
confirmation channel is the wallet's real dependency**, and that the wallet is not the only
thing waiting on it: `fs.delete` and `net.egress` are already `always_confirm` and already
unreachable for the same reason. Whoever schedules Phase 7 should know it unblocks four
things, not one.

## Consequences

**Good.** There is no unlock window to attack, no session to steal, and no lifetime to get
wrong. The daemon is stateless with respect to secrets, which makes it restartable, testable
without timers, and honest about what it holds: nothing. No file beside the vault reveals the
household's addresses.

**Bad, and worth naming.**

- **Every seed-using call costs 0.42 s** on the development host and low seconds on a T0
  board. The API has to be shaped for batches, and a UI that loops will feel it.
- **No wallet screen without a passphrase.** No balance at a glance, no address to copy
  without unlocking. Users will find this worse than other wallets, and they will be right
  that it is less convenient.
- **The passphrase crosses the socket on every call** rather than once. The socket is a Unix
  domain socket with no network path, and anything that can read it can already reach the
  daemon — but "once" would still be fewer copies than "every time", and this trades that for
  having no window.
- **The passphrase exists in the caller's memory too**, and this ADR governs only the daemon.
  Whatever prompts for it is inside the trust boundary, and nothing here makes that safe.
- **Most of the daemon is unreachable until Phase 7** (§4), so it will be built and tested
  before it can be used, and its first real use will be long after its code was written.

## Alternatives rejected

- **`wallet.unlock` with a session and an idle timeout.** What every desktop wallet does, and
  what users expect. Buys fractions of a second on a key used a few times a day, and costs a
  window in which spendable material sits in a live process. §1.
- **Hold the derived key but not the seed**, so only one account is exposed by a compromise.
  Strictly better than caching the seed and still a window; the same argument applies with a
  smaller radius.
- **Store the account `xpub` in the clear** so the finance surface works without unlocking.
  Publishes the household's whole address graph to anyone who reads the file. §2.
- **Derive the vault passphrase from the login session**, so unlocking is invisible. Ties
  funds to a login, and makes every process running as that user a wallet holder.
- **Let `wallet.create` run unattended**, so a node can be set up without a person, and
  confirm only when funding. The wallet is created before it is funded, so the seed is
  disclosed before anything guards it. §3.
- **Confirm rather than refuse when a vault already exists.** A prompt invites a yes, and the
  yes is unrecoverable. §3.

## What is deliberately not decided

- **Whether a passphrase may be empty.** It is a real request ("I accept the disk-level risk")
  and it interacts with `FINANCE.md` §3's threat model. Not answered here.
- **The batching shape of the read API** — how many keys one call returns — beyond the
  requirement in §1 that one exist.
- **Any watch-only mode** (§2), which if it ever happens is its own ADR with its own honesty
  requirement.
- **Rate limiting repeated passphrase attempts.** Argon2id at 0.42 s is itself a limiter;
  whether that is enough is a question for whoever builds the prompt.

## References

- ADR-0022 (the wallet's keys, curve, daemon and capabilities — this fills in what it left
  open), ADR-0010 (why a high-value key does not live in a daemon that answers constantly),
  ADR-0014 (the keyless egress daemon that carries what this signs).
- `docs/services/WALLET.md` §3 (the 0.42 s measurement this reasons from), `FINANCE.md` §2a
  and §3, CLAUDE.md §8.
- `crates/otwono-permd/src/policy.rs` — the `Allow` → `Ask` conversion that §4 describes.
- `crates/otwono-permd/src/audit.rs` — `AuditRecord`, which does not carry params.

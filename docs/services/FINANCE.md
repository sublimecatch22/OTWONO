# Finance

**Status:** `SPECIFIED`. No implementation. Depends on Phase 5 (content store and
encryption at rest) and Phase 7 (agent layer). Targeted at Phase 7.

---

## 1. What it is

A financial tracker and planner that holds the household's accounts, transactions, budgets
and plans **on the household's own disk**, encrypted, and never anywhere else.

This is the subsystem where the project's prime directives stop being philosophy. "The user
owns the keys" and "local-first" are, for financial data, the entire product: every
mainstream alternative is a service that reads your bank data on someone else's computer.

## 2. The label, and it has no exceptions

**Everything here is `PRIVATE`.** Not `SHARED`, not `REPLICATED`, not cached, not indexed
into anything a peer can query.

- It never enters the neighbourhood cache. `NEIGHBOURHOOD-CACHE.md` §5 already forbids it;
  this is the case that rule exists for.
- It is not backed up to peers. A backup is an explicit, user-driven, separately encrypted
  export.
- The agent may read it only under a brokered capability, and every read is audited.

Label promotion is not merely "an explicit user action" here — the UI should treat any
attempt to promote financial data as a mistake and say so.

## 2a. A crypto wallet lives here, and §2 does not yet allow it

**Decided 2026-08-25, and this section is a placeholder for the ADR that must resolve it.**

The finance surface is to carry a crypto wallet, on **secp256k1** (Ethereum/Bitcoin/Cosmos
family), so a household can hold and spend the rewards a future contribution system pays for
running a node.

As §2 is written, that is forbidden: everything here is `PRIVATE`, never `SHARED`, and the UI
should treat promotion as a mistake. A wallet cannot work under that rule, because a wallet
must broadcast signed transactions.

The distinction §2 is missing, and which the ADR must draw precisely:

- **Keys, balances, transaction history and the contribution record stay `PRIVATE`.** No
  exception, no cache, no peer index — exactly as §2 says today.
- **A signed transaction is deliberately published.** That is egress, it requires
  confirmation, and it is audited. It is not a label promotion of stored data; it is a new
  object created for the purpose of leaving.

Also open, and pulled forward by this: **the wallet forces the encrypted backup that the
identity keystore has been deferring.** `keystore.rs` says plainly that losing `node.key`
loses the identity, with no encrypted export yet. Tolerable for a machine's name. Not
tolerable for money — a wallet without seed-phrase backup eats funds.

secp256k1 also introduces a **third curve family** to a codebase that has so far used Ed25519
for signing and X25519 for agreement. That is a new dependency and a new code path in the
most sensitive part of the system, and it is a cost the ADR should state rather than absorb.

## 3. Encryption, and why the node key is not enough

Financial records get their own key, derived from a **user passphrase**, not the node
identity key.

The reason is specific: the node key sits on the same disk as the data it would protect. An
attacker with the disk has both. A passphrase-derived key (Argon2id, per-vault salt) means
the disk alone is not enough — which is the threat that actually matters for a device in a
house.

The cost is real and must be stated: **forget the passphrase and the data is gone.** No
recovery, no reset, nobody to call. That is the same trade the node identity already makes,
and the UI must say it in those words before the vault is created, not after.

## 4. Getting data in

Three options, in increasing order of capability and of risk:

| Method | Credentials held | Verdict |
|---|---|---|
| **Manual entry** | None | Always available. The floor. |
| **File import** — OFX / QFX / QIF / CSV | None | **The first implementation.** Every bank exports these; nothing is stored that can move money |
| **Open-banking / aggregator API** | Yes, or a third party holds them | Deferred (**OQ-19**) |

**Start with file import, and it may be enough.** It requires no credentials, no third
party, no commercial relationship, and no network access at all. The user downloads a
statement and drops it in. That covers the tracking and planning the requirement actually
asks for.

### The tension in the third option, stated rather than glossed

Automatic bank sync generally means one of two things:

1. **The node holds bank credentials.** They are then on a household device, protected by
   whatever that device's security is, and a compromise is a financial compromise. Screen
   scraping additionally breaks whenever the bank changes its site.
2. **An aggregator holds them** (Plaid, TrueLayer, GoCardless and similar). This works well
   and requires registration, a commercial agreement, and — decisively — **a third party
   reading the user's complete transaction history**. That is precisely the arrangement
   this OS exists to avoid. Adopting it would contradict §1 of `CLAUDE.md`, and it should
   not be adopted quietly as a convenience feature.

Where open banking gives the *user* a direct, credential-free, revocable grant to their own
software, the objection weakens considerably. That varies by jurisdiction and is what
**OQ-19** has to settle — with the default staying "no automatic sync" until it is settled.

If any bank connection is ever built, its egress goes through `otwono-fetchd` under a named
source in the allow-list (ADR-0014) — never a daemon reaching the network on its own.

## 5. What it does

- **Track** — accounts, balances, transactions, categorisation, reconciliation against
  statements.
- **Plan** — budgets, recurring commitments, cash-flow projection, savings goals,
  amortisation.
- **Explain** — local inference over the household's own numbers: "what changed this
  month", "can we afford this", "what happens if the rate moves". This is the part that
  benefits from the AI runtime being local, because the alternative is sending a complete
  financial picture to a stranger's server.

**No advice, and the distinction is load-bearing.** Arithmetic, projection and summary are
the product. Recommending financial products, tax positions or investments is regulated
activity in most jurisdictions, and a local model is in no position to do it responsibly.
The UI must not blur the line, and the system prompt must not either.

## 6. Tier behaviour

| Tier | Capability |
|---|---|
| T0 | Tracking, budgets, projection. Arithmetic needs no model |
| T1 | Categorisation assistance, plain-language summaries |
| T2+ | Multi-scenario planning, longer-horizon narrative explanation |

A T0 node is a complete tracker. Only the explanation layer scales with the machine.

## 7. What must be true before this is called done

- A statement imports, reconciles, and the balance matches the bank's — with a fixture per
  supported format.
- The vault cannot be read with the disk alone, proven by attempting exactly that.
- No code path can place financial data in the neighbourhood cache, proven negatively.
- The agent cannot read the vault without a brokered capability, and every read appears in
  the audit log.
- A wrong passphrase fails closed and does not corrupt the vault.

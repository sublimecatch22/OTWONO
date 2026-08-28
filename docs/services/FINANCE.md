# Finance

**Status:** `SPECIFIED`. No implementation. Depends on Phase 5 (content store and
encryption at rest) and Phase 7 (agent layer). Targeted at Phase 7.

The wallet (§2a, ADR-0022) is targeted at Phase 10 and is **gated behind Phase 7 by
construction**: `wallet.sign` always confirms. ADR-0024 has since built the channel, and the
wallet is still unreachable — only a designated confirmer may answer, and the shipped image
designates nobody. The blocker is now configuration and the agent's uid, not the channel.
Its keystore, derivation, addresses and backup can be built and tested before signing is
reachable at all.

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

- It never enters the cluster cache. `CLUSTER-CACHE.md` §5 already forbids it;
  this is the case that rule exists for.
- It is not backed up to peers. A backup is an explicit, user-driven, separately encrypted
  export.
- The agent may read it only under a brokered capability, and every read is audited.

Label promotion is not merely "an explicit user action" here — the UI should treat any
attempt to promote financial data as a mistake and say so.

## 2a. The wallet, and what §2 governs

**Settled by ADR-0022.** The finance surface carries a crypto wallet on **secp256k1**
(Ethereum/Bitcoin/Cosmos family), so a household can hold what a future contribution system
pays it for running a node.

§2 above is right and unchanged. What it needed was a distinction it never had to make:

- **The visibility labels govern objects in the content store.** Under that rule everything
  here stays `PRIVATE` with no exception — keys, balances, transaction history, addresses,
  and contribution records. No cache, no peer index, no promotion.
- **A signed transaction is not a stored object being promoted.** It is a new artefact built
  for the purpose of leaving, handed to `otwono-fetchd`, and never held as a labelled object.
  There is no label on it to promote.

**The label model governs the record; the capability model governs the act.** Sending is
`wallet.sign`: irreversible, always confirmed, audited — a stronger gate than
`label.promote`, not a way around it.

Two things this section must keep saying out loud, because both are easy to leave to a UI
that will not say them:

- **A public chain is a permanent public record.** No demotion, no deletion, no expiry. This
  document already promises that kind of honesty about replicated content peers hold; a chain
  is that, forever, in front of everyone. Addresses are therefore fresh per counterparty and
  per purpose by default — reusing one makes the household's whole contribution history
  publicly linkable to anyone who sees a single payment.
- **Contribution counters are not proof of anything.** They are self-reported; ADR-0021's
  receipts make them counter-signed, which is better and still not proof. A screen implying
  the OS guarantees earnings is lying.

Keys live in `otwono-walletd` — its own daemon, in Z1, with no network at all. It signs;
`otwono-fetchd` carries. See ADR-0022 for why not `otwono-idd`, and for what adding money to
this system does to its threat model.

`docs/services/WALLET.md` tracks what is actually built. Today that is the key material only:
`crates/otwono-wallet` holds the 24-word mnemonic, BIP-32/44 derivation on secp256k1, and the
Argon2id seed vault. There is no daemon, no signing, and no address encoding yet.

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
- No code path can place financial data in the cluster cache, proven negatively.
- The agent cannot read the vault without a brokered capability, and every read appears in
  the audit log.
- A wrong passphrase fails closed and does not corrupt the vault.

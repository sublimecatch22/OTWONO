# ADR-0022 — The wallet: a third key family, its own daemon, and what `PRIVATE` means when a transaction must be published

**Status:** accepted · **Date:** 2026-08-25 · **STATUS: SPECIFIED** — no code exists yet.

## Context

The finance surface is to carry a crypto wallet, so a household can hold and spend whatever a
future contribution system pays for running a node. The curve is decided: **secp256k1**, the
Ethereum/Bitcoin/Cosmos family.

That decision reaches further than it looks, and three things have to be settled before any
code:

1. Where the key lives, and what it is derived from.
2. `docs/services/FINANCE.md` §2, which as written **forbids a wallet**: everything in
   finance is `PRIVATE`, never `SHARED`, and the UI should treat promotion as a mistake. A
   wallet must broadcast signed transactions.
3. What adding money to this system does to its threat model, which is the part nobody asks
   about until afterwards.

## Decision

### 1. It is not the node key, and not derived from it

`FINANCE.md` §3 already set the precedent — financial keys come from a user passphrase, not
the node identity — and a wallet is the strongest case for it:

- **Losing a machine must not lose money.** A node key is a machine's name; losing it costs
  the node its identity, which succession records exist to recover from. There is no
  succession record for funds.
- **One person may run several nodes and want one wallet.** Deriving from the node identity
  would give a household as many wallets as it has computers.
- **`id.rotate` changes the NodeID.** A key that must not change cannot be derived from one
  that does.
- **The node key is used constantly** — every handshake. A wallet key should be used rarely,
  and keys used rarely can be protected in ways keys used constantly cannot.

**Derivation:** BIP-39 mnemonic → BIP-32 hierarchical derivation → BIP-44 paths. Twenty-four
words, not twelve: the extra entropy is free, the words are written down once, and for a
system whose entire pitch is *the user owns the keys*, the margin is the cheapest thing in
the design.

**At rest:** the seed is encrypted with an Argon2id passphrase-derived key and a per-vault
salt, matching `FINANCE.md` §3 and `NODE-IDENTITY.md`'s planned export. The file is 0600 and
a world-readable one is **refused, not used** — the same discipline `keystore.rs` already
applies, for the same reason: a key file anyone could read is a compromised key, not a
warning.

**Implementation:** RustCrypto's `k256`. Pure Rust, so it cross-compiles to
`aarch64-unknown-linux-gnu` without a C toolchain, which CLAUDE.md §5 requires of every
crate and which the reference `libsecp256k1` bindings would complicate. `libsecp256k1` is
faster and more battle-tested, and that is the real trade — but a wallet signs a few times a
day, so speed is irrelevant here, and the signing interface is narrow enough that swapping
later is contained.

### 2. Its own daemon, `otwono-walletd`, in Z1, with no network

Not `otwono-idd`, and the reasons are the ones ADR-0010 already made once:

- **`otwono-idd` is deliberately minimal** — `SECURITY-MODEL.md` §1 calls Z1 "small enough to
  audit". Adding a wallet to it makes the daemon holding the node's name also the daemon
  holding its money, and grows the thing whose smallness is the point.
- **The blast radii differ.** Compromising `otwono-idd` costs the node its identity, which is
  bad and recoverable. Compromising a wallet costs funds, which is bad and is not.
- **`otwono-idd` is on the hot path.** Every handshake calls it. A high-value key does not
  belong in a daemon that answers constantly.

**No network, ever.** `PrivateNetwork=yes`, `RestrictAddressFamilies=AF_UNIX`. It signs;
`otwono-fetchd` carries (ADR-0014 — the egress daemon that holds no keys at all). A
compromised chain RPC endpoint cannot reach the signing key, because the signing key is in a
process with no way to reach a socket.

### 3. Three capabilities, and signing always confirms

| Action | Blast radius | `always_confirm` |
|---|---|---|
| `wallet.read` | Read | no |
| `wallet.sign` | Irreversible | **yes** |
| `wallet.export_seed` | Irreversible | **yes** |

`wallet.sign` is `Irreversible` rather than `Egress` because signing does not send — but a
signed transaction, once broadcast, cannot be recalled, and that is what irreversible means
here. Policy cannot clear the confirmation: `policy.rs` already turns `Allow` into `Ask` for
any `always_confirm` action, and that is the mechanism, not a convention.

**A consequence worth stating rather than discovering:** no confirmation channel exists yet.
`otwono-permd` returns `confirmation_required` and says so — Phase 7. So **the wallet's
signing path is unusable until Phase 7 by construction**, and that is correct rather than
inconvenient. The keystore, the derivation, the address display and the backup flow can all
be built and tested before signing is reachable at all, which is a good order to build them
in.

**Amended 2026-08-25 by ADR-0024.** The channel now exists: an `Ask` opens a pending
confirmation and a person answers on a separate socket. The paragraph above was written when
nothing could ask anybody, and its conclusion still holds for a different reason — ADR-0024 §3a
lets only a designated confirmer answer, and the shipped image designates nobody, so the
wallet's signing path remains unreachable until a confirmer is configured and the agent has
its own uid.
What changed is the name of the blocker, not the state of the wallet.

`wallet.export_seed` is brokered and audited, and the UI must require **re-entering the
passphrase**, not merely a confirmation click. It is the one action that hands over
everything at once.

### 4. `FINANCE.md` §2 resolved: the label governs the record, not the transaction

§2 is right and stays. What it needs is a distinction it never had to make.

**The visibility labels govern objects in the content store.** `PRIVATE`, `SHARED`, `PUBLIC`,
`REPLICATED` describe what may happen to something this node *holds*. Under that rule,
everything in finance is `PRIVATE` with no exception: keys, balances, transaction history,
addresses, and contribution records. No cache, no peer index, no promotion. §2 is unchanged
on all of it.

**A signed transaction is not a stored object being promoted.** It is a new artefact
constructed for the purpose of leaving, handed to `otwono-fetchd`, and not retained as a
labelled object at all. It never enters the content store, so there is no label to promote
and no rule in §2 that applies to it.

So: **the label model governs the record; the capability model governs the act.** Sending is
`wallet.sign` — irreversible, always confirmed, audited. That is a stronger gate than
`label.promote`, not a way around it.

### 5. A fresh address per purpose, and the UI says what a public chain is

Hierarchical derivation makes addresses free, so there is no excuse for reusing one.

**Default: a fresh address per counterparty and per purpose.** Reusing a single address for
every reward payment makes the household's entire contribution history — how much it
contributes, when it is online, who it transacts with — **permanently and publicly
linkable** by anyone who learns one address.

And the UI must say, before the first transaction and in plain words, what CLAUDE.md §8
already demands the system say about published content: **this cannot be recalled.** A public
chain is a permanent public record. There is no demotion, no deletion, and no expiry. The
project already tells the truth about replicated content that peers hold; a chain is that,
forever, in front of everyone.

## Consequences

**Good.** A household can hold what it earns without the OS running a ledger. The signing key
is in a daemon with no network, behind a confirmation policy cannot clear, derived from
something the node key cannot compromise. The backup story is the standard one people already
have tooling and habits for.

**Bad, and the first one is the one that matters.**

- **This changes who attacks the system.** Before a wallet, breaking into an OTWONO node got
  an attacker a household's data — which is serious, and which draws the adversaries that
  target households. After a wallet, it gets them funds, which draws adversaries that target
  *money*: automated, financially motivated, indifferent to whose machine it is, and
  operating at a scale that household-targeting attackers do not. The same node is now worth
  attacking by people who had no reason to before. Every hardening decision in this project
  should be re-read with that in mind, and this ADR is the place that says so.
- **It forces the encrypted backup the identity keystore has been deferring.**
  `keystore.rs` says plainly that losing `node.key` loses the identity and there is no
  encrypted export yet. Tolerable for a machine's name. Not tolerable for money — so this
  work happens now rather than eventually, which is a schedule cost this ADR imposes on
  another subsystem.
- **A third curve family.** The codebase uses Ed25519 for signing and X25519 for agreement;
  secp256k1 is new crypto, a new dependency, and a new code path in the most sensitive part
  of the system.
- **People lose seed phrases.** The support burden is real, the answer is "nobody can help
  you", and the UI has to say that before the wallet exists rather than after.
- **Shipping a wallet in an OS may carry regulatory weight** in some jurisdictions. Not an
  engineering decision, named here because it is the kind of thing that surfaces late.

## Alternatives rejected

- **Derive the wallet key from the node identity.** One fewer secret, one fewer backup. Ties
  funds to a machine, gives a household one wallet per computer, and breaks on `id.rotate`.
- **Put the wallet in `otwono-idd`.** No new daemon, no new unit, no new socket. Puts the
  node's name and the household's money behind one compromise, and grows the daemon whose
  smallness is its security property.
- **Let `otwono-walletd` reach the network directly**, so it can broadcast its own
  transactions. Removes an IPC hop and puts a high-value key in a process that parses remote
  input. This is exactly the trade ADR-0010 and ADR-0014 both refused.
- **`libsecp256k1` via bindings.** Faster, and the reference implementation. Adds a C
  dependency to a workspace that cross-compiles cleanly without one, for performance a wallet
  does not need.
- **Twelve-word mnemonics.** The common default. The entropy difference costs nothing and the
  words are written once.
- **Reuse one address.** Simpler UI, and it publishes the household's contribution history to
  anyone who ever sees one payment.
- **Skip the confirmation until Phase 7 ships a channel**, so the wallet is usable sooner. It
  would mean shipping spending with no human in the loop, which is the one thing a wallet must
  never do.

## What is deliberately not decided

- **Which chain.** secp256k1 narrows the family, not the choice. The signing interface should
  not care.
- **Whether the wallet holds general assets or only the reward token.**
- **Fee and gas handling**, which is chain-specific and belongs with the chain decision.
- **Hardware wallet support** (Ledger, Trezor). Eventually yes — "the user owns the keys" is
  most true when the key is in a device the OS cannot read — but it is not this ADR.

## References

- ADR-0010 (why the signing key lives alone), ADR-0014 (the keyless egress daemon this uses),
  ADR-0006 (node identity), ADR-0021 (receipts, the other half of the contribution story).
- `docs/services/FINANCE.md` §2 and §3, `docs/services/DESKTOP.md` §4,
  `docs/security/SECURITY-MODEL.md` §1, CLAUDE.md §5 and §8.
- `docs/services/WALLET.md` — what of this ADR is built, and the measured Argon2id
  parameters, which this ADR left open.

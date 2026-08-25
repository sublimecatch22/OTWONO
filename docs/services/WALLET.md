# Wallet

**Status:** `VERIFIED` on a booted node, for what can run there. `crates/otwono-wallet`
(keys) and `crates/otwono-walletd` (the daemon) exist, are tested against a real
`otwono-permd` over real sockets, and the daemon ships in the image and starts on boot with
its network isolation asserted as an observable fact
(`out/amd64-qemu-ubuntu/boot.log`: `wallet_ns=isolated wallet_status=no-wallet`).

What has **not** run on a node is everything that needs a person: no wallet has been
created, signed with, or exported, and none can be until Phase 7. Signing is `SPECIFIED`
only.

Decided in **ADR-0022** (keys, curve, daemon, capabilities) and **ADR-0023** (how the
passphrase reaches it, and what it holds). The finance surface it belongs to is
`docs/services/FINANCE.md` §2a; the desktop placement is `docs/services/DESKTOP.md` §4.

---

## 1. What exists today

`crates/otwono-wallet` — key material and a file format, no policy and no socket, so it is
testable without a control plane and without root (CLAUDE.md §2.4).

| Piece | State |
|---|---|
| 24-word BIP-39 mnemonic, generate and parse | IMPLEMENTED, checked against the published BIP-39 vector |
| BIP-32 derivation on secp256k1 (`k256`) | IMPLEMENTED, checked against the published BIP-32 vector |
| BIP-44 paths `m/44'/coin'/account'/change/index` | IMPLEMENTED |
| Passphrase-encrypted seed vault (Argon2id + XChaCha20-Poly1305) | IMPLEMENTED |
| Address *encoding* | **not built, deliberately** — see §4 |
| `otwono-walletd`: `wallet.status`, `wallet.public_keys`, `wallet.create`, `wallet.export_seed` | IMPLEMENTED, 11 integration tests against a real broker |
| The four capabilities in `otwono-permd`'s registry | IMPLEMENTED |
| A systemd unit, and a place in the image | IMPLEMENTED, and booted |
| Signing | SPECIFIED, and unreachable until Phase 7 **by construction** — see §5 |

### What the daemon can actually do today

On a booted node: **read, and nothing else.** `wallet.create`, `wallet.sign` and
`wallet.export_seed` are `always_confirm`, `policy.rs` turns `Allow` into `Ask` for those,
and no confirmation channel exists — so `otwono-permd` will not issue a token for any of
them. A policy saying `wallet.* = allow`, which is what an operator would actually write,
still gets `ask` on all three and `allow` on `wallet.read`. That is tested at three levels:
the registry, the policy evaluation, and end to end through the broker.

The consequence for anyone building on this: **a wallet cannot be created through the daemon
yet.** Tests plant a vault directly through `crates/otwono-wallet`, which is also the shape a
console-side creation flow will take when there is one.

`wallet.sign` is deliberately not implemented at all rather than implemented-and-refusing:
nothing can be signed until a chain is chosen (§4), and a method that existed but always said
no would be a worse answer than one that is honestly absent.

## 2. Why the wallet key is not the node key

ADR-0022 §1 in one line each, because this is the decision everything else follows from:

- **Losing a machine must not lose money.** A node key is a machine's name; succession
  records exist to recover from losing one. There is no succession record for funds.
- **One person may run several nodes and want one wallet.** Deriving from the node identity
  gives a household as many wallets as it has computers.
- **`id.rotate` changes the NodeID.** A key that must not change cannot come from one that does.
- **The node key is used every handshake.** A wallet key should be used rarely, and rarely
  used keys can be protected in ways constantly used ones cannot.

The recovery phrase is therefore the backup, and it is deliberately the only one this crate
offers: the standard one, that people already have tooling, habits, and metal plates for.

## 3. The vault, and the parameters it was given

The seed is encrypted with an Argon2id passphrase-derived key and a per-vault salt
(`FINANCE.md` §3). The file is 0600 from the instant it exists, and a world-readable one is
**refused, not repaired** — by the time it is observed the bytes have already been readable,
so the honest report is that the key is compromised.

**Parameters: 19 MiB, t=2, p=1** — OWASP's second listed Argon2id option, verbatim.

The memory drove the choice more than the time. A tier-T0 board may have 512 MiB with the
rest of the system already in it, and a KDF allocating 64 MiB there may fail to unlock a
wallet on the machine that created it. Choosing a profile per machine is not an alternative:
a wallet whose protection depends on which computer you open it from has a weakest link that
moves.

Measured on the amd64 development host, single-threaded:

| m | t | p | time |
|---|---|---|---|
| 64 MiB | 3 | 4 | 2.24 s |
| 64 MiB | 3 | 1 | 2.23 s |
| 64 MiB | 1 | 1 | 0.70 s |
| 32 MiB | 2 | 1 | 0.73 s |
| **19 MiB** | **2** | **1** | **0.42 s** |

Two things that measurement settled rather than assumed:

- **`p_cost` is 1 because more buys nothing.** The first two rows are the evidence: the
  `argon2` crate computes lanes sequentially, so `p = 4` costs the same wall-clock and merely
  advertises a parallelism this build does not have. Raising it needs a threaded
  implementation and a fresh measurement, not an inherited default.
- **A T0 board is several times slower than that host**, so expect low seconds there. That is
  the right price for unlocking a wallet and the wrong one for anything on a hot path — which
  is another reason nothing but the wallet uses this.

The cost parameters are written **into the file**, so raising them later is a safe act rather
than a migration: an existing vault opens at whatever it was written with. They are also
attacker-controlled if the file is, so they are bounded on read — an `m_cost` of four billion
is a denial of service against the person opening their own wallet, or an out-of-memory kill
on a small board.

A wrong passphrase and a damaged or altered file report **the same error**, because the AEAD
tag covers both and the difference is genuinely not observable here.

## 4. Why there is no address yet

ADR-0022 leaves **which chain** deliberately undecided, and an address string is
chain-specific: Ethereum hashes the public key with Keccak, Bitcoin hashes it differently and
encodes with a network prefix. Committing to one encoder now would decide the chain by
implementation, which is how a "not yet decided" quietly becomes decided.

The crate goes as far as the compressed secp256k1 public key — which every chain in this
family agrees on — and stops. Whatever encodes an address takes those 33 bytes.

## 5. Signing, and why it is not reachable

`wallet.sign` is `Irreversible` and `always_confirm` (ADR-0022 §3). `policy.rs` turns `Allow`
into `Ask` for any `always_confirm` action, and no confirmation channel exists until Phase 7,
so `otwono-permd` answers `confirmation_required`.

**The wallet's signing path is therefore unusable until Phase 7 by construction**, and that
is correct rather than inconvenient. The vault, the derivation, the public side and the
backup flow can all be built and tested before signing is reachable at all, which is the
order this is being built in.

`otwono-walletd` will run in Z1 with **no network at all** — `PrivateNetwork=yes`,
`RestrictAddressFamilies=AF_UNIX`. It signs; `otwono-fetchd` carries (ADR-0014). A
compromised chain RPC endpoint cannot reach the signing key because the signing key is in a
process with no way to reach a socket.

## 6. What the UI must say, and must not

From ADR-0022 §5 and `FINANCE.md` §2a, restated here because these are the parts a screen is
most likely to soften:

- **A public chain is a permanent public record.** No demotion, no deletion, no expiry. This
  project already tells the truth about replicated content peers hold; a chain is that,
  forever, in front of everyone.
- **A fresh address per counterparty and per purpose.** Hierarchical derivation makes
  addresses free. Reusing one makes a household's whole contribution history — how much it
  contributes, when it is online, who it transacts with — permanently and publicly linkable
  to anyone who ever sees a single payment.
- **Contribution counters are not proof.** They are self-reported; ADR-0021's receipts make
  them counter-signed, which is better and still not proof. A screen implying the OS
  guarantees earnings is lying.
- **Forget the passphrase and the vault is gone; lose the phrase too and the money is gone.**
  Nobody can help. The UI has to say that before the wallet exists rather than after.

## 7. What is not built, and what is not decided

Not built:

- Signing, and therefore any transaction.
- Address encoding (§4), balances, history, and anything that talks to a chain.
- The encrypted identity backup ADR-0022's consequences say a wallet forces
  (`keystore.rs` still has no encrypted export).

Not decided (ADR-0022's own list): which chain; whether the wallet holds general assets or
only a reward token; fee and gas handling; and hardware wallet support, which is eventually
yes — "the user owns the keys" is most true when the key is in a device the OS cannot read.

## 8. The thing worth re-reading the rest of the project for

ADR-0022 says it and it belongs here too: **a wallet changes who attacks this system.**
Before one, breaking into an OTWONO node got an attacker a household's data — serious, and it
draws the adversaries who target households. After one, it gets them funds, which draws
adversaries that target *money*: automated, financially motivated, indifferent to whose
machine it is, and operating at a scale household-targeting attackers do not. The same node
is now worth attacking by people who had no reason to before.

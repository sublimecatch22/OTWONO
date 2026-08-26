# ADR-0025 — Render every address family, and decide the chain later

**Status:** accepted · **Date:** 2026-08-26 · **STATUS: VERIFIED on a booted node**

`out/amd64-qemu-ubuntu/boot.log`: a wallet created through a confirmation renders an
Ethereum, a Bitcoin and a Cosmos address at two indices, all six distinct.

## Context

ADR-0022 chose secp256k1 and left the chain open. `crates/otwono-wallet` therefore stopped
at the compressed public key, on the reasoning that shipping one encoder would decide the
chain by implementation — "committing to one here would be deciding the chain by
implementation, which is how a 'not yet decided' quietly becomes decided."

That was too cautious, and it had a cost: `otwono-walletctl address` printed 33 bytes of
hex. A wallet that cannot show a person somewhere to receive at is not usable, so the
undecided chain was blocking something it had no need to block.

## Decision

**Render all three families from the same key.** An address is a *pure function of a public
key*, and it is the same key in every case:

| Family | Derivation | Prefix |
|---|---|---|
| Ethereum | Keccak-256 of the **uncompressed** key, last 20 bytes, EIP-55 checksum | `0x` |
| Bitcoin | SHA-256 then RIPEMD-160, bech32 as a v0 witness program | `bc1q` |
| Cosmos | the same 20 bytes, bech32 under a chain's own prefix | `otwono1` |

This commits to nothing. It defers the chain choice **genuinely**, rather than by refusing
to render anything — and when the chain is decided, the work is deleting two renderings
rather than writing one.

### The UI must say they are one wallet

Three notations invite the reading *three wallets, send anywhere*. They are one set of funds
under different spellings, and **money sent to the wrong chain's address is usually gone**.
`otwono-walletctl` says so under the addresses, and a test asserts the sentence is there —
it is load-bearing, not decoration.

### Checked against published vectors, because the first attempt was wrong

The Ethereum encoder is checked against three published key/address pairs, not against
itself. This matters more than it sounds: **the first version of that test carried a vector
written from memory, and it was wrong.** The implementation was right and the test said it
was broken.

The vectors now come from a from-scratch Keccak-256 and secp256k1 written for the purpose,
whose Keccak was itself checked against the published empty-string and `"abc"` digests
before being trusted — `hashlib.sha3_256` is NIST SHA-3, not Keccak-256, and the padding
byte differs, which is a trap worth naming. Private key 1 lands on the widely published
`0x7e5f…5bdf`.

An encoder that is self-consistent and disagrees with the chain produces addresses nobody
can spend from. That is a way to lose money that looks like working software, and the only
defence is a vector from outside the codebase.

### Two narrowings

- **Ethereum hashes the uncompressed key.** Hashing the 33 compressed bytes yields a
  well-formed address belonging to nobody. A test pins that the two disagree.
- **Bech32 prefixes must be lowercase**, which is narrower than bech32 itself — the spec
  permits an all-uppercase prefix, and uppercase addresses are used for QR codes. Every real
  chain prefix is lowercase, so an uppercase one produces a technically valid address that
  looks wrong to a person and that some tooling rejects.

## Consequences

**Good.** The wallet shows a person something they can receive at. The chain decision is
deferred without blocking anything. Adding a fourth family is a match arm.

**Bad, and worth naming.**

- **Three addresses is a way to confuse somebody**, and the warning is the only thing
  standing between a user and sending to the wrong chain. A UI that drops it is dangerous.
- **Rendering an address is not supporting a chain.** Nothing here knows about balances,
  fees, nonces, or broadcasting, and a person who sees `bc1q…` may reasonably assume Bitcoin
  is supported. It is not; only the notation is.
- **Two more dependencies** (`sha3`, `bech32`) in a security-sensitive crate, for a
  convenience. Both are pure Rust and cross-compile.
- **The Cosmos prefix defaults to `otwono`**, which looks like a decision about running our
  own chain. It is a placeholder and this sentence is the disclaimer.

## Alternatives rejected

- **Keep printing raw public keys.** What was there. Correct, uncommittal, and unusable —
  the state this ADR exists to end.
- **Pick one family now.** Simpler output and a smaller crate. It is the thing ADR-0022
  declined to do, and the reason still holds; the error was concluding that *therefore
  nothing* could be rendered.
- **Render on demand, one family per call.** Same code, more round trips, and it hides that
  they are the same key — which is the fact most worth showing.
- **Let the daemon pick a family from configuration.** Moves the choice into a file nobody
  reads and reintroduces deciding-the-chain-by-default through the back door.

## References

- ADR-0022 (secp256k1, and the "which chain" question this defers rather than answers),
  ADR-0023 (why no public key is stored in the clear, so an address needs the passphrase).
- `docs/services/WALLET.md` §4, `crates/otwono-wallet/src/address.rs`.
- EIP-55 for the checksum, BIP-173 for bech32.

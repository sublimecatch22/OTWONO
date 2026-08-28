# ADR-0006 — Ed25519 node identity, separate from user identity

**Status:** accepted · **Date:** 2026-08-22

## Context

Nodes need a persistent, verifiable, network-independent identity with no central
authority. A tempting shortcut is one key that is both "this device" and "this person".

## Decision

**Ed25519** long-term signing key per node, generated on first boot, TPM-sealed where
available. `NodeID = multihash(sha2-256, pubkey)`, with an 80-bit human-checkable
fingerprint. X25519 agreement keys are derived; the long-term key signs and never encrypts
bulk traffic.

**Node identity and user identity are separate.** A user identity may span several nodes;
the binding is a signed, expiring certificate.

## Consequences

**Good:** small, fast, well-audited primitives available on every target; identity survives
IP, network, and location changes; separating device from person makes multi-device support
and device loss tractable; no registry, no consensus, no blockchain.

**Bad:** losing the key loses the identity — mitigated with an encrypted backup offered at
first boot and stated plainly in the UI; revocation without a central authority is
best-effort propagation, which the design must not overstate; entropy at first boot on an
SBC is a genuine risk, so generation blocks on `getrandom(2)` rather than degrading.

## Alternatives rejected

- **X.509 / a CA** — reintroduces a central authority and the whole PKI operational burden.
- **Blockchain-anchored identity** — requires consensus, connectivity, and usually a token;
  contradicts offline-first.
- **One combined device+user key** — makes device loss identity loss and blocks multi-device.
- **secp256k1** — no advantage here, and worse library ergonomics for signing.

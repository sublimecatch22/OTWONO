# Threat Model

**Status:** `SPECIFIED`, first pass. Revisit before any public release.

## Assets

| Asset | Why it matters |
|---|---|
| Node private key | Impersonation of the node |
| User private key | Impersonation of the person across nodes |
| `PRIVATE` user data | The core promise of a local-first OS |
| Storage encryption key | Unlocks everything at rest |
| Release signing key | Compromise means arbitrary code on every installation |
| Audit log integrity | Without it, no incident can be reconstructed |

## Adversaries

| # | Adversary | Capability | In scope |
|---|---|---|---|
| A1 | Remote unauthenticated peer | Send arbitrary bytes to ONM ports/radio | Yes |
| A2 | Authenticated but untrusted peer | Everything A1 can, plus a valid handshake | Yes |
| A3 | Malicious content author | Crafts documents/pages/messages the agent will read | Yes |
| A4 | Compromised app in Z5 | Local unprivileged code execution | Yes |
| A5 | Local network attacker | Sniff, spoof, MITM the LAN | Yes |
| A6 | Radio-range attacker | Jam, replay, spoof radio links | Partly — jamming is out of scope |
| A7 | Malicious model | Weights crafted to induce harmful tool calls | Yes |
| A8 | Supply-chain attacker | Compromise a dependency or a mirror | Partly |
| A9 | Physical attacker, powered-off device | Steal the disk | Yes, via FDE |
| A10 | Physical attacker, running device | Evil maid, DMA, cold boot | Partly |
| A11 | Global passive observer | Traffic analysis | **No — explicitly out of scope** |
| A12 | Nation-state with kernel 0-day | — | No |

## Principal attack surfaces, in order of risk

1. **`otwono-netd` parsers (A1, A2, A5, A6).** Untrusted bytes, constantly. Mitigation:
   Rust, no `unsafe`, explicit size bounds everywhere, continuous fuzzing, tight seccomp,
   no filesystem write outside a spool, no `exec`.
2. **The agent's tool surface (A3, A7).** Text becomes action. Mitigation: provenance
   tagging, capability scoping to the originating user request, confirmation prompts that
   describe the concrete effect, no generic shell tool.
3. **Model and package supply chain (A7, A8).** Mitigation: signed manifests, hash pinning,
   snapshot-pinned mirrors, reproducible builds, reduced tool access for unverified models.
4. **The permission broker itself (A4).** Mitigation: keep it small, keep it auditable,
   test it adversarially, and treat every new action type as a security change.
5. **Key storage (A9, A10).** Mitigation: TPM sealing where available, FDE, and honest UI
   about what is and is not hardware-protected.

## Explicit non-goals

Anonymity, traffic-analysis resistance, protection from a compromised kernel or firmware,
and defence against an attacker who controls the hardware supply chain.

## Review triggers

Re-run this model when: a new daemon is added, a new link type is added, the agent gains a
new class of tool, remote inference is enabled by default, or federation is implemented.

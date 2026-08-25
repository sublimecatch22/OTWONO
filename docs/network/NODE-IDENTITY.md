# Node Identity

**Status:** partly `VERIFIED`, partly `SPECIFIED`.

* **Implemented and exercised on a booted system:** the signing and agreement keys, the
  NodeID encoding and fingerprint, the on-disk keystore, succession records, and the
  identity-bound Noise handshake. Two QEMU nodes have authenticated each other — see
  `docs/build/VERIFICATION-LOG.md`.
* **Implemented, not yet booted:** the two-daemon key split of ADR-0010, and the sharing
  key of ADR-0019 — generated at startup, vouched for, published in `node.pub` and by
  `id.sharing_binding`. Covered by integration tests over real sockets. No node has booted
  with one, and nothing yet *uses* it: there is no `id.unwrap_shared`, and `SHARED` still
  fails closed at every boundary.
* **Not implemented:** TPM/TrustZone sealing, the encrypted export, service subkeys, the
  storage key, revocation records, and the user-identity certificate. Everything below
  describing those is design, not code.

## 1. Requirements

1. Persistent across reboots, network changes, IP changes, and physical relocation.
2. Independent of MAC address, hostname, DHCP lease, and DNS.
3. Cryptographically verifiable by any peer with no central authority.
4. Backed by hardware (TPM 2.0 / ARM TrustZone) where available, software otherwise.
5. Exportable, rotatable, and revocable.
6. Human-checkable — two people must be able to compare fingerprints over a phone call.

## 2. Key material

| Key | Algorithm | Purpose | Lifetime |
|---|---|---|---|
| Node signing key | Ed25519 | Identity, signing records, succession | Long-term |
| Node agreement key | X25519 (generated independently) | Noise handshakes | Long-term, rotatable |
| Node sharing key | X25519 (generated independently) | Unwrapping `SHARED` content keys | Long-term |
| Session keys | ChaCha20-Poly1305 / AES-GCM via Noise | Channel encryption | Per connection, rekeyed |
| Service subkeys | Ed25519, signed by the node key | Per-service signing | Medium-term |
| Storage key | XChaCha20-Poly1305 | At-rest encryption of `PRIVATE`/`SHARED` | Long-term, TPM-sealed |

The long-term key **signs**; it never encrypts bulk traffic. Separating those roles limits
the damage from any single compromise.

The agreement key is *generated*, not derived from the Ed25519 seed. The birational map
would have saved a file, but deriving it would tie the two keys together and remove the
property the separation exists for: the agreement key can be replaced after a suspected
compromise without the node losing its name.

Because they are separate keys they live in separate processes (ADR-0010). `otwono-idd`
holds the signing key and nothing else opens it; `otwono-netd` holds the agreement key and
asks `otwono-idd` for the two signatures a handshake needs. Losing the agreement key costs
sessions; losing the signing key costs the node's name, permanently.

The **sharing key** (ADR-0019) is a third X25519 key, and it is deliberately not the
agreement key. Content keys are unwrapped with it, and unwrapping in `otwono-netd` would
put plaintext content keys in the process that parses input from the network — the exact
trade ADR-0010 refused. So it lives in `otwono-idd` alongside the signing key. Each key has
one job, and the cost of each being lost is different:

| Key | Lost | Consequence |
|---|---|---|
| Signing | permanent | the node's name, and every peer relationship attached to it |
| Agreement | recoverable | current sessions; the signing key vouches for a replacement |
| Sharing | permanent, but bounded | everything shared *to* this node becomes unreadable; everything it shared *by* is unaffected |

That last asymmetry is unusual enough to be worth stating twice, and `otwono-idd` says it
on the boot where the key is generated.

A peer learns where to seal by being told: the signed binding travels in the `Hello` that
follows every Noise handshake, so a node that has authenticated another knows where to seal
to it without a second exchange. It is public information — it is what `node.pub` publishes.
A `Hello` carrying **no** binding is fine and means a node nothing can be shared with; a
`Hello` carrying one that does not verify, or one that names a different NodeID than the
handshake authenticated, ends the session. Treating that as an absence would teach the mesh
daemon to ignore signed claims that do not check out, and the claim is about where somebody
else's data would go.

Like the agreement key, the sharing key is **vouched for**: `node.key` records which
sharing public key the signing key stands behind, and the signed `SharingBinding` is
published in `node.pub` and returned by `id.sharing_binding`. The binding carries its own
signature under its own domain string, so an agreement binding cannot be replayed as a
sharing one, and `id.sign` cannot be used as an oracle to forge either. A sender must
verify a binding before sealing; sealing to an unsigned field would be sealing to whichever
key somebody claimed was the recipient's.

## 3. NodeID

```
NodeID       = multihash(sha2-256, ed25519_public_key)
Text form    = "otw1" + base32-crockford(NodeID)          # full, ~55 chars
Fingerprint  = first 80 bits, grouped:  otw1:qm7f-2k9x-8v3t-rj5p  # human-checkable
```

Eighty bits of fingerprint is the deliberate trade-off between collision resistance and
something a human will actually read aloud. Full comparisons are always done on the
complete NodeID; the fingerprint is a UI affordance.

## 4. Storage

```
/var/lib/otwono/identity/
  node.key            0600  Ed25519 seed + the agreement public key it vouches for
                            (or, with a TPM, a handle reference)     — otwono-idd only
  agreement.key       0600  X25519 secret                            — otwono-netd only
  sharing.key         0600  X25519 secret                            — otwono-idd only
  node.pub            0644  the published PublicIdentity, including
                            the signed sharing binding
  succession.jsonl    0644  signed rotation records, append-only
  subkeys/                  SPECIFIED; not implemented
```

Separate private files, not one, because different daemons need different halves and
neither should hold the other's (ADR-0010). `node.key` records the agreement and sharing
*public* keys so `otwono-idd` can say what it has vouched for without holding a secret it
has no use for. `sharing.key` is its own file even though the same daemon holds it, so the
two secrets can be backed up, rotated and eventually TPM-sealed on their own schedules —
and so adding it did not change `node.key`'s schema on nodes that already had one.

Both daemons currently run as root, so the mode bits stop another *user*, not another root
process. The separation today is by process and code path; kernel enforcement waits on the
Z2/Z3 user separation and Landlock work.

With a TPM: the key is generated in the TPM and sealed to PCR state; the file holds only a
handle. **This is not implemented.** Today it is always an on-disk key, ideally on a
LUKS-encrypted root, and the stored metadata records `hardware_backed: false` so nothing
downstream can claim protection that is not there.

## 5. Node identity vs user identity

These are deliberately separate:

- **Node identity** = a device. A user may own several.
- **User identity** = a person. May span devices and may be pseudonymous.
- Binding = a certificate signed by the user key asserting "this node is mine", with an
  expiry.

Conflating device and person identity is a common and costly design error: it makes device
loss equal identity loss, and it makes multi-device support impossible to add later.

## 6. Lifecycle

**Generation** — first boot, from the kernel CSPRNG. On SBCs, entropy at first boot is a
real risk: generation must block on `getrandom(2)` without `GRND_NONBLOCK` and must not
fall back to a weaker source. The first-boot service also seeds the RNG from any hardware
RNG present.

**Backup** — offered immediately at first boot: an encrypted export (passphrase-derived key
via Argon2id), optionally split with Shamir's Secret Sharing. The UI states once, plainly:
*lose this key and you lose this identity.*

**Rotation** — a new key signed by the old one, published as a signed succession record.
Peers that know the old key accept the new one automatically; peers that do not re-verify
from scratch. Rotation drops both bindings, because the new key has vouched for nothing:
`otwono-netd` must re-bind the agreement key before the node can handshake again, and
`node.pub` is **removed** until it does rather than left naming a NodeID the node no longer
has. `otwono-idd` re-binds the sharing key itself, since it holds both halves. What happens
to content keys already wrapped to the old sharing key is **OQ-27** and is unanswered; the
sharing secret is untouched by rotation, so nothing already shared becomes unreadable.

**Revocation** — a signed revocation record, propagated as `REPLICATED` content and cached
by peers. Without a central authority, revocation is best-effort propagation, and the
design must be honest about that rather than implying instant global revocation.

**Recovery** — restore from backup, or generate a new identity and re-establish peer
relationships manually.

## 7. Trust

Trust is local and explicit. There is no global reputation system and no consensus.

| State | Meaning |
|---|---|
| `unknown` | Seen, authenticated, unnamed. Minimal access. |
| `known` | The user gave it a petname. Can exchange messages. |
| `trusted` | Explicitly trusted for named capabilities (AI, replication, gateway). |
| `blocked` | Rejected at handshake. |

Users may import signed peer lists from a `trusted` peer as *suggestions* — never as
automatic trust. Transitive trust is a suggestion, not a grant.

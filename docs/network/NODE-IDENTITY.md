# Node Identity

**Status:** `SPECIFIED`. No implementation yet.

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
| Node agreement key | X25519 (derived) | Noise handshakes | Long-term, rotatable |
| Session keys | ChaCha20-Poly1305 / AES-GCM via Noise | Channel encryption | Per connection, rekeyed |
| Service subkeys | Ed25519, signed by the node key | Per-service signing | Medium-term |
| Storage key | XChaCha20-Poly1305 | At-rest encryption of `PRIVATE`/`SHARED` | Long-term, TPM-sealed |

The long-term key **signs**; it never encrypts bulk traffic. Separating those roles limits
the damage from any single compromise.

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
  node.key            0600, root:root  (or a TPM handle reference)
  node.pub            0644
  node.meta.json      created_at, algorithm, tpm_sealed, succession chain
  subkeys/
```

With a TPM: the key is generated in the TPM and sealed to PCR state; the file holds only a
handle. Without: an on-disk key, ideally on a LUKS-encrypted root — and the UI says so
rather than implying hardware protection that is not there.

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
from scratch.

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

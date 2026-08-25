# Data Visibility and Replication Model

**Status:** `SPECIFIED`. No enforcement code exists yet.

## 1. Labels

Every object in `otwono-stored` carries exactly one visibility label.

| Label | Storage | Network | Encryption at rest | Encryption in transit |
|---|---|---|---|---|
| `PRIVATE` | Local only | Never leaves the node on its own | Yes, node storage key | n/a |
| `SHARED` | Local | Only to explicitly authorized nodes/users | Yes | Yes, per-recipient wrapped keys |
| `PUBLIC` | Local, served on request | To any peer permitted by network policy | No | Channel encryption only |
| `REPLICATED` | Local + peer replicas | Actively copied to cache/archive peers | No | Channel encryption only |

## 2. Rules

1. **Default `PRIVATE`.** Missing, unknown, or unparseable label ⇒ `PRIVATE`. Fail closed.
2. **Promotion is an explicit user act.** The agent may propose; only the user promotes.
   Promotion is logged with a before/after record.
3. **Demotion is honest.** Un-publishing stops local serving and asks peers to drop, but
   `PUBLIC`/`REPLICATED` content already copied cannot be recalled. The UI says this
   *before* promotion, not after.
4. **Provenance propagates.** Derived content inherits the most restrictive label of its
   inputs. A summary of a `PRIVATE` document is `PRIVATE`. This is the mechanism that stops
   an agent from laundering private data into a public post, and it is enforced in the
   store, not left to the agent's good judgement.
5. **Prompts are data.** Sending content to a remote inference provider is egress, and the
   labels apply.
6. **Backups are exports.** Backing up `PRIVATE` data off-node is an explicit action with
   its own confirmation, and the backup target's trust level is part of the record.

## 3. Object model

```json
{
  "schema_version": "1.0.0",
  "cid": "blake3:…",
  "label": "SHARED",
  "owner": "otw1:…",
  "created_at": "2026-01-01T00:00:00Z",
  "provenance": { "derived_from": ["blake3:…"], "tool": "libreoffice-writer" },
  "authorized": { "nodes": ["otw1:…"], "users": ["usr1:…"] },
  "replication": {
    "target_replicas": 3, "max_hops": 4, "ttl_days": 365,
    "max_size_bytes": 104857600, "allow_rereplication": true
  }
}
```

`replication` is meaningful only for `REPLICATED`. `authorized` is meaningful only for
`SHARED`.

## 4. Enforcement points

| Point | Enforces |
|---|---|
| `otwono-stored` write path | Label assignment, provenance propagation, encryption at rest |
| `otwono-stored` read path | Caller authorization before returning bytes |
| `otwono-netd` egress | Label check on **every** outbound object. `PRIVATE` is dropped and the attempt is logged as an incident. **STATUS: IMPLEMENTED** — `otwono_netd::content::may_leave_a_node`. |
| `otwono-svcd` | Services cannot publish what the label forbids |
| `otwono-permd` | Confirmation for promotion and for egress of `SHARED` |

Egress enforcement is deliberately duplicated in the store and the network daemon.
Defence in depth: a bug in one must not be sufficient to leak private data.

The duplication is only worth having if the two checks are *different code*.
`otwono-stored` asks `Visibility::may_leave_the_node_unattended()`, an enum method over a
parsed label. `otwono-netd` does not call it: it holds an allow-list of the two strings that
may appear on a wire and refuses everything else, including labels that do not exist yet. A
shared helper would have been tidier and would have duplicated nothing — one bug would pass
both gates. See ADR-0017.

`otwono-netd` also holds `store.serve` and no other store capability, so there is no call it
can make that returns a `PRIVATE` object even if both checks failed at once.

## 5. Encryption

- **Everything, whatever its label**: XChaCha20-Poly1305 with the node storage key.
  Implementation note, and a deliberate strengthening of what this section originally said:
  a chunk is content-addressed and label-agnostic, so the *same* chunk can be referenced by
  a `PRIVATE` object and a `PUBLIC` one at once. Encryption keyed on the label would have to
  answer "which object referenced this chunk first?", and every answer to that is a bug. So
  the label governs who may read an object; it does not govern whether the bytes on disk are
  encrypted. Chunk digests are over **plaintext**, so two nodes with different storage keys
  still agree on what a chunk is called — without that the neighbourhood cache could not
  exist. TPM sealing where available remains unimplemented.
- `SHARED`: a per-object content key, wrapped once per authorized recipient with an X25519
  key agreement. Adding a recipient re-wraps the key; **removing one does not un-share what
  they already have**, and the UI says so.

  **STATUS: partly VERIFIED** — ADR-0019 settled the three questions this sentence leaves
  open, and a booted node has now sealed an object to itself, kept the plaintext out of the
  chunk store, and opened it again (`out/amd64-qemu-ubuntu/boot.log`). What has *not* booted
  is the serving half: no `SHARED` object has passed between two machines. The object is encrypted *before* chunking,
  so its chunks and its `ContentId` are over ciphertext: chunking the plaintext and
  encrypting each chunk would let a holder confirm a guessed plaintext against a digest, and
  would give a `SHARED` object the same id as the same file stored `PUBLIC`. The wrap uses a
  fresh ephemeral sender key, so the sharing node needs no long-term secret. And the
  unwrapping key is a **third** node key held by `otwono-idd` rather than the agreement key
  in `otwono-netd`, because ADR-0010 keeps the network-facing daemon holding only what Noise
  needs.

  Two consequences the UI must not hide. A `SHARED` object does not deduplicate and is not
  cacheable, so a household sharing one large video with three neighbours stores it four
  times. And the recipient list is itself a social graph living in the object record — the
  wire sends each recipient only their own copy of the key, but a node holding a `SHARED`
  object it cannot read can still read who can (**OQ-28**, unsolved).

  What is still missing: adding and removing recipients after the fact (ADR-0019 §5), and
  any variant of the share and accept calls for objects past the control plane's inline cap.
- `PUBLIC`/`REPLICATED`: unencrypted but signed, so peers verify authenticity and detect
  tampering.

## 6. Testing

Negative tests are the point of this subsystem:

- A `PRIVATE` object must never appear on any link, under any code path, including error
  paths, debug logs, crash dumps, and replication.
- An unauthorized peer requesting a `SHARED` object gets a refusal indistinguishable from
  "not found" — an authorization error would leak the object's existence.
- Derived content inherits the most restrictive input label, verified by a property test
  across random label combinations.
- Label demotion stops future serving.

### Where each of those stands

**STATUS: VERIFIED** for the first, third and fourth; **IMPLEMENTED**, not booted, for the
second.

| Property | Where it is proven |
|---|---|
| A `PRIVATE` object never appears on any link | **On an actual link between two booted nodes.** `build/files/otwono-mesh-content-check` under `build/qemu/two-node-test.sh`: two VMs on a segment with no DHCP, mutually authenticated over Noise XX, each refusing the other a `PRIVATE` object it demonstrably holds. Also host-side in `tests/control-plane/tests/content_over_a_link.rs` |
| A refusal is indistinguishable from not-found | **On an actual link**, byte-identical replies for refused and absent; and host-side in the same file |
| Derived content inherits the most restrictive label | `tests/control-plane/tests/store_labels.rs` |
| Demotion stops future serving | proven over a link, not only at the method |
| A `SHARED` object reaches only the peers named in it | `tests/control-plane/tests/content_over_a_link.rs`, host-side over TCP and Noise. Not yet between booted nodes |

The `SHARED` case is now proven, but on a host and not on a machine.
`tests/control-plane/tests/content_over_a_link.rs` shares an object with one node, fetches
it from another over a real TCP link under Noise, opens it back to the original bytes, and
asserts that a peer *not* named in the envelope gets a refusal byte-identical to the one an
absent object gets. Every link in that test is a socket in one process tree. Until it has
run between two booted VMs — as the `PRIVATE` case has — this row is `IMPLEMENTED` and not
`VERIFIED`, and the difference is not a formality: defects 38, 43 and 44 on this branch were
all things that passed on a host and failed on a machine.

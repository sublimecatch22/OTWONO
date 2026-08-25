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

  **STATUS: VERIFIED** — ADR-0019 settled the three questions this sentence leaves open, and
  ADR-0020 settled the one it did not anticipate: a recipient could not *discover* what had
  been shared with it, because a `SHARED` object's id is over ciphertext and cannot be
  derived from the content. Two booted nodes now complete the whole loop — each seals to the
  other, each asks what it has been sent, and each fetches and opens it, with no id passing
  between the machines by any other route
  (`out/amd64-qemu-ubuntu/two-node/node-{a,b}.log`). The object is encrypted *before* chunking,
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

  Adding and removing recipients after the fact (ADR-0019 §5) is now built — and carries
  its own honesty requirement, which is where this section's second sentence stops being a
  design note and starts being a UI obligation. `store.remove_recipients` deletes the named
  recipients' wrapped copies of the content key and **nothing else**. It stops future
  serving and future discovery, exactly as `demote` does; it does not reach a recipient who
  already fetched, and the reply says so in words. Removing every recipient is refused
  rather than performed, because after ADR-0019 §5a the owner is on that list. Genuinely
  revoking access means re-encrypting under a new content key and re-sharing.

  What is still missing: any variant of the share and accept calls for objects past the
  control plane's inline cap.
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

**STATUS: VERIFIED** for all four.

| Property | Where it is proven |
|---|---|
| A `PRIVATE` object never appears on any link | **On an actual link between two booted nodes.** `build/files/otwono-mesh-content-check` under `build/qemu/two-node-test.sh`: two VMs on a segment with no DHCP, mutually authenticated over Noise XX, each refusing the other a `PRIVATE` object it demonstrably holds. Also host-side in `tests/control-plane/tests/content_over_a_link.rs` |
| A refusal is indistinguishable from not-found | **On an actual link**, byte-identical replies for refused and absent; and host-side in the same file |
| Derived content inherits the most restrictive label | `tests/control-plane/tests/store_labels.rs` |
| Demotion stops future serving | proven over a link, not only at the method |
| A `SHARED` object reaches only the peers named in it | **Between two booted nodes**, each sealing to the other and fetching what it discovered; and host-side for the refusals, in `tests/control-plane/tests/content_over_a_link.rs` |
| Asking what was shared with you reveals nothing else | A stranger, a peer with nothing, and a refusal all give the same empty page (ADR-0020). Host-side; the schema also refuses a request naming who is asking |

The `SHARED` case is proven on machines as well as on a host. Two booted VMs each seal an
object to the other, each discovers what it was sent, and each fetches and opens it —
nothing passes an id between them by any other route. `content_over_a_link.rs` covers what
the boot run does not reach: a peer *not* named in the envelope gets a refusal
byte-identical to the one an absent object gets, and a stranger asking what has been sealed
to it gets the same empty answer a node that shares with nobody gives.

What the machines have not yet exercised: paging an index or a manifest across several
windows, and any object larger than the control plane's inline cap.

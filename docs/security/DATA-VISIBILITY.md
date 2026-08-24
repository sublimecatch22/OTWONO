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
| `otwono-netd` egress | Label check on **every** outbound object. `PRIVATE` is dropped and the attempt is logged as an incident. |
| `otwono-svcd` | Services cannot publish what the label forbids |
| `otwono-permd` | Confirmation for promotion and for egress of `SHARED` |

Egress enforcement is deliberately duplicated in the store and the network daemon.
Defence in depth: a bug in one must not be sufficient to leak private data.

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

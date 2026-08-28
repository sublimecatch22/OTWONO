# ADR-0030 — Asking whether an object is already here is a writer's question, not a reader's

**Status:** accepted
**Date:** 2026-08-28
**Phase:** 6 (first distributed services)

## Context

`otwono-netd` collects mail addressed to this node on a timer (ADR-0028 §9, `CARRIAGE.md`
§3a). At the time of this decision a carrier kept an envelope until it expired **even after
handing it over** — drop on delivery was named in ADR-0028 §7 and not built — so a sweep that
did not check what it already had would re-download the same message every thirty seconds for
as long as the sender's expiry allowed.

Drop on delivery was built afterwards, and it does not retire this. The release is best
effort: a carrier that refuses it, answers something else, or never hears it keeps the
envelope to its deadline and keeps offering it. `store.holds` is what stops the sweep
re-fetching in exactly that case, and it is the only thing that does. Nothing would be corrupted; `store.accept_shared` is
content-addressed and idempotent. It would simply cost bandwidth for ever, and it would
amplify: one envelope with a week-long expiry is twenty thousand fetches.

So the sweep needs to ask its own store one question: *do I already have this?*

The obvious method is `store.stat`, which is guarded by `store.read`.

## The problem with the obvious answer

`otwono-netd` is the **Z3 hostile-input daemon** — the process that parses whatever arrives
from the network (`docs/security/SECURITY-MODEL.md`). It does not hold `store.read`, and that
is deliberate and tested: `the_serving_node_serves_without_ever_holding_store_read` runs the
whole serving path under a policy that denies it, precisely so that a mistake here is a test
failure rather than a quiet widening.

Giving that daemon `store.read` so it can skip a redundant download would mean **a compromise
of the mesh daemon reads the user's entire store**. That is a large amount of authority bought
to save a small amount of bandwidth, and it would be bought silently: nothing about "collect
mail on a timer" announces that it costs the store's confidentiality.

## Decision

**`store.holds` answers presence for one named object, and is guarded by `store.write`.**

```
store.holds { content_id } -> { schema_version, holds: bool }
```

Three properties, each load-bearing:

1. **The caller must already name an exact content id.** It is not a search and not a listing.
   Anyone who can ask has, by construction, already learned the id somewhere else — for the
   collection sweep, from a carrier's scoped index, which only names mail addressed to the
   asker.
2. **The reply is a bool and nothing else.** No size, no label, no chunk list, no timestamps.
   A caller that wants any of those is asking to read the object and can go and hold
   `store.read`. A test asserts the reply carries exactly a schema version and a bool, so
   growing it is a deliberate act.
3. **It answers `false` for an incomplete object.** A half-transferred object is one this node
   can neither serve nor open, so `true` would strand the very fetch that would finish it.

The capability is `store.write` because that is the authority the question is *for*: a caller
that may write objects may ask whether a write would be redundant. Reading a name it already
holds is strictly less than writing under that name, which it is already permitted to do.

## Why this is not just `store.read` with extra steps

The distinction is the difference between a **presence oracle for a known name** and a
**read**. With `store.holds` a compromised `otwono-netd` learns whether the node holds objects
whose ids it can already name. It cannot enumerate, cannot read a byte, and cannot discover
the id of anything it was not already told about.

The residual leak is real and worth stating: a caller holding `store.write` and a list of
candidate content ids can test that list against the store. For `otwono-netd` the list it can
build is the mail addressed to this node, which it is about to download anyway.

## Alternatives rejected

- **Give `otwono-netd` `store.read`.** Rejected above. It is the large-authority answer to a
  small problem, and it would silently delete a tested security property.
- **Remember it in the daemon's memory.** A `HashSet` of collected ids needs no capability at
  all, and is wrong twice: it is lost on restart, so a restarted node re-downloads everything
  still held for it, and it grows without bound. It is also exactly the shape of defect this
  subsystem has produced repeatedly — state that is only ever written and never reconciled
  with the thing it claims to describe.
- **Implement drop on delivery instead.** This is the *right* long-term answer and it does not
  replace this one. A carrier that dropped an envelope after serving it would leave nothing to
  re-collect, but it needs either a third wire method or per-envelope chunk-serving state
  (ADR-0028 §7), and a carrier is not obliged to be honest about it. A recipient that knows
  what it already has does not depend on any carrier's good behaviour, which is the property
  worth having whichever way §7 is eventually built.
- **Let the sweep re-download.** Correct, and the cheapest thing to write. Rejected on
  amplification: the cost is paid every sweep, for ever, per undelivered envelope, and it is
  paid by the carrier as much as by the recipient.

## Consequences

- `otwono-netd` still does not hold `store.read`, and the test that says so still passes.
- A node's collection sweep is idempotent: a second pass over the same carrier takes nothing.
- Any future caller wanting "is this here?" has a method to use that does not require
  widening its authority to a read.
- `store.holds` is a new presence oracle in the store's surface. It is narrow, but it is not
  nothing, and a future capability review should treat `store.write` as implying it.

## References

ADR-0028 (addressed messages and store-and-forward, §7 and §9), ADR-0019 (sealed objects),
`docs/network/CARRIAGE.md` §3a, `docs/security/SECURITY-MODEL.md` (trust zones).

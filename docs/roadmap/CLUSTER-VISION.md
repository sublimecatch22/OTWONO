# The cluster vision

**Status:** `SPECIFIED` — **nothing here is built, and most of it is not yet decided.** This
document exists so the direction is written down rather than remembered, and so later work
has somewhere to attach. Do not read any section as settled; each one that survives becomes
an ADR first.

Recorded 2026-08-25 from the project owner. The wording is theirs; the open questions and
concerns are the engineering reading of it, kept in the same document deliberately — a
vision doc that records only the vision is how a project talks itself into the hard parts.

---

## 1. Clusters, and clusters of clusters

A **cluster** is two or more nodes that have agreed to work together to serve data. Forming
one is itself a contribution: a cluster is meant to *stay up*, so membership requires
passing a benchmark and having a power/battery system behind it.

Three scales, each built from the one below: individual nodes → clusters of nodes →
clusters of clusters.

When somebody near a cluster asks for something — the example given is finding a video
through the OTWONO search — the cluster should spread that content across as many stable
nodes as it can and deliver it in parallel, so the request is served nearly instantly.

**What already exists:** fan-out fetch across peers holding disjoint pieces is built and
tested (`tests/control-plane/tests/fan_out_fetch.rs`), and content-defined chunking makes
objects naturally splittable. The *parallel delivery* half is largely done.

**What does not:** any notion of cluster membership, any ranking of peers, and any push of
content ahead of demand.

**Open — what makes a cluster?** Same link-layer segment, a latency band, an explicit join,
or a signed agreement between members. This is the first ADR and everything else depends on
it.

**Open — the battery requirement may be narrower than intended.** A laptop and a phone are
already battery-backed nodes. If the requirement reads as "a UPS", clusters will be rare; if
it reads as "survives a power cut", they are common.

## 2. Priority tiers, benchmarks, and paid access

Every node is used according to how its owner set it up. A node may opt to be a **priority
node** if it passes a benchmark, which also gives the owner a safe range to choose within.
Higher levels get more work. Tiers run from the smallest contributor up to a **data-centre
level**.

Selection works **bottom-up**: the system checks whether a lower tier can serve the request
before reaching for a higher one.

**There is always a free tier with access to everything.** What is bought is *priority* —
faster, higher-tier nodes. A business paying for higher priority receives data-centre-grade
service from the cluster.

**Open — self-reported benchmarks are forgeable.** A node claiming to be huge gets chosen
for everything. See §6.

**Open — who is paid.** Selling priority access to volunteers' hardware means the
volunteers must actually be paid. That is a payments obligation rather than a token
distribution, and it is a heavier legal and operational shape than "rewards for
contributing". It may push the chain decision (ADR-0022) sooner than planned.

## 3. Contribution sliders

Contribution is set by the owner across five axes: **CPU, GPU, RAM, uptime, and disk**.
These are not decoration over a fixed policy — they set real limits, and the cluster's
scheduling priorities adjust to them automatically.

**Open — publishing a node's resource profile is a privacy leak.** Resources plus uptime is
a fairly precise description of a household: when they are home, what they own, when they
sleep. CLAUDE.md §8 says telemetry is never sent by default, and a cluster that must know
each member's budget is in tension with that.

## 4. Reach: from a low-power antenna to orbit

Node access points across the planet behave like low-power satellites and ground stations,
spanning:

- **a low-power antenna** — very basic functions only,
- **through to actual satellites** relaying node data,
- with an eventual possibility of contracting bandwidth from a provider such as Starlink to
  give the node network a global service.

**Worth separating, because the cost differs by orders of magnitude:**

- **Satellite as an IP link (Starlink) is nearly free.** It is IP with ~25–50 ms added.
  The existing protocol runs over it essentially unchanged.
- **Low-power radio is where the design strains.** A LoRa-class link is roughly 1–5 kbps, so
  a 300 KiB object takes 8–40 minutes. The Noise handshake needs 1.5 round trips before any
  content moves, and ADR-0017's ranged chunk fetch is round-trip bound. Neither breaks, but
  "very basic functions" is exactly right: text and small structured data, never video.
  Reticulum is already in CLAUDE.md §2.3's integrate list for this.
- **True delay-tolerant networking** — store-and-forward with no end-to-end path — is a
  different protocol family (Bundle Protocol), not an extension of this one. If orbital
  relays with intermittent contact are in scope, that is a second network stack.

## 5. A node that can rebuild a society

A single node should be able to copy the OS, produce an install image, bring up other
devices that already have broadcast and receive hardware, and form a cluster — so that a
network can be started almost anywhere.

Every copy of the OS carries **a bundle of open knowledge**: manuals, books, tutorials,
Wikipedia and similar, plus an AI assistant that can help a person use it. This is the seed
of the free node internet, and it should be the fastest thing in the system to search and
serve.

**This is the highest-value, lowest-risk item in this document.** It is
[Kiwix](https://kiwix.org) — mature, open source, exactly this problem, and the right shape
for CLAUDE.md §2.3. A curated Wikipedia subset is a few GB.

**Open — image size against tier.** A T0 board with an 8 GB card cannot hold a 15 GB
corpus. The bundle has to be tier-aware, which the capability engine already knows how to
express.

**Constraint, not open:** every installation must be **human-initiated, on hardware the
person controls**. A node that emits an installer somebody writes to a USB stick is a tool.
Software that installs itself onto nearby devices is a worm, and will be classified as one
by every antivirus vendor regardless of intent.

## 6. Contributing by uploading, and dealing with bad actors

Uploading data to a cluster is a form of contribution. Uploads land in a security stage
where an AI checks for malicious content, and are published if safe.

The owner's intent for enforcement: a node that uploads a virus is banned and locked, must
be reinstalled, and loses what was on it. A fresh installation wipes any previous state and
starts trusted. Lesser offences accrue marks. Threat levels escalate to matching responses,
with the possibility of contacting law enforcement for actual crimes — with the explicit
caveat that **free speech matters and this must be worked through carefully; threatening
harm is different from speech.**

**This section is recorded as intent, and three parts of it do not survive engineering
review as stated.** They are written here so the design work starts from the real
constraints:

- **A node cannot be banned on hardware its owner controls.** With an open-source OS and the
  user holding root, "corrupt status" is a flag they delete and "must reinstall" is a thing
  they simply do not do. Enforcement is impossible by construction. What *does* work is
  **reputation**: other nodes decline to peer. It lives on other people's machines, needs no
  central authority, cannot be forged by the offender, and degrades gracefully. Same intent,
  achievable.
- **AI classification is not a security boundary.** False positives and false negatives are
  both certain. Attaching an irreversible penalty — permanent lockout, total data loss, no
  appeal — to an unreliable automated judgement means innocent people are destroyed by it.
  If scanning stays, the penalty must be proportionate and reversible, with a human in the
  loop for anything terminal.
- **Automated law-enforcement referral should not be built.** Which jurisdiction, for a
  global network? A false positive costs somebody a police visit rather than a node. Humans
  reporting to humans is the mechanism that exists and works.

## 7. The uncensored tier

An age-verified, uncensored space: free speech taken further than a centralised platform can
offer, on a decentralised network where content cannot be stopped, behind an age limit and
terms of service explaining the framework completely.

**This is the highest-risk element of the project, and the risk is not technical.**

- Age verification requires identity data, which cuts against the privacy premise the rest
  of the system is built on.
- "No one could stop it" means the operator cannot comply with a lawful takedown — including
  for content whose distribution carries operator liability in most jurisdictions regardless
  of decentralisation.

**This needs legal counsel before it needs an ADR**, and nothing in it should be built until
that has happened. Everything else in this document can proceed without it.

## 8. The interaction nobody should discover late

**Ranking plus rewards is a centralisation gradient.**

Today every peer is interchangeable, and that is a security property rather than an
accident: every chunk is verified against its digest at the receiving end, so a hostile peer
can waste bandwidth and cannot corrupt data. It is why fan-out is safe.

Ranking peers by capability changes two things at once:

1. **A surveillance position.** A node that advertises itself as the most powerful and
   stable gets selected for everything, and thereby observes what a household fetches. It
   still cannot corrupt anything. It learns everything — which is precisely what ADR-0015
   and ADR-0020 were written to prevent.
2. **Concentration.** If contribution earns rewards and the largest nodes are chosen most,
   rewards concentrate on the largest nodes. **A reward system that pays the biggest
   participants most is a reward system that funds a data centre** — the opposite of the
   premise.

Mitigations exist — measure rather than trust, randomise among candidates that are good
enough, cap any single peer's share, weight rewards sub-linearly in capacity — but they are
design work, not parameters, and they are much cheaper to get right before anybody has
bought hardware on the strength of the reward curve.

---

## What happens next

Nothing here is scheduled. When a piece of it is picked up it starts as an ADR, and this
document should be edited down as sections graduate into decisions — a vision doc that only
ever grows is one nobody reads.

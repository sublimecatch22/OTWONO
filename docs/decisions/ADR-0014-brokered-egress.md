# ADR-0014 — One brokered fetch daemon for outbound HTTPS, and the mesh keeps its own transport

**Status:** accepted · **Date:** 2026-08-23

## Context

Three subsystems need bytes that are not on this disk, and none of them can go and get
them:

- `ai.models.pull` — `otwono-aid` runs `PrivateNetwork=yes` with
  `RestrictAddressFamilies=AF_UNIX`, and should keep both. It is the daemon that must keep
  answering when other things break, and a child process inherits its namespace, so the
  fetcher cannot simply be spawned the way a backend adapter is.
- Update bundles (Phase 8) — `otwono-updated` is **Z1**, and the trust-zone table says Z1
  has *no network*. Giving the updater a socket would put a TLS client in the same zone as
  the node's Ed25519 signing key.
- ONM content — `otwono-netd` already has the network, but over its own mesh transport.

So the question in OQ-13 is not whether to add a network-capable component but where it
lives and how many of it there are. Getting this wrong is expensive in a specific way: an
outbound fetcher is an **exfiltration channel wearing an ingress costume**. Whoever composes
the request decides what leaves the node, whatever the response is used for.

## Decision

**One new daemon, `otwono-fetchd`, is the only component in OTWONO that makes outbound
client connections to hosts outside the mesh. The mesh keeps its own transport.**

That boundary is drawn by protocol, not by subsystem. `otwono-fetchd` does client HTTPS to
named external hosts. `otwono-netd` continues to fetch ONM content from peers over the
authenticated transport it already owns — routing peer content through an HTTPS client
would be a second implementation of something that already works, not a consolidation.

### The shape of a request

Callers do **not** pass a URL. They name a **source** — an entry in the allow-list — and a
path suffix under that source's configured prefix. `otwono-fetchd` composes the URL itself.
Consequently a caller cannot choose the host, the scheme, the port, the headers, the
request body, or the query string.

- **The allow-list is policy, not code.** Changing it is `policy.write` —
  `Irreversible`, `always_confirm`. Adding a place this node may talk to is a decision a
  human makes once, deliberately and audibly.
- **Redirects never leave the source.** A `3xx` to a host or prefix outside the entry is a
  denied fetch, not a followed one.
- **The response goes to a spool, never to a subsystem's store.** `otwono-fetchd` streams
  to `/var/lib/otwono/fetch/`, enforces a size cap and a timeout, and returns a path. The
  caller verifies and installs.
- **`otwono-fetchd`'s digest is a convenience, never a credential.** `otwono-aid` re-hashes
  the file with the code it already has. The fetcher is on the far side of the trust
  boundary from the thing that decides what is trustworthy, and must stay there.
- **It holds no keys.** It is the only network-facing component in the system with nothing
  to steal.

`ai.models.pull` therefore becomes `otwono-fetchd` fetches, then the existing, tested
`ai.models.install` verifies and installs. **The pull adds no new trust code.**

### `net.fetch` is a new action, and it does not always confirm

`net.egress` ("Send data off this node") is `BlastRadius::Egress` with
`always_confirm: true`. `net.fetch` is registered separately: `Egress` blast radius —
because bytes do leave — but `always_confirm: false`.

This is the same move ADR-0010 made for signing, and the registry's own test says why:
"Separate actions so policy can grant the mesh what it needs without granting a general
signing oracle." A general egress oracle needs a person every time. A fetch from a source
the user already approved does not, and requiring one would make unattended update
downloads impossible on exactly the headless node this OS is for.

**The confirmation moves rather than disappearing.** It happens when the source is added to
the allow-list. `docs/security/SECURITY-MODEL.md` §2 lists "sending data off-node" under
mandatory confirmation; that list is written about exporting the user's data, and this ADR
amends it explicitly rather than quietly reinterpreting it.

### Hardening

`otwono-fetchd` is **Z3** — a hostile-input boundary, since TLS records and HTTP responses
from a remote host are attacker-controlled bytes — but a *separate* Z3 process from
`otwono-netd`, so that a compromise of one does not yield the other's keys.

It gets `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6` — no `AF_NETLINK`, unlike
`otwono-netd`, because it never enumerates interfaces — plus the standard hardening set,
and:

```
IPAddressDeny=localhost link-local multicast
```

which contains the obvious SSRF: a fetch aimed at `127.0.0.1`, at the node's own control
plane, or at a printer on the LAN. Verified as a valid directive on systemd 255 with
`systemd-analyze verify` (the same tool rejects `IPAddressDenyy` and an invalid token, so
its silence here means something).

### The client

`ureq`, with `rustls`. Measured on 2026-08-23: **28 crates and no async runtime**, against
**85 crates plus tokio and hyper** for `reqwest` with `default-features = false` and
`rustls,blocking`. This workspace is entirely blocking `std` today; `reqwest`'s blocking
mode runs a tokio runtime inside it, which would make an event loop the price of
downloading a file. `ca-certificates` is already in every recipe.

## Consequences

**Good.** One place decides what this node may contact, which is the same principle
CLAUDE.md §2.6 applies to capability derivation — three subsystems with three allow-lists
is three chances to be wrong and one guarantee of drift. `otwono-aid` keeps
`PrivateNetwork=yes`; `otwono-updated` stays in Z1 with no socket; the model-trust code
stays exactly as tested, with no network anywhere near it. Every fetch is one brokered,
audited action with a named source, so "what did this node talk to" is a query against the
audit log rather than an exercise in reading three daemons. And a component with no keys is
a cheap thing to lose.

**Bad, and worth naming.**

- **The path suffix is a covert channel and we are not closing it.** A caller that wants to
  leak can encode bytes into the path it requests from an approved host, and DNS resolution
  is a second channel underneath that one. Bounded and logged is the honest claim; not
  possible is not.
- **`IPAddressDeny` is best-effort.** It is a cgroup BPF filter, and on a kernel without
  `CONFIG_CGROUP_BPF` systemd logs a failure and runs the unit anyway. It is a mitigation,
  not a boundary.
- **A fourth daemon.** More units, more sockets, more startup ordering, on a board where
  every resident process costs memory that inference wants.
- **The named-source model is less flexible than a URL, deliberately.** A model host that
  needs a query string, a signed URL, or an auth header does not work through this
  interface as specified, and making it work is a policy schema change rather than a
  caller's choice. That is the point, and it will be inconvenient at least once.
- **Resumption is required, not optional.** The target is a 4 GB model over a link that
  drops, so `Range` support and a resumable spool entry are part of the first
  implementation, not a later refinement.
- **CI in this environment must go through the allow-listed proxy.** Any integration test
  of `otwono-fetchd` here honours `HTTPS_PROXY`; that is an environment constraint on the
  tests, not a product requirement, and the proxy must not become an assumed part of the
  design.

**Nothing is built** at the time this ADR was written. **STATUS: SPECIFIED** then;
implemented since — see the amendment below.

## Amendment, 2026-08-24: the allow-list is its own directory

Implementation moved the allow-list from `/etc/otwono/policy.d/` to
**`/etc/otwono/fetch.d/`**. The intent above is unchanged — it is still policy, and writing
it is still `policy.write` — but sharing a directory with `otwono-permd` is a footgun. The
two loaders have two different schemas, and `permd`'s `PolicyFile` deserializes a file full
of `[[source]]` tables as *zero rules* without complaint. A typo in a source entry would
then read as a valid empty policy rather than an error, in the one directory where silent
acceptance is least affordable. Separate directories, separate loaders, and each refuses
what it does not understand.

Two further details settled by building it, both narrower than the decision above:

- **The `net.fetch` token's resource is the source id**, so policy can grant one caller the
  model host and not the update host. Without it every grant would silently be "anywhere in
  the allow-list", which is coarser than the allow-list itself already expresses.
- **`fetch.get` is bounded and resumable rather than blocking to completion.** The control
  plane's client sets a read timeout and a 4 GB model does not fit inside one; a caller
  loops. This is how the ADR's "resumption is required, not optional" is met, and it makes
  resumption the ordinary path rather than an error path.

Two claims in the decision above were **wrong**, and pointing the daemon at a real host is
what showed it:

- **"`ca-certificates` is already in every recipe"** was true and irrelevant. `ureq`
  defaults to the Mozilla roots compiled into the binary, so the image's trust store was
  not what the fetcher consulted — a node could not have fetched from a mirror behind a
  private CA, and nothing in the image would have explained why. The daemon now uses the
  platform verifier and reads `/etc/ssl/certs`. Three extra crates (31 against 28).
- **Nothing in the decision considered content encoding**, and `gzip` is on by default in
  `ureq`. A decompressed body is not the bytes a server's `Range` addresses, so every
  resumed fetch of a compressed response would have asked for the wrong offset — and on a
  server that accepted it, silently assembled a corrupt file that only the caller's digest
  would have caught. Automatic decoding is off.

## Alternatives rejected

- **Give `otwono-netd` an HTTPS client.** It already has the network, so this looks like
  the frugal answer. It is the opposite: `otwono-netd` is the hostile-input boundary that
  holds the X25519 agreement key, and it is supposed to be the smallest, most boring
  process in the system. Adding a TLS client, redirect handling, and URL composition to it
  widens exactly the component that must not widen, and makes a compromise cost the
  agreement key as well as the download.
- **One fetcher per subsystem.** Each subsystem knows best what it needs, and no new
  daemon. But it puts the allow-list in three places, the sandbox in three unit files, and
  the audit format in three implementations — and the answer to "what may this node
  contact" stops being answerable in one place.
- **A transient `systemd-run` unit per fetch.** Genuinely appealing: the network-capable
  process exists only while a fetch is running, and systemd applies the sandbox. Rejected
  because something must call `systemd-run`, and the privilege to create transient units is
  broader than the privilege to fetch a file — we would be holding a worse capability
  permanently in order to hold a better one briefly.
- **Let `otwono-aid` drop `PrivateNetwork=yes` for the duration of a pull.** Not a thing a
  process can do to itself, and if it were, the daemon that answers when other things break
  would be the daemon parsing TLS from a stranger.

## References

- Resolves **OQ-13**.
- Implemented in `crates/otwono-fetch` and `crates/otwono-fetchd`; see
  `docs/network/EGRESS.md` and `schemas/egress-source.schema.json`.
- ADR-0003 (control plane), ADR-0010 (splitting a broad capability into a narrow one),
  ADR-0011 and ADR-0012 (the adapter and its confinement, which this keeps intact).
- `docs/ai/AI-RUNTIME.md` §5.1, which stated the problem and deferred it here.
- `docs/security/SECURITY-MODEL.md` §1 (zone table) and §2 (confirmation list), both
  amended by this decision.

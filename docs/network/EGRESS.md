# Brokered Egress

**Status:** `VERIFIED` for the fetch path — the daemon has reassembled a real object over
TLS from a live host in three resumed calls, byte-identical to what `curl` retrieves, and
refused a traversal, an unauthorized call and an unknown source on the same running
instance. **`IMPLEMENTED`, not `VERIFIED`, for the unit** — `otwono-fetchd.service` has been
checked with `systemd-analyze verify` and its syscall filter confirmed against the target
rootfs, but no image has been built or booted with it. See `docs/build/VERIFICATION-LOG.md`.

Decided in **ADR-0014**.

---

## 1. What this is for

Three subsystems need bytes that are not on this disk, and none of them can go and get
them:

| Who | Why it cannot fetch |
|---|---|
| `otwono-aid` | `PrivateNetwork=yes`, `RestrictAddressFamilies=AF_UNIX`, and it should keep both — it is the daemon that must keep answering when other things break |
| `otwono-updated` | Z1, where the trust-zone table says there is no network, and where the node's Ed25519 signing key lives |
| `otwono-netd` | Has the network, but over the mesh transport; ONM content is peers, not hosts |

`otwono-fetchd` is the one component that makes outbound client connections to hosts
outside the mesh. The boundary is drawn by **protocol**: this daemon does client HTTPS to
hosts an operator named; `otwono-netd` keeps fetching ONM content from peers over its own
authenticated transport.

## 2. The shape of a request

A caller names a **source** and a **path suffix**. It never supplies a URL.

```
fetch.get { "source": "models", "path": "q4/model.gguf" }
                ↓
https://models.example.org/otwono/models/q4/model.gguf
        └── from the allow-list ──┘└── caller's ──┘
```

Consequently a caller cannot choose the scheme, the host, the port, the query string, a
fragment, or a header. The scheme is always `https` and is not configurable, so no
configuration file can turn a source into cleartext.

### The residue, stated plainly

The path suffix is caller-chosen text that leaves this node, which makes it a covert
channel. This design bounds it rather than pretending it is closed:

- at most **256 bytes**,
- from `[A-Za-z0-9._~/-]` only — `%` is refused rather than decoded, so no encoded
  delimiter survives to be decoded by something downstream,
- no `.` or `..` segment, no empty segment, no leading or trailing `/`,
- one audit-log record per fetch, naming the source and the path.

DNS resolution is a second channel underneath that one, and it exists on any node with a
network. "Bounded and logged" is the claim. "Impossible" is not.

## 3. The allow-list

`/etc/otwono/fetch.d/*.toml`, loaded in filename order. Schema:
`schemas/egress-source.schema.json`. Ships **empty**, and empty permits nothing.

```toml
[[source]]
id = "models"                       # what a caller names, and the token's resource
host = "models.example.org"         # DNS name, lowercase, never an IP literal
port = 443                          # optional
path_prefix = "/otwono/models/"     # absolute, slash-terminated
max_bytes = 21474836480             # hard cap on one object
```

Rules the loader enforces, and why each one is there:

| Rule | Reason |
|---|---|
| `path_prefix` must end in `/` | Otherwise a suffix extends the last segment: `/otwono/model` + `s-elsewhere/x` reaches `/otwono/models-elsewhere/x` |
| `host` must be a DNS name, not an IP literal | The allow-list exists to be read by a person deciding whether to approve it, and a name is what TLS verifies against |
| `host` must be lowercase | Matching is case-insensitive; the file should read the way the code compares it |
| Unknown fields are refused | `max_byte = 10` would otherwise read as a cap and impose none |
| A duplicate `id` is refused | First-wins and last-wins are both defensible and both surprising |
| A malformed file stops the daemon | Better than a node that permits something nobody wrote down |

Validate before deploying: `otwono-fetchd --check --source-dir /etc/otwono/fetch.d`.

## 4. Redirects are decided here, not by a library

The HTTP client is configured with `max_redirects(0)`. A `3xx` is a server proposing a
different request, and whether that request is permitted is a question about the operator's
allow-list.

Every hop is resolved and then put through the same admission the original request passed:
same scheme, same host (compared case-insensitively), same port, same prefix, same path
rules. A `Location` that is neither an absolute `https://` URL nor a root-relative path is
refused rather than resolved. At most five hops.

A redirect that leaves the source is a **denial**, not a hop.

## 5. Resumption is the normal path

The control plane's client sets a read timeout, and a 4 GB model on a rural link does not
fit inside one call. So `fetch.get` transfers up to a per-call budget (64 MiB by default)
and returns progress; the caller calls again until `complete` is true.

```json
{ "complete": false, "bytes_have": 67108864, "bytes_total": 4294967296, "blob_path": null }
```

This makes resumption the ordinary path rather than an error path, which is the only way it
ever gets tested. Three cases the implementation handles, each with a test:

- **`206 Partial Content`** — append at the offset we asked for.
- **`200 OK` in reply to a `Range`** — the server ignored us and is sending the whole
  object from byte zero. It **replaces** the partial rather than extending it. Appending
  would produce a file of the right length and the wrong bytes; the caller's digest would
  catch that, after wasting the entire transfer.
- **The object changed** — a differing `ETag` on a resumed download is refused early rather
  than discovered by a failed digest gigabytes later.

## 6. Nothing it fetches is trusted

Bytes land in `/var/lib/otwono/fetch` (mode 0700) and a path comes back. Verification —
digest, signature, provenance — happens in the **caller**, with the caller's code.
`otwono-aid` re-hashes every blob it installs, and this daemon's opinion of what it
downloaded is not an input to that.

A finished object is renamed into place, so a `.blob` is never a truncated file. Partials
are `.part`; the spool key is a BLAKE3 of `source\0path` rather than the path itself,
because a filename built from caller-influenced text is a traversal waiting to be found.

`ai.models.pull` is therefore a fetch followed by the existing `ai.models.install`. **It
adds no new trust code.**

## 7. `net.fetch`

Registered separately from `net.egress` — the same narrowing ADR-0010 made for signing,
where `id.sign_session` exists so that policy can grant the mesh what it needs without
granting a general signing oracle.

| | `net.egress` | `net.fetch` |
|---|---|---|
| Blast radius | `Egress` | `Egress` |
| `always_confirm` | yes | **no** |
| Resource | — | the source id |

`always_confirm: false` is the one place this design weakens the rule in
`SECURITY-MODEL.md` §2, and it does so deliberately: **the confirmation moves rather than
disappearing.** It happens when a source is added to the allow-list, which is a
`policy.write` and does confirm. Requiring a human per call would make unattended update
downloads impossible on exactly the headless node this OS is for.

The token's resource is the **source id**, so policy can grant one caller the model host
and not the update host:

```toml
[[rule]]
action = "net.fetch"
subjects = ["uid:0"]
resource = "models"
decision = "allow"
```

Nothing is granted by default. A node fetches nothing until an operator makes **two**
separate decisions: adding a source, and granting `net.fetch`. Either alone does nothing.

## 8. Hardening

Certificates are verified against the **system trust store** (`/etc/ssl/certs`), not the
roots bundled into the binary. An operator running a mirror behind their own CA installs it
where the rest of the OS looks, and it works; with bundled roots it would not, and nothing
in the image would explain why. This is what makes `ca-certificates` in every recipe
load-bearing rather than decorative.

`otwono-fetchd` is **Z3** — a hostile-input boundary, since TLS records and HTTP responses
from a remote host are attacker-controlled bytes — but a *separate* Z3 process from
`otwono-netd`, so a compromise of one does not yield the other's keys. It holds no keys at
all: the only network-facing component in OTWONO with nothing to steal.

```
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6   # no AF_NETLINK: it enumerates nothing
IPAddressDeny=localhost link-local multicast       # contains the obvious SSRF
ReadWritePaths=/run/otwono /var/lib/otwono/fetch   # the allow-list is read-only to it
```

`IPAddressDeny` is best-effort by nature: it is a cgroup BPF program, and on a kernel
without `CONFIG_CGROUP_BPF` systemd logs a failure and runs the unit anyway. A mitigation,
not a boundary. It is worth having because a source is a DNS name that must resolve
somewhere, and a name pointing at `127.0.0.1` would otherwise aim a fetch at this node's
own control plane.

## 9. What this does not do

- **No transparent decompression, deliberately.** `Accept-Encoding` negotiation is off. A
  decompressed body is not the bytes the server's `Range` header addresses, so a resumed
  fetch would ask for the wrong offset — and on a server that accepted it, would silently
  assemble a corrupt file. We fetch opaque blobs; the caller hashes them.
- **A path must name a file**, so no trailing `/`. Real servers do redirect a directory-ish
  path to its slash form — PyPI's simple index does — and this daemon refuses to follow
  that. It fetches objects, not listings.
- **An object whose size the server will not state must fit one call.** Without a stated
  size there is nothing to resume to, so a partial that could never be continued is
  discarded and the call fails with a message saying to retry with a larger `max_bytes`.
- **Proxy support is untested.** `ureq` reads the standard proxy environment variables, and
  the dev environment's egress is proxied — which is how TLS was exercised at all — but no
  node ships proxy configuration and nothing here manages it.
- **No authentication to a source.** No headers, no bearer tokens, no signed URLs. A source
  that needs one does not work through this interface as specified, and making it work is a
  schema change rather than a caller's choice.
- **No parallelism.** One object at a time per call, and a caller drives the loop.
- **No bandwidth limit.** A fetch will use whatever the link gives it.
- **No content-type or size negotiation.** The object is whatever the server sends, up to
  `max_bytes`.

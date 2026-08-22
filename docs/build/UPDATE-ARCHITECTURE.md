# Update Architecture

**Status:** `SPECIFIED`. No implementation yet.

## 1. A/B images

```
GPT: [ ESP 512M ] [ root_a ] [ root_b ] [ otwono-data ]
```

- Write the inactive slot, verify the signature and hash, flip the boot pointer, reboot.
- Boot-attempt counter reverts to the previous slot on failure — GRUB environment block on
  amd64, U-Boot `bootcount` on arm64.
- A userspace health check must mark the new slot good; until then it is provisional.
- `/var` and `/home` live on the data partition and survive updates. The root filesystem is
  treated as replaceable, which is exactly what makes rollback safe.

## 2. Health check

The new slot is marked good only when: all core units are active, `otwono-permd` and
`otwono-idd` answer on their sockets, the node identity loads, the network subsystem
initialises at least one link, and the capability profile validates against its schema.
Anything less and we roll back — an OS that boots to a broken assistant on a headless SBC
in a shed is not "booted".

## 3. Independent layers

| Layer | Mechanism | Cadence |
|---|---|---|
| Base image | A/B, signed | Release train |
| Applications | Flatpak / apt | Independent |
| AI models | Content-addressed, hash-pinned, tier-gated | Independent |
| Policy and schemas | Signed config bundles | Independent |

A security fix must not require a new model, and a new model must not require a new kernel.

## 4. Transport, including over the mesh

Update bundles are content-addressed and labelled `REPLICATED`, so a node with an uplink
can seed an update to an offline cluster over ONM. Verification is against the release key,
so a relaying peer cannot tamper with the payload. This is precisely why the design relies
on content addressing plus signing rather than on trusting the transport.

Delta updates: content-addressed chunking means only changed chunks transfer — which
matters enormously on a `Narrow` or `Trickle` link.

## 5. Implementation candidates

We will not write our own updater. To be settled by ADR (OQ-5):

| Candidate | For | Against |
|---|---|---|
| **RAUC** | Mature, embedded-focused, A/B native, bundle signing, both architectures | Extra dependency, C |
| **systemd-sysupdate** | Already in the base, fewer moving parts | Newer, weaker rollback story |
| **Mender** | Full fleet management | Server-oriented; conflicts with local-first |

## 6. Guarantees

- An interrupted update never bricks a device — the running slot is never touched.
- Every update is signed; an unsigned update is refused, with no override flag in a
  production build.
- Rollback is automatic on boot failure and manual on user request.
- Updates work with no Internet, over the mesh, or from removable media.

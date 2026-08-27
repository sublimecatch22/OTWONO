# Architecture Decision Records

One file per decision: `ADR-NNNN-short-title.md`.

Format: **Context** (forces at play) → **Decision** → **Consequences** (good and bad)
→ **Alternatives rejected, and why**.

Status: `proposed` · `accepted` · `superseded by ADR-NNNN` · `deprecated`.

Rules:
- A decision that is hard to reverse gets an ADR **before** the code.
- Replacing working code requires an ADR (see `CLAUDE.md` §2.2).
- Introducing a language, a mesh routing protocol, an updater, or a screen-synthesis app
  adapter each require an ADR.

| ADR | Title | Status |
|---|---|---|
| 0001 | Rust for privileged system daemons | accepted |
| 0002 | Debian-derived base with a staged image builder | accepted |
| 0003 | JSON-RPC 2.0 over Unix sockets as the Local Control Plane | accepted |
| 0004 | Capability vector with tier composition, not a single score | accepted |
| 0005 | Integrate inference backends; never write an engine | accepted |
| 0006 | Ed25519 node identity, separate from user identity | accepted |
| 0007 | Fail-closed data visibility labels with provenance propagation | accepted |
| 0008 | A/B image updates with automatic rollback | accepted |
| 0009 | Mount-free image assembly, and the kernel on the ESP | accepted |
| 0010 | Split the node's two keys across two daemons | accepted |
| 0011 | Drive llama.cpp as a supervised adapter process, not a linked library | accepted |
| 0012 | Confine the inference engine with Landlock, applied by the adapter to itself | accepted |
| 0013 | Boot the Raspberry Pi 4 through U-Boot's UEFI, not vendor UEFI | accepted |
| 0014 | One brokered fetch daemon for outbound HTTPS; the mesh keeps its own transport | accepted |
| 0015 | A content-addressed cluster cache, not a ledger | accepted |
| 0016 | Content-defined chunking with FastCDC at 16/64/256 KiB | accepted |
| 0017 | The ONM content-fetch protocol: ranged, object-scoped, verified per chunk | accepted |
| 0018 | Large content moves as files handed to the caller's uid, not as bytes on the control plane | accepted |
| 0019 | `SHARED` objects: encrypt before chunking, wrap per recipient, sharing key in `otwono-idd` | accepted |
| 0020 | A recipient discovers what was shared with it, by asking, and learns nothing else | accepted |
| 0021 | Signed transfer receipts: the receiver counts, and says so under its own key | accepted |
| 0022 | The wallet: a third key family, its own daemon, and what `PRIVATE` means when a transaction must be published | accepted |
| 0023 | `otwono-walletd` holds nothing unlocked: the passphrase per call, no session, and no public key in the clear | accepted |
| 0024 | The confirmation channel: asynchronous, bound to one request, and refused to the subject that asked | accepted |
| 0025 | Render every address family, and decide the chain later | accepted |
| 0026 | Replication is pulled, never pushed, and it is best-effort by construction | accepted |
| 0027 | Signed mutable pointers: sequence numbers, not timestamps, and rollback is the threat | accepted |
| 0028 | Addressed messages: the envelope already exists, so store-and-forward is about custody, and it is pulled | accepted |

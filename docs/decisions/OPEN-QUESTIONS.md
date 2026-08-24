# Open Questions

Unresolved decisions. Each must end in an ADR backed by measurements, not opinion.
Do not treat any of these as settled, and do not let code quietly settle one.

| ID | Question | Blocks | Owner phase |
|---|---|---|---|
| **OQ-1** | Which desktop stack per tier? Headless + TUI for T0/T1 is clear; T2+ is not — GNOME, KDE, Sway/labwc, or something custom-light? | Phase 10 | Prototype on real hardware first |
| **OQ-2** | Revisit NixOS (or OSTree) for 1.x reproducibility and atomic updates once Debian A/B is working and we know what we actually need | 1.x | After Phase 8 |
| **OQ-3** | Kernel strategy: Debian generic everywhere, or a per-board vendor kernel where mainline is inadequate? Affects the entire maintenance burden | Phase 1 | Measure on Pi 5 and RK3588 |
| **OQ-4** | Mesh routing: Reticulum, Yggdrasil, Babel, or libp2p-only with a thin DTN layer? Needs measurements on real radio links, not a table | Phase 9 | Phase 3 evaluation, Phase 9 decision |
| **OQ-5** | Updater: RAUC or systemd-sysupdate? | Phase 8 | Phase 8 |
| **OQ-6** | Default model set per tier, and their licences. Redistribution rights differ sharply and constrain what can ship in an image | Phase 4 | Phase 4 |
| **OQ-7** | Does the agent get a general shell tool at all, behind a strong confirmation? Enormously useful, enormously dangerous | Phase 7 | Phase 7 |
| **OQ-8** | Federation trust model between separate ONM networks — descriptor exchange is specified, but the trust bootstrap is not | Phase 10 | Phase 10 |
| **OQ-9** | Where does the user identity live when a user has five nodes? Which node is authoritative, and how is it recovered? | Phase 3 | Phase 3 |
| **OQ-10** | Are LoRa duty-cycle rules encodable generically, or does the OS need per-region regulatory profiles shipped as data? | Phase 9 | Phase 9 |
| **OQ-11** | Distributed search ranking without a global index and without a reputation system — is a useful ranking even possible? | Phase 6 | Phase 6 research |
| **OQ-12** | Do we ship a browser integration for `onm://`, or a local HTTP gateway with a reserved hostname? Extension maintenance versus a weaker security boundary | Phase 6 | Phase 6 |
| **OQ-14** | The arm64 boot counter. ADR-0008 names U-Boot `bootcount`, but the packaged `u-boot-rpi` for the Pi 4 is built with `CONFIG_BOOTCOUNT_LIMIT` off (measured 2026-08-23). Rebuild U-Boot with it, or count boots in userspace against a grubenv-equivalent on the ESP? | Phase 8 | Phase 8 |
| **OQ-15** | Wi-Fi `AccessPoint` and 802.11s `WiFiMesh` roles: which consumer chipsets and drivers actually support AP+STA concurrently and 802.11s at all? Needs a hardware survey, not a datasheet reading. Also: per-region channel/power/DFS profiles, the Wi-Fi analogue of OQ-10 | Phase 9 | Phase 9 |
| **OQ-17** | Freeloading in the neighbourhood cache. ADR-0015 deliberately ships no fairness mechanism, because accounting between distrusting neighbours is where a ledger creeps back in. Revisit only with evidence of real harm — and if revisited, what is the cheapest thing that works? | 1.x | After Phase 6 |
| **OQ-18** | Assessment integrity in the education service. A record signed by a node the learner controls proves what was recorded, not that the learner did the work. What does a defensible proctored mode look like, and which accreditors would accept it? Partly a research question and partly a conversation with an accrediting body | Phase 7 | Phase 7 |
| **OQ-19** | Bank connectivity beyond file import. Holding credentials on a household device, or an aggregator holding them, are both at odds with §1 of CLAUDE.md. Does any jurisdiction's open-banking regime give the *user* a direct, revocable, credential-free grant to their own software? Default stays "no automatic sync" until answered | Phase 7 | Phase 7 |
| **OQ-20** | Cross-platform application model. Portable Rust core plus thin native shells, WebAssembly components, a PWA over `onm://`, or a combination? Long half-life, needs an ADR rather than a default. `docs/services/PORTABLE-APPS.md` §4 argues WASM for third-party apps because capability-brokering is what WASM already is | Phase 7 | Phase 7 |
| **OQ-21** | Apple platforms. A Linux node cannot produce a distributable iOS build — Xcode on Apple hardware and App Store review are required, and macOS needs notarization. Full native support is a commercial decision (hardware, a paid account, submitting to a gatekeeper) that sits awkwardly with §1 of CLAUDE.md. Accept a PWA on iOS, or take the dependency? | Phase 10 | Needs a decision from the project owner, not an engineer |
| **OQ-22** | How are OTWONO apps distributed? Over the mesh as signed, content-addressed bundles is the obvious answer and reuses the model-manifest machinery — but installing third-party executable content is the highest-consequence thing a node can do, and `pkg.install` already always confirms | Phase 7 | Phase 7 |
| **OQ-23** | A compact encoding for constrained links. Measured 2026-08-24: an ONM content message spends 229–360 bytes on its JSON envelope alone — two 64-character hex ids and their field names — so a manifest window does not fit an EU868 LoRa frame at all and a chunk reply carries about six bytes of payload. What is the cheapest thing that fixes it: CBOR, session-scoped short handles for the ids, or shorter field names? Each trades away some of the "readable with `socat`" property that made JSON right everywhere else | Phase 9 | Phase 9, with OQ-24 |
| **OQ-24** | The handshake does not fit a radio either. Measured 2026-08-24: the two Noise session-proof frames are 447 bytes each, against a 256-byte `Trickle` payload, so ONM cannot authenticate over LoRa however small the content messages become. Fragment at the link layer, or shrink the proof (raw bytes instead of base64 JSON, an abbreviated binding)? Link-layer fragmentation is the more general answer and it is a new failure mode on a lossy medium | Phase 9 | Phase 9, before OQ-23 is worth doing |
| **OQ-26** | A want-list for fan-out. Without one, a peer holding a third of an object is asked for chunks it lacks about twice for every one it has, and each miss is a wasted round trip. A worker now remembers what a peer has refused so it never asks twice, which bounds the waste at one round trip per (peer, chunk) — but that is still O(peers x chunks) misses on a sparse street. Bitswap-style want-lists, a Bloom filter of held chunks in the manifest reply, or an explicit `content.have` message? | Phase 6 | Phase 6 |

## Resolved

Kept with their original IDs, because other documents cite them.

| ID | Question | Settled by |
|---|---|---|
| **OQ-13** | Where does brokered network egress live? One egress daemon or one per subsystem? | **ADR-0014** — one `otwono-fetchd` for outbound HTTPS; `otwono-netd` keeps ONM's own transport |
| **OQ-16** | Chunking parameters for the neighbourhood cache — a network-wide compatibility constant | **ADR-0016** — FastCDC v2020 at 16/64/256 KiB, one parameter set for the whole network, chosen on measured index cost |
| **OQ-25** | Streaming a fan-out fetch straight to a file, so a small board can fetch an object larger than its RAM | **Done** — `fetch_object_to_file` writes each verified chunk at its known offset with `pwrite`, holding one chunk per peer; the offsets come from the manifest, which is verified before any chunk is requested |

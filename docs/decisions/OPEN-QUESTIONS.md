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
| **OQ-16** | Chunking parameters for the neighbourhood cache — algorithm and size. Two nodes that chunk differently cannot serve each other, so this is a network-wide compatibility constant, versioned in the schema. Changing it splits the swarm | Phase 5 | Phase 5 |
| **OQ-17** | Freeloading in the neighbourhood cache. ADR-0015 deliberately ships no fairness mechanism, because accounting between distrusting neighbours is where a ledger creeps back in. Revisit only with evidence of real harm — and if revisited, what is the cheapest thing that works? | 1.x | After Phase 6 |
| **OQ-18** | Assessment integrity in the education service. A record signed by a node the learner controls proves what was recorded, not that the learner did the work. What does a defensible proctored mode look like, and which accreditors would accept it? Partly a research question and partly a conversation with an accrediting body | Phase 7 | Phase 7 |
| **OQ-19** | Bank connectivity beyond file import. Holding credentials on a household device, or an aggregator holding them, are both at odds with §1 of CLAUDE.md. Does any jurisdiction's open-banking regime give the *user* a direct, revocable, credential-free grant to their own software? Default stays "no automatic sync" until answered | Phase 7 | Phase 7 |
| **OQ-20** | Cross-platform application model. Portable Rust core plus thin native shells, WebAssembly components, a PWA over `onm://`, or a combination? Long half-life, needs an ADR rather than a default. `docs/services/PORTABLE-APPS.md` §4 argues WASM for third-party apps because capability-brokering is what WASM already is | Phase 7 | Phase 7 |
| **OQ-21** | Apple platforms. A Linux node cannot produce a distributable iOS build — Xcode on Apple hardware and App Store review are required, and macOS needs notarization. Full native support is a commercial decision (hardware, a paid account, submitting to a gatekeeper) that sits awkwardly with §1 of CLAUDE.md. Accept a PWA on iOS, or take the dependency? | Phase 10 | Needs a decision from the project owner, not an engineer |
| **OQ-22** | How are OTWONO apps distributed? Over the mesh as signed, content-addressed bundles is the obvious answer and reuses the model-manifest machinery — but installing third-party executable content is the highest-consequence thing a node can do, and `pkg.install` already always confirms | Phase 7 | Phase 7 |

## Resolved

Kept with their original IDs, because other documents cite them.

| ID | Question | Settled by |
|---|---|---|
| **OQ-13** | Where does brokered network egress live? One egress daemon or one per subsystem? | **ADR-0014** — one `otwono-fetchd` for outbound HTTPS; `otwono-netd` keeps ONM's own transport |

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

## Resolved

Kept with their original IDs, because other documents cite them.

| ID | Question | Settled by |
|---|---|---|
| **OQ-13** | Where does brokered network egress live? One egress daemon or one per subsystem? | **ADR-0014** — one `otwono-fetchd` for outbound HTTPS; `otwono-netd` keeps ONM's own transport |

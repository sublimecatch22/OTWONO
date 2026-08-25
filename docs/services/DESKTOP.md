# Desktop

**Status:** `SPECIFIED`. No implementation. Targeted at Phase 10, with pieces landing earlier
where they belong to another subsystem — see §7 for what lives where.

---

## 1. What it is

The thing a person actually sees. Everything else in this repository is machinery that
exists so this can be good.

It is **tier-selected**, not one desktop: `OTWONO-ARCHITECTURE.md` L5 already commits to
"headless CLI · lightweight Wayland · full desktop", chosen by the capability profile
(CLAUDE.md §2.6) and not by a build flag or an installer question. A Raspberry Pi 4 and a
16-core workstation run the same OS and must not run the same shell.

## 2. What it must feel like

Stated as requirements because "user friendly" is not one:

1. **Quick access to apps and functions.** Whatever a person does most is reachable without
   hunting. Search that finds an app, a document, a setting and a person in one field.
2. **A customisable dashboard.** The home surface is arranged by its owner, not by us. What
   the node is doing, what is shared, what the household's week looks like — chosen widgets,
   moved and removed freely, and a default that is useful on first boot without being noisy.
3. **The assistant is toggleable, at any moment, from anywhere.** Not buried in settings, not
   requiring a restart, and *off* is a real state in which nothing is listening and nothing
   is inferred. A person who turns it off and gets a subtly worse computer has been punished
   for the choice, and will not trust the toggle again.
4. **Nothing waits on the network.** Every surface renders and every local action works with
   the cable out (prime directive 2). Things that genuinely need a peer say so in place,
   rather than spinning.

## 3. The contribution control

The node's own settings surface, and the first place most people will meet the mesh.

- **One switch: the node is on, or it is off.** Off means off — not degraded, not "still
  helping a little."
- **Sliders for what is contributed**: storage, RAM, CPU, and GPU. These are not decoration
  over a fixed policy; they set real limits, enforced by cgroups and by the capability
  policy engine, and the neighbourhood cache's existing budget is the first of them
  (`NEIGHBOURHOOD-CACHE.md`).
- **A plain reading of what the node has actually done** — bytes served, storage held,
  uptime — and where that record goes, which by default is nowhere. See §4.

## 4. Wallet, contributions, and honesty about rewards

The finance surface holds a **crypto wallet** alongside the household's accounts, and the
contribution readout feeds a reward system that is **deliberately not part of this OS**
(`OTWONO-ARCHITECTURE.md` §Non-goals; ADR-0021).

Three things the UI has to get right, because they are easy to get wrong in a way that
misleads someone about their own money and their own privacy:

- **A wallet key is not the node key** (ADR-0022). Passphrase-derived, BIP-39/32/44, in
  `otwono-walletd` — its own daemon, no network. Losing a machine must not lose money, and
  one person may run several nodes against one wallet.
- **Contribution records are `PRIVATE` and stay put unless exported**, and export is an
  explicit, confirmed, audited action — never a background sync. The UI says where the
  numbers are and what leaving would mean.
- **The UI must not describe contribution counters as proof.** They are self-reported;
  ADR-0021's receipts make them counter-signed, which is better and still not proof. A
  screen that implies earnings are guaranteed by the OS is lying.

## 5. Virtual machines

First-class, not an afterthought: a person keeping a Windows install, an old distribution or
a disposable sandbox is a person who can move to this OS without giving something up.

Integrated, never written (CLAUDE.md §2.3): KVM/QEMU with libvirt, driven through a UI, and
gated by the capability profile — a T0 board with 512 MiB does not offer VMs it cannot run,
and says why rather than failing at launch.

## 6. Games

A small number, built in, for the reason a desktop has ever shipped a card game: a new
machine should have something on it that is simply pleasant. Scoped small, local-only, and
never a reason to add a dependency the rest of the system does not want.

## 7. What already has a home, and what is new here

Much of the desktop's content belongs to subsystems that already exist on paper. This
document is the shell and the surfaces; it does not restate them.

| Surface | Where it is specified |
|---|---|
| Financial tracker and planner | `docs/services/FINANCE.md`, whose §2a carries the wallet (ADR-0022) |
| Teacher / school system | `docs/services/EDUCATION.md` |
| AI assistant, and driving ordinary apps | `docs/ai/AI-RUNTIME.md`, `docs/ai/APP-INTEGRATION.md` |
| Media editing and viewing | CLAUDE.md §2.3's integration table — LibreOffice, GIMP, Krita, Inkscape, Audacity, Ardour, Kdenlive, Shotcut, mpv, ffmpeg. **Integrated, never rewritten** |
| Contribution metering | ADR-0021 (receipts) and the metering ADR that follows it |
| Tier-selected shell | `OTWONO-ARCHITECTURE.md` L5, `docs/hardware/CAPABILITY-TIERS.md` |
| **New here** | the dashboard, the contribution control, VMs, games, the assistant toggle, and the wallet's placement in the finance surface |

## 8. Media playback, and a note on skinning

**mpv and VLC are integrated, never rewritten.** A themed VLC is a legitimate want and a
cheap one — VLC supports skins — but it must stay a skin. The moment "custom OTWONO skin"
becomes "our own player" the project has taken on a codebase it cannot maintain, for a
problem two mature projects already solved. If theming turns out to require patching either
player, that is an ADR, not a Tuesday.

## 9. What is deliberately not decided here

- **Which desktop environment or compositor.** GNOME, KDE, a bespoke Wayland shell and
  wlroots-based options all have different costs at T0. It is an ADR, informed by measurement
  on real hardware, and it is the largest open question in this document.
- **Whether `otwono-shell` is an application inside a conventional desktop, or the desktop
  itself.** These are very different projects.
- **Widget and extension model** for the dashboard, including whether third parties can add
  to it — which is a sandboxing question before it is a UI one.

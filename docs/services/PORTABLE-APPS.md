# Portable Applications

**Status:** `SPECIFIED`, and partly **blocked on things engineering cannot decide** — see
§3. No implementation. Related to but distinct from `docs/ai/APP-INTEGRATION.md`, which is
about *driving* existing Linux applications; this is about *producing* software that runs
elsewhere.

---

## 1. Two different requirements in one sentence

"The OS should add and create features and apps that run natively on Linux, Windows,
Android and macOS/iOS" contains two requests that need separating, because they cost
wildly different amounts:

| | Request | Difficulty |
|---|---|---|
| **A** | The node **authors** software that runs natively on other platforms — an app factory | Hard, and partly blocked (§3) |
| **B** | OTWONO's **own** features reach a user on their phone, laptop or work PC — a companion client | Tractable, and probably what makes the OS usable daily |

They are not alternatives — B is a prerequisite for anyone caring about A — but B is worth
an order of magnitude more per unit of effort, and it should come first.

## 2. What "natively" actually costs, per platform

| Target | Build from a Linux node? | Signing | Distribution |
|---|---|---|---|
| **Linux** | Yes — native | Optional | Direct, Flatpak, distro packages |
| **Windows** | Yes — `x86_64-pc-windows-gnu`, mingw-w64 | Authenticode cert (paid) or SmartScreen warnings | Direct download, winget |
| **Android** | Yes — NDK + SDK both run on Linux | Local keystore, free | Direct APK, F-Droid, or Play (account required) |
| **macOS** | Partly — cross-compilation works; **notarization does not** | Apple Developer account | Notarized `.app`, or Gatekeeper blocks it |
| **iOS** | **No** | Apple Developer account | App Store review only |

Three of the five are genuinely achievable from a Linux node with cross toolchains. The
project already cross-compiles to `aarch64` in CI, so the machinery is not foreign.

## 3. The Apple wall, stated plainly

**A Linux machine cannot produce a distributable iOS build.** Not because the toolchain is
missing but because Apple's terms and tooling require Xcode on macOS hardware, and App
Store review is the only general distribution path. No amount of engineering removes this.
macOS is softer — cross-compilation is possible — but notarization still needs Apple
credentials, and without it Gatekeeper refuses the app on a normal user's machine.

What this means for a project whose first principle is that the user owns their computer:

- **Full native iOS support is a commercial decision, not a technical one.** It requires
  Apple hardware in the build path and a paid developer account, renewed annually, plus
  submission to a review process that can decline software for reasons unrelated to
  quality. That is a dependency on a gatekeeper, which is precisely what this OS exists to
  reduce.
- **A web app reaches iOS without any of that**, at the cost of "native".

This is not a reason to abandon the goal. It is a reason to decide it deliberately rather
than discover it after building for the four platforms that were easy. Recorded as
**OQ-21**.

## 4. What an "app" should be here

Four options, and they are not exclusive:

| Model | Reach | Native feel | Sandboxable | Verdict |
|---|---|---|---|---|
| Per-platform native binaries | All five, at 5× cost | Best | Per-platform | Only where it earns it |
| **Portable Rust core + thin native shell** | All five | Good | Per-platform | **The default for OTWONO's own clients** |
| **WebAssembly component + host shell** | Anywhere with a host | Good enough | **By construction** | **The default for third-party apps** |
| Web app / PWA over `onm://` | All five, one artifact | Weakest | Browser sandbox | The reach floor, and the iOS answer |

### Why WebAssembly is the right answer for third-party apps *specifically here*

This OS already has the hard half of the problem solved. Its whole security model is that
**nothing holds ambient privilege** — every capability is brokered, scoped, time-limited and
audited (`SECURITY-MODEL.md` §2). A WASM component is a unit of code that *cannot* reach the
filesystem, the network or the clock unless a host hands it that capability explicitly.

Those two models are the same shape. An app that must ask `otwono-permd` for everything is
not a constraint bolted onto WASM; it is what WASM already is. A native binary, by contrast,
starts with the user's full authority and has to be fenced back in — which is the work that
Landlock and seccomp are doing for the inference engine, and it is much harder.

WASM also happens to be portable, which is the requirement. That it is *also* the safest way
to run software a user did not write is the reason to pick it rather than a coincidence.

## 5. What should ship first

1. **The companion client (B).** A small application that speaks the Local Control Plane to
   the user's own node over the mesh: read the wiki, send messages, ask the assistant, check
   the node. Rust core plus a thin shell — Linux and Android first, Windows next, a PWA for
   iOS. This makes the node useful from the device people actually carry.
2. **A WASM app host on the node**, wired to the permission broker. An app declares what it
   needs; the broker decides; the user sees it.
3. **Authoring (A) last**, and only for the targets that do not require somebody else's
   permission — Linux, Windows, Android.

## 6. What is deliberately not promised

- **No claim of "native on iOS"** until §3 is settled with a decision and a budget.
- **No cross-platform UI framework is chosen here.** That is a real decision with a long
  half-life and it needs its own ADR (**OQ-20**), not a paragraph.
- **No app store.** Distribution of OTWONO apps over the mesh is the interesting question
  and it belongs with the content store and signing work, not here.

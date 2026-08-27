# AI Runtime Abstraction

**Status:** partly `VERIFIED`, partly `IMPLEMENTED`, partly `SPECIFIED`.

* **`VERIFIED` — local inference runs on a booted node.** Both architectures boot, install a
  model over the control plane and complete a prompt, printing
  `OTWONO-AI-INFER-OK … tokens=8`. Built with `AI_SMOKE_MODEL=1`, which bundles a generated
  model and a policy granting `ai.admin`; a release image ships neither and stage 60 asserts
  it.
* **`VERIFIED` — local inference runs.** llama.cpp is integrated as a supervised adapter
  process (ADR-0011, §3.1). A prompt goes from a control-plane client through the
  permission broker, admission control, the supervisor, the adapter and `llama-server` into
  a GGUF model and comes back as generated tokens. Exercised end to end against a real
  engine by `crates/otwono-llama/tests/end_to_end.rs` and
  `tests/control-plane/tests/ai_infer_llama.rs`; see `docs/build/VERIFICATION-LOG.md` for
  the run.
* **Implemented and unit tested:** the model manifest and its JSON Schema
  (`schemas/model-manifest.schema.json`), the footprint arithmetic, **admission control**
  including every refusal in §4, backend *selection* as a pure function, the on-disk
  catalog, manifest signature verification against a publisher trust store, the
  out-of-process backend supervisor, and backend discovery. In `crates/otwono-ai`,
  exercised against the fixture machines in `otwono-capability`'s `testing` module.
* **Implemented and exercised over sockets:** `ai.capabilities`, `ai.models.list`,
  `ai.admit` and `ai.infer` in `otwono-aid`.
* **Not shipped by default.** The engine is 17 MiB of third-party C++ per architecture and
  a ten-minute build, so images opt in with `AI_ENGINE=llama.cpp`. A stock image reports
  `local_inference_available: false` and refuses `ai.infer` with a reason — which is
  accurate, not a stub.
* **Not implemented:** streaming (§3.2), `ai.models.pull`, `ai.embed`, `ai.transcribe`,
  `ai.synthesize`, `ai.vision`, `ai.session.*`, remote inference over ONM, and every tiered
  assistant shape in §6.

## 1. The problem

The same request — "summarize this document" — must run on a Raspberry Pi with a 1B model
on four Cortex-A76 cores, on a laptop with an 8B model on an iGPU, on a workstation with a
70B model across two GPUs, and possibly on a trusted peer's machine over ONM. The caller
must not care.

## 2. The interface

`otwono-aid` exposes one JSON-RPC surface on `/run/otwono/ai.sock`:

| Method | Purpose |
|---|---|
| `ai.capabilities` | What this node can do right now, given its tier and loaded models — **implemented** |
| `ai.models.list` | The catalog, each entry with whether this machine can run it and why not — **implemented** |
| `ai.admit` | Dry run: would this model load, at what cost, and if not what context would fit — **implemented** |
| `ai.models.install` | Install from a local manifest and weights, verifying both — **implemented**, needs `ai.admin` |
| `ai.models.verify` | Re-hash an installed model against its manifest — **implemented** |
| `ai.models.pull` / `ai.models.remove` | Fetching over the network, and removal — specified; see §5.1 |
| `ai.infer` | Text completion — **implemented**, non-streaming, gated by `ai.infer` |
| `ai.embed` | Embeddings for RAG |
| `ai.transcribe` | Speech to text |
| `ai.synthesize` | Text to speech |
| `ai.vision` | Image understanding |
| `ai.session.*` | Stateful sessions with KV-cache reuse |

Every method returns a typed error. `ModelTooLargeForTier`, `NoBackendAvailable`, and
`InsufficientMemory` are ordinary, expected results on small hardware — not exceptions.

That is literal, not rhetorical: `ai.admit` returns a **successful call reporting
`admissible: false`**, with the numbers and a suggested smaller context, rather than an RPC
error. Browsing a catalog on a Pi should not mean handling an exception on every second
entry.

## 3. Backends

We integrate; we do not write an inference engine.

| Backend | Hardware | Tier |
|---|---|---|
| `llama.cpp` (CPU) | Any; ARM NEON/dotprod/i8mm, x86 AVX2/AVX-512 | T1+ |
| `llama.cpp` (Vulkan) | Any GPU with Vulkan compute, including Mali/Adreno/iGPU | T2+ |
| `llama.cpp` (CUDA / ROCm) | NVIDIA / AMD discrete | T3+ |
| ONNX Runtime + vendor EP | RKNN (RK3588), Hailo, Coral, Intel NPU, AMD XDNA | T1+ (NPU-dependent) |
| `whisper.cpp` | ASR, all tiers with size selection | T1+ |
| Piper | TTS, cheap enough for T1 | T1+ |
| vLLM | High-throughput multi-request serving | T4 |
| Remote peer | Another node's `otwono-aid` over ONM | any, opt-in |

### 3.1 How llama.cpp is attached

`STATUS: VERIFIED`. Three processes, decided in ADR-0011:

```
otwono-aid  ──NDJSON JSON-RPC on stdio──▶  otwono-llama-backend  ──HTTP over a
 (daemon)      (otwono_ai::supervisor)         (otwono-llama)        Unix socket──▶  llama-server
```

- The daemon links no engine, so a model loader that segfaults cannot take the control
  plane with it — and `cargo test --workspace` needs no C++ toolchain, no engine and no
  model file.
- The adapter translates; it does not re-solve. `llama-server` already does model loading,
  KV-cache reuse across requests, slot management and sampling.
- The engine listens on a **Unix socket in a `0700` directory**, not a loopback TCP port.
  `llama-server` has no authentication, so on a multi-user machine a port would let any
  local account drive the model and read what is in flight.
- Availability is **discovered on disk**, never decided at compile time: a backend exists
  when its adapter is under `/usr/libexec/otwono/ai-backends` and its engine under
  `/usr/lib/otwono/ai/llama.cpp/<variant>/bin/`. One OTWONO build therefore serves a
  CPU-only Pi and a CUDA workstation, and `ai.capabilities` describes the machine rather
  than the build.

Every `ai.infer` goes through admission control first, and the engine is started with the
context window admission control granted — not with its own defaults, which know nothing
about this node's reserve. That is asserted directly: a test reads the engine's
`/proc/<pid>/cmdline` and checks the `--ctx-size` it was actually given.

### 3.2 What is not there yet

- **Streaming.** One request, one response. Interactive use wants tokens as they are
  produced, which needs several frames per request *and* a control plane that can carry
  them to the caller. `llama-server` can stream; the gap is ours.
- ~~**Sandboxing the engine.**~~ Done: the adapter confines itself with Landlock before
  starting an engine (ADR-0012), so the engine can read the model store and the system
  libraries and nothing else of the node's — not the identity key, not the audit log, not
  the policy store. It fails closed on a kernel without Landlock. What is still missing is
  PID and mount isolation and a seccomp filter; `/proc` and `/sys` stay readable because
  ggml's CPU detection needs them.
- **Any backend other than llama.cpp.** whisper.cpp, Piper, ONNX Runtime and vLLM each need
  their own adapter. The protocol is deliberately engine-neutral so that is additive work.
- **GPU variants: the build exists, the execution has never happened.** Stage 35 takes
  `AI_ENGINE_VARIANTS` (default `cpu`) and can install a `vulkan` build alongside the CPU
  one, discovered and selected the same way. Two things are worth stating plainly rather
  than leaving to be discovered:

  **No GPU has ever run this.** The Vulkan variant has not been built, let alone executed,
  because the development environment has neither the Vulkan SDK nor a GPU. `STATUS:
  IMPLEMENTED` for the build path and the selection logic; nothing about GPU inference is
  `VERIFIED`. What *is* verified is that the CPU path is unchanged by their existence — a
  booted node still reports `backends=llama-cpp-cpu` and `local_inference=available`
  through the refactored stage (`docs/build/VERIFICATION-LOG.md`, 2026-08-27).

  **A backend can be discovered and still fail to start.** Discovery is a path probe, so an
  image carrying a Vulkan engine reports `llama-cpp-vulkan` as installed even where no
  driver exists. Selection will not choose it on a machine whose profile reports no
  accelerator — but a machine that *has* a GPU and lacks a working driver would get past
  selection and fail at exec. That gap is real and untested; it is why the variant is
  opt-in and why `cuda` and `rocm` are still unbuildable by the stage.

- **`cuda` and `rocm` builds.** Discovery and selection handle them; the stage does not
  build them. Each needs a vendor toolchain on the build host and a licence decision about
  redistribution.

Rules:

- Backends run **out of process** and are supervised. A backend crash is a typed error,
  never a hang and never a daemon restart. Implemented in `otwono-ai::supervisor`:
  newline-delimited JSON over stdin/stdout (the same framing as ADR-0003), a `hello`
  exchange so a wrapper script's error message is a protocol error rather than a timeout,
  a deadline on every read, a cap on line length because a backend is a large C++ program
  parsing untrusted files, and **process-group kill** so terminating a wrapper does not
  orphan the engine it started. Since ADR-0012 the adapter also confines itself with
  Landlock before starting an engine, so the engine inherits a filesystem boundary it
  cannot escape.
- Backend selection comes from the capability profile plus the model manifest. It is a
  pure function, and it is unit-testable against fixture profiles with no hardware present.
- A backend that fails to load falls back down the list and records why, so `ai.capabilities`
  can explain "CUDA present but the driver is too old" instead of silently going slow.
  *(Specified; today a load failure is reported rather than retried against the next
  backend.)*

## 4. Admission control

The most common failure mode of local AI on small hardware is a confident load followed by
the OOM killer. So loading is gated:

```
required = model.footprint(quantization, context_length, batch)
available = profile.memory.available - reserve(tier)
if accelerator_offload: check VRAM headroom separately
if required > available: refuse with ModelTooLargeForTier and suggest an alternative
```

The reserve keeps the desktop, the network daemon, and the user's actual work alive. It is
tier-dependent and configurable, and it is never zero — `Reserve::FLOOR` is 256 MiB and
`Reserve::custom` clamps to it. A configurable reserve that can be set to zero is not a
safety mechanism, it is a footgun with a dial on it.

Details the implementation pins down, each with a test:

- **The default context is the model's maximum, not its minimum.** Admitting on the
  strength of a short first turn is how a session gets killed three messages later.
- **A partial KV block is charged in full.** Rounding down under-counts.
- **KV is charged per sequence**; weights and overhead are shared.
- **Arithmetic saturates.** A manifest is external data, and an overflow would wrap a
  colossal model into a small number and admit it.
- **`vram_bytes: None` means undetectable, not zero.** An offloading backend on a card
  reporting no figure is refused rather than guessed at — guessing either way risks the
  exact OOM this exists to prevent.
- **A refusal suggests the largest context that would fit**, or says nothing fits. A
  refusal a user cannot act on is barely better than a crash.

## 5. Model catalog

Models are content-addressed blobs plus a signed manifest:

```json
{
  "schema_version": "1.0.0",
  "id": "qwen3-4b-instruct-q4_k_m",
  "family": "qwen3", "parameters": 4000000000,
  "quantization": "Q4_K_M",
  "format": "gguf",
  "blake3": "…",
  "size_bytes": 2500000000,
  "min_tier": "T1_EDGE",
  "footprint": { "weights_bytes": 2500000000, "kv_per_1k_ctx_bytes": 130000000 },
  "max_context": 32768,
  "capabilities": ["chat", "tools"],
  "license": "apache-2.0",
  "backends": ["llama-cpp-cpu", "llama-cpp-vulkan", "llama-cpp-cuda"]
}
```

### What the digest is for

`blake3` in a manifest was, until Phase 4 slice 5, only a *filename*: the catalog joined it
onto the blob directory and nothing hashed the contents. A signed manifest paired with a
swapped blob therefore loaded as trusted — the signature covered the manifest, the manifest
named a digest, and nobody checked the bytes against it. Signing was doing half a job.

`ai.models.install` now hashes the blob and refuses on mismatch, so the chain runs end to
end: a trusted publisher signs a manifest, the manifest names a digest, and the digest names
these exact bytes. Size is checked first, because a truncated download is the common case
and costs a `stat` rather than a full hash.

Verification happens **at install, not at load**. Hashing is linear in model size, and
paying it on every load — on the hardware this project exists for — would tax the common
path to defend against an attacker who already has write access to a root-owned directory,
which is to say root. `ai.models.verify` re-checks on demand, and reports a mismatch as a
successful call rather than an error: auditing a catalog should not mean handling an
exception per corrupt model.

Installs are atomic. A blob is staged beside its destination and renamed into place, so an
interrupted install leaves a stray `.incoming-*` file and never a truncated blob under a
name claiming to be complete — which matters because `weights_present` is a file-exists
check and would answer yes.

### 5.1 Fetching

`ai.models.pull` is still absent, and the reason is architectural rather than a matter of
effort. `otwono-aid` runs with `PrivateNetwork=yes` and `RestrictAddressFamilies=AF_UNIX`;
it has no network and should not gain one, since it is the daemon that must keep answering
when other things break. A child process inherits that namespace, so the fetcher cannot
simply be spawned the way a backend adapter is.

Downloading therefore needs a separate brokered component with its own network namespace,
its own hardening, and a policy about which hosts it may contact — that last part being a
design decision in its own right, not an implementation detail. **ADR-0014 settles it**
(closing OQ-13): one daemon, `otwono-fetchd`, is the only component that makes outbound
client connections to hosts outside the mesh. Callers name a source from an allow-list and
a path suffix, never a URL; the response lands in a spool and the caller verifies it.
`ai.models.pull` is therefore a fetch followed by the `ai.models.install` below — **it adds
no new trust code.** Both exist now.

**The ordering is the design.** Each step is cheaper than the one after it, and each can
refuse, so the expensive one only runs once the cheap ones have agreed:

1. Fetch the **manifest** — kilobytes.
2. Check **provenance**. A manifest signed by nobody this node trusts is refused here,
   before a byte of weights moves. `install` already applies that reasoning to hashing;
   applied to downloading, it saves an hour rather than a minute.
3. Check whether the model **could ever fit this machine** — `fits_this_machine`, which is
   deliberately *not* `admit`. `admit` asks whether a model can load right now and so
   refuses when no backend is installed, which is exactly the state a node is in while
   being set up. It also returns only its first error, so `NoBackendAvailable` masked the
   memory arithmetic entirely — a fresh 4 GiB board would have downloaded a 40 GiB model
   without ever weighing it. The narrower check takes no backend list and cannot be masked.
   Overridable with `allow_unadmittable`, because downloading for another machine is real.
4. Fetch the **weights**, resumably, in bounded calls.
5. **Install**, which re-hashes them against the manifest. The fetcher's word is taken for
   nothing.
6. **Discard the spool copy** — `install` copies rather than moves, and leaving it would
   mean a 4 GB model costs 8 GB on a board that has 8.

Guarded by `ai.admin`, the same as a local install: what makes it powerful is that it
changes what the node will run, and where the bytes came from does not change that. The
fetch itself needs `net.fetch`, which `otwono-aid` requests from the broker scoped to the
named source.

A node with no `--fetch-socket` says so plainly rather than failing obscurely, and that is
the shipped default: an operator must add a source **and** grant `net.fetch` before this
node downloads anything.

Splitting it this way has a payoff already banked: everything that decides whether to
*trust* a model is tested exhaustively with no network anywhere near it.

- Models are **never** committed to git.
- Manifests are signed; unsigned or unverified models require an explicit opt-in and run
  with reduced tool access, because a model that can call tools is executable content.
- The catalog is tier-filtered by default: a T1 node is not offered a 32B model it cannot
  run. It may still be shown, greyed, with the reason.

### What a signature covers, and the three outcomes

A signature covers the manifest with its own `signature` field removed, serialized as JSON
with object keys sorted and no insignificant whitespace, prefixed with
`otwono-model-manifest-v1:`. The canonicalizer is written out rather than delegated to
`serde_json`'s map ordering: that ordering is a consequence of a feature flag any
transitive dependency could flip, and the meaning of every signature must not depend on it.

Verification has **three** outcomes, deliberately not two:

| Outcome | Meaning | Loadable? |
|---|---|---|
| Trusted | Signature verifies, publisher is in the trust store | Yes |
| Unsigned | No signature at all | Only with an explicit opt-in |
| Untrusted publisher | Signature verifies, signer unknown to this node | Only with an explicit opt-in |
| **Bad signature** | Signature does not verify | **Never** |

Collapsing the last into "unsigned" would be a real weakness: the opt-in means *"I know
where this came from"*, and it must never silently cover *"somebody changed this in
transit"*. Adding a key to the trust store is a sensible response to an unknown publisher;
it is never the fix for a broken signature.

The trust store is `/etc/otwono/publishers.d/*.toml`. It **ships empty**, and empty means
trust nobody. No default publisher key is baked into the image: shipping one would mean
every OTWONO node automatically trusts whoever holds it, which is the node operator's
decision and not the image builder's.

## 6. Tiered assistant behaviour

| Tier | Assistant shape |
|---|---|
| T0 | No LLM. A deterministic command grammar (`otwono do …`) plus optional delegation to a trusted peer or a cloud provider **if the user configures one**. Honest about being non-conversational. |
| T1 | 1–3B model, single-step tool calling, no RAG index, short context |
| T2 | 7–8B model, embeddings + local RAG, multi-step planning, ASR |
| T3 | 14–32B, GPU offload, speculative decoding, parallel sub-agents, TTS, optional image generation |
| T4 | 70B-class, concurrent sessions, serving peers, optional fine-tuning |

Degradation must be **honest**: a T0 node says "I cannot do that locally; I can queue it
for your workstation when it is reachable" rather than producing a bad answer.

**STATUS: IMPLEMENTED** for T0 — `otwono-capability::AssistantShape` (derived from the tier,
per CLAUDE.md §2.6), the `otwono-agent` grammar, and the `otwono` binary that `otwono do …`
names. Unit, cross-process and end-to-end tested: the real binary runs as a process against
real daemons, saves a file and reads it back by content id.

The binary is `otwono`, not `otwono-agentctl`, and that is a deliberate departure from
CLAUDE.md §4.3. Every other CLI in the system drives exactly one daemon — `otwono-hwctl`
drives `otwono-hwd`. This one drives none; it reaches whichever service a verb happens to
name, and it is the single command a person types. The crate keeps §4.3's name.

**The subcommand name has a cost, found by using it.** `do` is a shell keyword, so
`otwono do help` makes shellcheck warn SC1010 on every line that calls it — it cannot tell a
subcommand from a malformed `for` loop. It parses correctly; the warning is real but wrong.
Any script driving the assistant has to quote it (`otwono 'do' help`) or wrap it in a
helper, which `build/files/otwono-assistant-check` does once at the top.

That is a small, permanent tax on every shell script in the project that touches the
assistant, and it is worth weighing against the reason for the name: `otwono do …` is what
two design documents promised and what reads naturally to a person. Recorded here so the
trade is visible if the name is ever revisited.

**Not exercised on a booted node.** The binary is staged into images by build stage 05, but
no boot check runs it, so there is no entry in `VERIFICATION-LOG.md` for it yet.

Three properties are worth stating because they are what make a T0 assistant safe rather
than merely small:

- **It executes nothing.** A parse produces an inert `Intent` naming a control-plane method
  and the capability the *caller* must hold. It carries no token and confers nothing; an
  assistant that could act because it parsed something would have ambient privilege, which
  §2.5 forbids. A test asserts a serialized intent contains nothing bearer-shaped.
- **It invents no operations.** Every verb names a method that already exists and is already
  gated by `otwono-permd`, checked against the daemons' own `describe` output rather than
  against a copy of it — so adding a verb is a vocabulary decision, never a security one.
- **It does not guess.** The only concession to fuzziness is a "did you mean" over the
  closed verb set, which produces a better error and never an `Intent`. A deterministic
  grammar that guesses is a language model with none of the benefits.

What T0 does *not* have yet is delegation: `Elsewhere::Peer` and `ConfiguredProvider` are
modelled and rendered into the refusal, but nothing queues work for a peer or forwards it to
a provider. The binary therefore always answers `Nowhere`, and that is honest rather than
lazy — a peer is only somewhere to send work if it *advertises inference*, and §7's
`ai-provider` does not exist. Naming an authenticated peer as a candidate would produce a
refusal pointing at a machine that would reject the request.

**A note on how the refusal is chosen.** An unknown first word is two different failures, and
telling them apart is most of what §6 asks for. "fetc" is a misspelling — the user knows the
vocabulary and missed, and the fix is to offer the verb they nearly typed. "summarise my
week" is not a misspelling; it is a request the machine genuinely cannot serve, and answering
it with "I do not know how to summarise" blames the user's phrasing for a limit of the
hardware. The discriminator is whether anything in the closed verb set is within an edit
distance of two.

The first version got this wrong — it answered both with `UnknownVerb`, which made §6's
sentence unreachable on the only tier it was written for. Every unit test passed, because
they asserted what the code did. It was found by running the binary and reading the output.

## 7. Remote inference over ONM

A `T3`/`T4` node may offer `ai-provider` to authorized peers, with per-peer quotas
(tokens/day, concurrent sessions, allowed models).

For the requesting node, remote inference is a **data egress event**:

- The visibility labels of everything in the prompt are checked first.
- `PRIVATE` content is refused by default.
- The user sees which peer served the request.
- It is written to the audit log.

Convenient remote inference that silently ships private documents to someone else's GPU
would defeat the entire point of the project.

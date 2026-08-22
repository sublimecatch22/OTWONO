# AI Runtime Abstraction

**Status:** `SPECIFIED`. No implementation yet.

## 1. The problem

The same request — "summarize this document" — must run on a Raspberry Pi with a 1B model
on four Cortex-A76 cores, on a laptop with an 8B model on an iGPU, on a workstation with a
70B model across two GPUs, and possibly on a trusted peer's machine over ONM. The caller
must not care.

## 2. The interface

`otwono-aid` exposes one JSON-RPC surface on `/run/otwono/ai.sock`:

| Method | Purpose |
|---|---|
| `ai.capabilities` | What this node can do right now, given its tier and loaded models |
| `ai.models.list` / `ai.models.pull` / `ai.models.remove` | Catalog management |
| `ai.infer` | Text completion / chat, streaming |
| `ai.embed` | Embeddings for RAG |
| `ai.transcribe` | Speech to text |
| `ai.synthesize` | Text to speech |
| `ai.vision` | Image understanding |
| `ai.session.*` | Stateful sessions with KV-cache reuse |

Every method returns a typed error. `ModelTooLargeForTier`, `NoBackendAvailable`, and
`InsufficientMemory` are ordinary, expected results on small hardware — not exceptions.

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

Rules:

- Backends run **out of process** and are supervised. A backend crash is a typed error,
  never a hang and never a daemon restart.
- Backend selection comes from the capability profile plus the model manifest. It is a
  pure function, and it is unit-testable against fixture profiles with no hardware present.
- A backend that fails to load falls back down the list and records why, so `ai.capabilities`
  can explain "CUDA present but the driver is too old" instead of silently going slow.

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
tier-dependent and configurable, and it is never zero.

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

- Models are **never** committed to git.
- Manifests are signed; unsigned or unverified models require an explicit opt-in and run
  with reduced tool access, because a model that can call tools is executable content.
- The catalog is tier-filtered by default: a T1 node is not offered a 32B model it cannot
  run. It may still be shown, greyed, with the reason.

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

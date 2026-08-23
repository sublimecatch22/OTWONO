#!/usr/bin/env python3
"""Synthesize a tiny, randomly-initialized llama-architecture GGUF model.

Why this exists
---------------
The llama.cpp integration has to be tested against a *real* engine loading a *real*
model file. Downloading a published model is not an option in every environment where
this needs to run -- CI has no model store, and the OTWONO dev environment's egress
allow-list does not include a model host -- and a 2 GB download is a poor test
dependency in any case.

So: generate a model. It is a genuine GGUF file with the tensor set, tokenizer and
metadata llama.cpp requires for `general.architecture = llama`, at dimensions small
enough (~500 KB) to load in milliseconds. The weights are random, so the tokens it
emits are gibberish.

That is the point, and it is worth being blunt about the limit: this proves the
*integration* -- that llama.cpp starts, loads a model, accepts a prompt, and returns
generated tokens through our adapter -- and it proves nothing whatsoever about output
quality. A test that asserted on text content would be asserting on a specific model's
behaviour, which is not what we are integrating.

The output is deterministic for a given --seed so a fixture can be regenerated
byte-identically.

Models are never committed to git (CLAUDE.md section 5). Generate into out/ or a temp dir.

Usage:
    tools/make-tiny-gguf.py --gguf-py <llama.cpp>/gguf-py --out /tmp/tiny.gguf
"""

import argparse
import pathlib
import sys


def build_vocab():
    """A printable-ASCII SentencePiece vocabulary.

    Note what is *not* here: the 256 `<0xXX>` byte-fallback tokens a real SentencePiece
    vocabulary carries. That is deliberate and it is the interesting detail of this file.

    With random weights the model samples tokens uniformly at random, so if byte tokens are
    reachable it will emit arbitrary bytes -- and llama.cpp's response parser rejects a
    completion that is not valid UTF-8 with a 500, which looks exactly like an integration
    bug and is not one. Restricting the vocabulary to printable ASCII makes every possible
    output valid UTF-8 by construction, so the only thing left that can fail the test is
    the thing under test.

    The cost is that this fixture can only tokenize printable ASCII prompts. That is all
    the integration test feeds it, and a real model ships a real tokenizer.
    """
    tokens = ["<unk>", "<s>", "</s>"]
    scores = [0.0, 0.0, 0.0]
    # 2 = UNKNOWN, 3 = CONTROL, 1 = NORMAL, in gguf's TokenType enum.
    types = [2, 3, 3]

    def normal(piece, score=-1.0):
        tokens.append(piece)
        scores.append(score)
        types.append(1)

    # U+2581 is SentencePiece's space marker; a literal 0x20 piece is not how SPM works.
    normal("\u2581", 0.0)
    for code in range(0x21, 0x7F):
        normal(chr(code))
    # A handful of multi-character pieces so the SPM merge path is exercised at all, and
    # not only the single-character fallback.
    for piece in ["\u2581the", "\u2581a", "\u2581of", "in", "er", "th", "on"]:
        normal(piece, -0.5)
    return tokens, scores, types


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--gguf-py", required=True, help="path to llama.cpp's gguf-py directory")
    ap.add_argument("--out", required=True, help="path to write the .gguf to")
    ap.add_argument("--manifest", help="also write an OTWONO model manifest describing the output")
    ap.add_argument("--model-id", default="otwono-smoke-test", help="manifest id, used with --manifest")
    ap.add_argument("--seed", type=int, default=20260823)
    ap.add_argument("--layers", type=int, default=2)
    ap.add_argument("--embd", type=int, default=64)
    ap.add_argument("--heads", type=int, default=4)
    ap.add_argument("--ff", type=int, default=128)
    ap.add_argument("--ctx", type=int, default=512)
    args = ap.parse_args()

    sys.path.insert(0, str(pathlib.Path(args.gguf_py).resolve()))
    try:
        import gguf
        import numpy as np
    except ImportError as e:
        sys.exit(f"cannot import a dependency ({e}); need numpy and llama.cpp's gguf-py at --gguf-py")

    n_embd, n_head, n_layer, n_ff = args.embd, args.heads, args.layers, args.ff
    if n_embd % n_head:
        sys.exit(f"--embd {n_embd} must be divisible by --heads {n_head}")
    head_dim = n_embd // n_head

    tokens, scores, types = build_vocab()
    n_vocab = len(tokens)

    rng = np.random.default_rng(args.seed)

    def w(*shape):
        # Small values keep the forward pass numerically tame: an untrained model with
        # large weights can saturate into NaN logits, which would look like an engine
        # bug rather than the meaningless-but-finite output we want.
        return (rng.standard_normal(shape) * 0.02).astype(np.float32)

    def ones(*shape):
        return np.ones(shape, dtype=np.float32)

    out = pathlib.Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    writer = gguf.GGUFWriter(str(out), "llama")

    writer.add_name("otwono-tiny-test")
    writer.add_description("randomly initialized; for integration testing only, output is gibberish")
    writer.add_context_length(args.ctx)
    writer.add_embedding_length(n_embd)
    writer.add_block_count(n_layer)
    writer.add_feed_forward_length(n_ff)
    writer.add_head_count(n_head)
    writer.add_head_count_kv(n_head)
    writer.add_rope_dimension_count(head_dim)
    writer.add_rope_freq_base(10000.0)
    writer.add_layer_norm_rms_eps(1e-5)
    writer.add_file_type(gguf.LlamaFileType.ALL_F32)

    writer.add_tokenizer_model("llama")
    writer.add_token_list(tokens)
    writer.add_token_scores(scores)
    writer.add_token_types(types)
    writer.add_bos_token_id(1)
    writer.add_eos_token_id(2)
    writer.add_unk_token_id(0)
    writer.add_add_bos_token(True)
    writer.add_add_eos_token(False)

    # numpy shape is (out_features, in_features); GGUF stores ne reversed, which is what
    # llama.cpp's llama-arch expects for each of these names.
    writer.add_tensor("token_embd.weight", w(n_vocab, n_embd))
    for i in range(n_layer):
        writer.add_tensor(f"blk.{i}.attn_norm.weight", ones(n_embd))
        writer.add_tensor(f"blk.{i}.attn_q.weight", w(n_embd, n_embd))
        writer.add_tensor(f"blk.{i}.attn_k.weight", w(n_embd, n_embd))
        writer.add_tensor(f"blk.{i}.attn_v.weight", w(n_embd, n_embd))
        writer.add_tensor(f"blk.{i}.attn_output.weight", w(n_embd, n_embd))
        writer.add_tensor(f"blk.{i}.ffn_norm.weight", ones(n_embd))
        writer.add_tensor(f"blk.{i}.ffn_gate.weight", w(n_ff, n_embd))
        writer.add_tensor(f"blk.{i}.ffn_up.weight", w(n_ff, n_embd))
        writer.add_tensor(f"blk.{i}.ffn_down.weight", w(n_embd, n_ff))
    writer.add_tensor("output_norm.weight", ones(n_embd))
    writer.add_tensor("output.weight", w(n_vocab, n_embd))

    writer.write_header_to_file()
    writer.write_kv_data_to_file()
    writer.write_tensors_to_file()
    writer.close()

    print(f"wrote {out} ({out.stat().st_size} bytes): "
          f"{n_layer} layers, n_embd={n_embd}, n_head={n_head}, n_ff={n_ff}, vocab={n_vocab}")

    if args.manifest:
        write_manifest(args, out, n_embd, n_layer, n_vocab, head_dim)


def write_manifest(args, gguf_path, n_embd, n_layer, n_vocab, head_dim):
    """Emit a manifest that genuinely describes the file just written.

    The digest is computed here, from these bytes, rather than being copied from anywhere:
    the whole point of `ai.models.install` is that it refuses a manifest whose digest does
    not match, so a manifest generator that guessed would only ever produce a refusal.

    It is deliberately *unsigned*. Signing would need a publisher key, and shipping one in
    a build would mean every node that trusts it trusts whoever holds it -- exactly what
    the empty trust store exists to avoid. So this model installs only with the explicit
    unsigned opt-in, which also means the boot check exercises that path.
    """
    import json
    import pathlib as _pathlib

    try:
        import blake3
    except ImportError:
        sys.exit("--manifest needs the blake3 module (pip install blake3)")

    hasher = blake3.blake3()
    with open(gguf_path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            hasher.update(chunk)

    size = gguf_path.stat().st_size
    # 2 x n_layer x n_embd x 4 bytes per token for an f32 KV cache, x 1024 tokens.
    kv_per_1k = 2 * n_layer * n_embd * 4 * 1024
    manifest = {
        "schema_version": "1.0.0",
        "id": args.model_id,
        "family": "llama",
        "parameters": n_vocab * n_embd * 2 + n_layer * n_embd * n_embd * 4,
        "quantization": "F32",
        "format": "gguf",
        "blake3": hasher.hexdigest(),
        "size_bytes": size,
        "min_tier": "T0_MICRO",
        "footprint": {
            "weights_bytes": size,
            "kv_per_1k_ctx_bytes": kv_per_1k,
            # A llama-server process costs tens of megabytes before it touches a model.
            # Under-declaring it is how admission control says yes and the OOM killer
            # disagrees, which is the failure this whole subsystem exists to prevent.
            "overhead_bytes": 64 * 1024 * 1024,
        },
        "max_context": args.ctx,
        "capabilities": ["chat"],
        "license": "apache-2.0",
        "backends": ["llama-cpp-cpu"],
    }
    path = _pathlib.Path(args.manifest)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"wrote {path}: id={manifest['id']} blake3={manifest['blake3'][:16]}... unsigned")


if __name__ == "__main__":
    main()

# MLX / mlx-lm: KV-cache quantization + MLX-vs-GGUF state

**All fetches performed 2026-09-19 (system date `Sat Sep 19 16:26 UTC 2026`).**
Markers: ✅ verified (fetched primary source) · ⚠️ secondary (author-reported / third-party claim, not independently reproduced) · ❓ unverified (no evidence fetched).

Caveat: `raw.githubusercontent.com/.../main/...` fetches reflect `main` at fetch time but do not pin a commit SHA. "Current" = as fetched on 2026-09-19.

---

## PART A — mlx-lm PR/issue statuses

| # | Actual type | Title | Status | Opened | Last activity | Source |
|---|---|---|---|---|---|---|
| 1583 | **ISSUE** (not PR) | KV-cache quantization unreachable in BatchGenerator (RotatingKVCache.to_quantized() unimplemented) | **OPEN** | 2026-07-18 (mabaeyens) | **2026-09-19** (rromenskyi pushed referencing commit `8f3ac9f` in fork `ipsupport-llc/mlx-lm`) | ✅ [issues/1583](https://github.com/ml-explore/mlx-lm/issues/1583) |
| 1074 | **PR** (not issue) | feat: QuantizedRotatingKVCache + KVSplit (K/V different bits) | **CLOSED, unmerged** | 2026-03-30 (deceptech-packet-ninja) | closed 2026-08-21 by maintainer `zcbenz` | ✅ [pull/1074](https://github.com/ml-explore/mlx-lm/pull/1074) |
| 1832 | **PR** (not issue) | Adding kv cache quantization to server | **MERGED** (commit `a1ae4f4`) | 2026-09-03 (kunalb123) | merged 2026-09-09 by `michalk8` | ✅ [pull/1832](https://github.com/ml-explore/mlx-lm/pull/1832) |
| 1854 | **PR** | server: Add --kv-bits for KV cache quantization | **CLOSED, unmerged** | 2026-09-06 (dsengupta-mdsol) | closed 2026-09-09 by `michalk8` | ✅ [pull/1854](https://github.com/ml-explore/mlx-lm/pull/1854) |
| 1584 | PR (bonus, same author as #1583) | Implement KV-cache quantization for the continuous-batching path | **CLOSED, unmerged** | 2026-07-18 (mabaeyens) | closed 2026-08-21 by `zcbenz` | ✅ [pull/1584](https://github.com/ml-explore/mlx-lm/pull/1584) |

URL resolution (asked explicitly): `pull/1583` **redirects to `issues/1583`** → 1583 is an **issue**. `issues/1074` **redirects to `pull/1074`** → 1074 is a **PR**. Likewise `issues/1832` → `pull/1832` (PR). ✅

### What the discussions say

**#1583 (OPEN issue).** `maybe_quantize_kv_cache`/`QuantizedKVCache` are "complete and tested" for the single-sequence `generate_step`/`stream_generate` path, but unreachable from `BatchGenerator` whenever `max_kv_size` is set: `BatchGenerator._make_new_cache()` always wraps per-job caches in `RotatingKVCache`, which merges into `BatchRotatingKVCache` — and both `to_quantized()` raise. Suggested fix: `RotatingQuantizedKVCache` + `BatchRotatingQuantizedKVCache`, `BatchGenerator` gains `kv_bits`/`kv_group_size`/`quantized_kv_start`, rejects `quantized_kv_start > 0` on the batch path. Last activity is a fork commit on the report date → still live.

**#1074 (CLOSED).** Added `QuantizedRotatingKVCache` + asymmetric KVSplit (`bits=(8,4)`). Was approved by reviewer `Thump604`, who runs Qwen3.5-122B on M2 Ultra in production. Closed by maintainer `zcbenz`: *"the number of PRs is way beyond our capacity to review so I'm closing the non-essential ones so we can actually work on this repo."* Also carries author benchmarks (see Part C). Valuable as evidence the primitive is wanted and works, but **not upstream**.

**#1832 (MERGED, 2026-09-09).** The PR that actually landed. Closes #1308, #1043, #615. Adds `--kv-bits`/`--kv-group-size`/`--quantized-kv-start` to `mlx_lm.server`, fixes `_is_batchable` to return False when `kv_bits` is set, adds `SERVER.md` docs and tests. Ships memory benchmarks (below). Maintainer `michalk8` approved ("LGTM (did some minor modifications)") and merged.

**#1854 (CLOSED).** Same feature, 8 days later by a different author. Closed by `michalk8`: *"Closing this in favor of #1832 (the other PR also fixed the batchable property, etc.)"* Its diff used `--quantized-kv-start` default **0**; the merged #1832 uses `DEFAULT_QUANTIZED_KV_START` (=5000).

### Is quantized KV cache MERGED upstream? — YES, with a hard scope limit

**Available upstream today for the single-sequence path, exposed via the server CLI. NOT available for the continuous-batching (`BatchGenerator`) path.**

Exact quoted signatures from fetched `main` (2026-09-19):

`mlx_lm/models/cache.py` ✅
```python
class QuantizedKVCache(_BaseCache):
    step = 256
    def __init__(self, group_size: int = 64, bits: int = 8):
```
```python
# KVCache
    def to_quantized(self, group_size: int = 64, bits: int = 4) -> QuantizedKVCache:
```
```python
# RotatingKVCache  — STILL A STUB
    def to_quantized(self, group_size: int = 64, bits: int = 4) -> QuantizedKVCache:
        raise NotImplementedError("RotatingKVCache Quantization NYI")
```
```python
# BatchRotatingKVCache — STILL A STUB
    def to_quantized(self, group_size: int = 64, bits: int = 4) -> QuantizedKVCache:
        raise NotImplementedError("BatchRotatingKVCache Quantization NYI")
```
```python
def make_prompt_cache(model: nn.Module, max_kv_size: Optional[int] = None) -> List[Any]:
```
`QuantizedKVCache` state carries `(keys, values, offset, group_size, bits)`; `nbytes` sums the packed tuples via `tree_reduce`.

`mlx_lm/generate.py` ✅
```python
DEFAULT_QUANTIZED_KV_START = 5000
```
```python
def maybe_quantize_kv_cache(prompt_cache, quantized_kv_start, kv_group_size, kv_bits):
    if kv_bits is None:
        return
    for e, c in enumerate(prompt_cache):
        if hasattr(c, "to_quantized") and c.offset >= quantized_kv_start:
            prompt_cache[e] = c.to_quantized(group_size=kv_group_size, bits=kv_bits)
```
```python
def generate_step(
    prompt: mx.array,
    model: nn.Module,
    stream: mx.Stream | mx.ThreadLocalStream = generation_stream,
    *,
    max_tokens: int = 256,
    sampler: Optional[Sampler] = None,
    logits_processors: Optional[List[LogitsProcessor]] = None,
    max_kv_size: Optional[int] = None,
    prompt_cache: Optional[Any] = None,
    prefill_step_size: int = 2048,
    kv_bits: Optional[int] = None,
    kv_group_size: int = 64,
    quantized_kv_start: int = DEFAULT_QUANTIZED_KV_START,
    prompt_progress_callback: Optional[Callable[[int, int], None]] = None,
    input_embeddings: Optional[mx.array] = None,
) -> Generator[Tuple[mx.array, mx.array], None, None]:
```
```python
def speculative_generate_step(
    ...,
    kv_bits: Optional[int] = None,
    kv_group_size: int = 64,
    quantized_kv_start: int = DEFAULT_QUANTIZED_KV_START,
) -> Generator[Tuple[mx.array, mx.array, bool], None, None]:
```
Argparse (generate.py): `--kv-bits` ("Number of bits for KV cache quantization. Defaults to no quantization."), `--kv-group-size` (default 64), `--quantized-kv-start` (default `DEFAULT_QUANTIZED_KV_START` = 5000).

`BatchGenerator.__init__` — **no KV-quant parameters** ✅:
```python
    def __init__(
        self,
        model: nn.Module,
        *,
        max_tokens: int = 128,
        stop_tokens: Optional[Sequence[Sequence[int]]] = None,
        sampler: Optional[Sampler] = None,
        logits_processors: Optional[List[LogitsProcessor]] = None,
        completion_batch_size: int = 32,
        prefill_batch_size: int = 8,
        prefill_step_size: int = 2048,
        max_kv_size: Optional[int] = None,
        stream=None,
    ):
```

`mlx_lm/server.py` ✅ — server-side flags **are present**:
```python
from .generate import (
    DEFAULT_QUANTIZED_KV_START,
    BatchGenerator, ...
```
```python
    parser.add_argument(
        "--kv-bits",
        type=int,
        default=None,
        help="Number of bits for KV cache quantization (e.g., 4 or 8). "
        "Reduces memory usage for long contexts. Disables batching, so "
        "requests are served one at a time. Default: None (full precision)",
    )
    parser.add_argument("--kv-group-size", type=int, default=64, ...)
    parser.add_argument(
        "--quantized-kv-start",
        type=int,
        default=DEFAULT_QUANTIZED_KV_START,
        help="Token position to start KV cache quantization "
        f"(default: {DEFAULT_QUANTIZED_KV_START})",
    )
```
```python
    def _is_batchable(self, args):
        return (
            self.model_provider.is_batchable
            and args.seed is None
            and self.cli_args.kv_bits is None
        )
```

**Documentation split (important):** `README.md` does **not** mention KV quantization at all — its "Long Prompts and Generations" section only documents `--max-kv-size` and `--prefill-step-size` ✅. `mlx_lm/SERVER.md` **does** document it, including the key caveat ✅:

> Attention on a quantized cache is not fused, so it keeps a score matrix of `prefill_step_size x context_length`. Decrease `--prefill-step-size`, or the matrix can be larger than the memory that the quantized cache saves.
> **A quantized KV cache does not support batching. The server processes requests one at a time when you set `--kv-bits`.**

**Net answer:** parameter names upstream are `kv_bits` / `kv_group_size` / `quantized_kv_start` (Python) and `--kv-bits` / `--kv-group-size` / `--quantized-kv-start` (CLI, both `mlx_lm generate` and `mlx_lm.server`). `QuantizedKVCache` exists and defaults to `bits=8, group_size=64`. Merged server support landed 2026-09-09. Quantization is **one request at a time** on the server, and **unavailable** for `BatchGenerator` (issue #1583 open, PRs #1074/#1584 closed unmerged) and **unavailable** whenever `max_kv_size` is used (RotatingKVCache stub — note `make_prompt_cache` builds `RotatingKVCache(max_size=..., keep=4)` when `max_kv_size` is set, so the two flags are mutually exclusive in practice).

---

## PART B — which servers implement quantized KV, and under what flag

| Server / runtime | Flag or config name | Notes | Marker | Source |
|---|---|---|---|---|
| **mlx-lm upstream** (`mlx_lm.server`) | `--kv-bits`, `--kv-group-size`, `--quantized-kv-start` | Now **upstream**, not private. Disables batching; default start 5000 | ✅ verified (raw source) | [server.py](https://raw.githubusercontent.com/ml-explore/mlx-lm/main/mlx_lm/server.py), [SERVER.md](https://raw.githubusercontent.com/ml-explore/mlx-lm/main/mlx_lm/SERVER.md) |
| **mlx-lm upstream** (Python API) | `kv_bits`, `kv_group_size`, `quantized_kv_start` args to `generate_step` / `speculative_generate_step` / `stream_generate` | `maybe_quantize_kv_cache(prompt_cache, quantized_kv_start, kv_group_size, kv_bits)` | ✅ verified | [generate.py](https://raw.githubusercontent.com/ml-explore/mlx-lm/main/mlx_lm/generate.py) |
| **LM Studio** (lmstudio-ai/mlx-engine) | Internal API: `kv_bits`, `kv_group_size`, `quantized_kv_start` (`ModelKit.__init__` / `_full_model_init` → `CacheWrapper`); UI label "KV Cache Quantization" with `KVQ Q8_0` values | Source also logs **"max_kv_size is ignored when using KV cache quantization"**. Exact LM Studio app JSON config key string **not verified**. Known bug: hybrid/Mamba models crash (`'MambaCache' object has no attribute 'offset'`) | ✅ verified for param names (source); ⚠️ for UI label and config key | [model_kit.py](https://raw.githubusercontent.com/lmstudio-ai/mlx-engine/main/mlx_engine/model_kit/model_kit.py), [cache_wrapper.py](https://raw.githubusercontent.com/lmstudio-ai/mlx-engine/main/mlx_engine/cache_wrapper.py), [mlx-engine#221](https://github.com/lmstudio-ai/mlx-engine/issues/221) |
| **oMLX** | `--kv-cache-quant <3\|6\|8>` (TurboQuant) | Daemon-level, set once at start, shared across models; requires oMLX v0.3.4+ for 3-bit, later/dev for 6 & 8. Surfaced through LLMKube as CRD field `turboQuantBits` (enum 3/6/8). Claimed "up to 67% memory reduction, ~7% overhead" | ✅ verified (third-party operator mapping to the oMLX CLI) | [LLMKube commit 68e0291](https://github.com/defilantech/LLMKube/commit/68e02916ab594625c33daa008b1d1b025362594c) |
| **arozanov/mlx-lm fork** (`feature/turboquant-kv-cache`) | `--kv-cache-quantization K,V` (e.g. `8,4`), `--quantized-kv-start N`, `--prompt-cache-dir PATH`, `--no-batch` | Ships `MixedQuantKVCache`; adds disk prompt-cache persistence + MoE/CacheList support | ✅ verified (fork's own README); ⚠️ not checked against fork source | [turboquant-mlx README](https://github.com/arozanov/turboquant-mlx/blob/main/README.md) |
| **rMLX** (Rust MLX engine, `rmlx serve`) | `--kv-quant <codec>` (e.g. `none`, `k8v4`, `k8v8`, `planar`, `k8vturbo3`, `tsym4`, `planar_k`), plus `--ctk`/`--ctv`, `--kv-preset`, `--paged-kv`; env `RMLX_KV_PAGE_SIZE`; per-request `kv_quant` field on the OpenAI route | Very large codec matrix; explicitly notes `mlx-lm.to_quantized` raises `NotImplementedError` for rotating caches and matches that behaviour | ✅ verified (repo docs) | [rMLX docs/KV_QUANT.md](https://raw.githubusercontent.com/Pushkinist/rMLX/refs/heads/main/docs/KV_QUANT.md) |
| **MLX Swift LM** (ml-explore/mlx-swift-lm) | PR #232 "add TurboQuant KV cache compression" | Status and Swift API/flag names **not fetched** | ❓ unverified — lead only | [mlx-swift-lm#232](https://github.com/ml-explore/mlx-swift-lm/pull/232) |
| **Ollama** | — | Runs MLX on Apple Silicon (v0.19 preview; v0.30 integrates llama.cpp too). **No user-facing KV-cache-quant flag found.** Ollama's `-mlx` tags are NVFP4 | ❓ unverified (absence of evidence only) | [Ollama MLX blog](https://ollama.com/blog/mlx), [DevelopersIO](https://dev.classmethod.jp/en/articles/apple-mlx-ollama-deep-dive/) |

---

## PART C — MLX vs GGUF / llama.cpp on Apple Silicon

### C1. Model size for equivalent quantization

| Model | MLX (4-bit) | GGUF | Source | Vendor/Indep | Marker |
|---|---|---|---|---|---|
| Falcon-H1 1.5B-Instruct | 0.88 GB (mlx-community) | 0.90 GB (tiiuae) | [#1847](https://github.com/ml-explore/mlx-lm/discussions/1847) | Independent community | ⚠️ |
| Falcon-H1 3B-Instruct | — (converted by author) | 1.76 GiB (Q4_K_M from same bf16) | #1847 | Independent community | ⚠️ |
| Granite-4.0-H 350M | 0.20 GB | 0.21 GB | #1847 | Independent community | ⚠️ |
| Granite-4.0-H 1B | 0.83 GB | 0.86 GB | #1847 | Independent community | ⚠️ |
| Granite-4.0-H Tiny 7B-A1B | 3.91 GB (lmstudio-community) | 4.25 GB (unsloth) | #1847 | Independent community | ⚠️ |
| Nemotron-3 Nano 4B | 2.24 GB | 2.84 GB | #1847 | Independent community | ⚠️ |
| Nemotron-3 Nano 30B-A3B | 17.8 GB | 24.6 GB (unsloth) | #1847 | Independent community | ⚠️ |
| gemma4 12B | 6.8 GB (NVFP4) | 7.6 GB (Q4_K_M) | [DevelopersIO](https://dev.classmethod.jp/en/articles/apple-mlx-ollama-deep-dive/) | Independent blogger | ✅ |
| Llama-3.3-70B | 39.71 GB | 42.52 GB | [LM Studio #258](https://github.com/lmstudio-ai/lmstudio-bug-tracker/issues/258) | Independent user (2024) | ⚠️ |

Bit-width caveat, stated by the #1847 author: "Q4_K_M and mlx 4-bit gs64 affine do not move the same bytes per token (about 4.8 vs 4.5 bits per weight), and the unsloth 30B-A3B GGUF is closer to 6.2." ⚠️

### C2. Single-request (batch-1) decode throughput

**Source A — mlx-lm Discussion #1847** (independent community benchmark; posted in the MLX project's own forum). **M4 Max, 128 GB, macOS 27.0**, batch 1, greedy, warm. MLX = median of 3 processes, 4-bit gs64; llama.cpp = `llama-bench` mean ± std, `-r 3`, Metal build b8680, Q4_K_M. MLX numbers are `mlx_lm generate`'s own `Generation:` figure on 256 tokens; llama.cpp is `tg256`. ⚠️ **Independent third party, not a vendor blog** (author maintains `john-rocky/apple-silicon-llm-bench`), though hosted on the MLX repo.

| Model | mlx-lm 0.31.3 / mlx 0.32.2 (4-bit) | llama.cpp Metal b8680 (Q4_K_M) | Apple Core AI |
|---|---|---|---|
| Falcon-H1 1.5B-Instruct | **300.0** | 148.6 ± 0.3 | — |
| Falcon-H1 3B-Instruct | **167.9** | 86.1 ± 0.4 | — |
| Falcon-H1 7B-Instruct | (not measured) | 52.9 ± 0.4 | — |
| Granite-4.0-H 350M | **521.7** | 282.1 ± 1.0 | 191.1 (fp16) |
| Granite-4.0-H 1B | **275.8** | 144.0 ± 2.0 | 136.5 (int8) |
| Granite-4.0-H Tiny 7B-A1B | **202.2** | 117.3 ± 0.9 | — |
| Nemotron-3 Nano 4B | **176.8** | 88.4 ± 0.2 | 85.2 (int8 head) |
| Nemotron-3 Nano 30B-A3B | **159.7** | 86.2 ± 0.5 | — |

Direction: MLX ~1.7–2.0x llama.cpp on these hybrid Mamba-2 architectures. Author also flags a **quality** issue: Falcon-H1 at MLX 4-bit repeats itself / fails a bubble-sort prompt, while 8-bit and bf16 are coherent and llama.cpp Q4_K_M answers cleanly — "not a diagnosis". ⚠️

**Source B — DevelopersIO (Classmethod), independent blogger.** **M2 Max, 32 GB, macOS 15.7.4, Ollama 0.30.10**, gemma4 12B, ~7,000-token prefill, ~300-token decode, median of 3, `think:false`. ✅ (independent third party, but measured *through Ollama*, not raw runtimes)

| Metric | GGUF Q4_K_M | MLX NVFP4 |
|---|---|---|
| Prefill (~7K tokens) | **~156 tok/s** (146–159) | ~114 tok/s (112–117) → GGUF ~1.4x |
| Decode (~300 tok) | ~13.7 tok/s | **~14.0 tok/s** (~2% MLX edge) |
| Memory (`ollama ps`) | 8.1 GB | 6.8 GB |
| Default context | 65,536 | 262,144 |

Author's own caveat: `gemma4:12b-mlx` and `gemma4:12b-nvfp4` are the **same artifact** (same ID), so MLX-vs-quant format cannot be disentangled here; the GGUF build is multimodal (adds a CLIP projector). Author explicitly counters the **vendor** Ollama blog claim ("NVFP4 generates ~20% faster than q4_K_M", M5/Qwen3.5-35B-A3B) as not apples-to-apples.

**Source C — academic, independent.** arXiv 2511.05502, "Production-Grade Local LLM Inference on Apple Silicon: A Comparative Study of MLX, MLC-LLM, Ollama, llama.cpp, and PyTorch MPS" (submitted 2025-10-09). **Mac Studio M2 Ultra, 192 GB**, Qwen-2.5 family, prompts up to 100,000 tokens. Findings: **MLX achieves the highest sustained generation throughput**; MLC-LLM lower TTFT for moderate prompts; **llama.cpp highly efficient for lightweight single-stream use**; Ollama lags throughput and TTFT; PyTorch MPS memory-limited. ⚠️ **Abstract only** — I could not fetch the full text for numbers (arXiv has no HTML for v1; ar5iv blocked as a cross-origin redirect).

### C3. Long-context behaviour

**Contra Collective, "Long Context Decode on Apple Silicon" (2026-06-19)** — ⚠️ **third-party consultancy marketing blog, no repo/scripts released**, older stack (llama.cpp b4321 Metal flash-attn; MLX 0.21 / mlx-lm 0.20.4 server), same 4-bit weights (Q4_K_M / mlx-community Q4), LongBench v2 filler + 256 generated tokens, median of 3, variance <4%.

Decode tok/s — Llama 3.3 **8B** Q4:

| Context | M5 Pro llama.cpp | M5 Pro MLX | M5 Max llama.cpp | M5 Max MLX |
|---|---|---|---|---|
| 2K | 38.4 | 41.2 | 62.7 | 68.9 |
| 32K | 31.8 | 34.6 | 54.1 | 59.7 |
| 128K | 19.2 | 22.4 | 39.8 | 44.6 |
| 512K | 8.6 | 11.3 | 22.7 | 26.4 |
| 1M | 4.1 | 5.9 | 12.8 | 15.7 |

Decode tok/s — Llama 3.3 **70B** Q4:

| Context | M5 Pro llama.cpp | M5 Pro MLX | M5 Max llama.cpp | M5 Max MLX |
|---|---|---|---|---|
| 2K | 6.2 | 7.4 | 11.8 | 13.6 |
| 32K | 5.1 | 6.2 | 10.2 | 11.9 |
| 128K | 3.4 | 4.1 | 7.8 | 9.2 |
| 512K | OOM | OOM | 4.6 | 5.8 |
| 1M | OOM | OOM | 2.7 | 3.4 |

Claims: MLX leads 10–25% at every context length/model; gap widest (25–35%) at 512K–1M; 8B MLX M5 Max falls 68.9→15.7 tok/s (4.4x) from 2K→1M. Hardware: M5 Pro 48 GB, M5 Max 96 GB. FAQ claim: **Q8 KV vs FP16 buys ~1.8x throughput at ≥512K; Q4 KV another ~1.4x but degrades quality on retrieval-heavy work.** ⚠️ Not independently reproducible from the post.

**Counter-evidence / disagreement.** These sources do not agree on direction:
- #1847 (M4 Max) and Contra (M5 Pro/Max) both favour MLX on decode.
- DevelopersIO (M2 Max, via Ollama) found **GGUF 1.4x faster on prefill** and decode essentially tied.
- LM Studio #258 (M4 Max 128 GB, Dec 2024) user found **GGUF faster than MLX** on Llama-3.3-70B (11:48 vs 8:49 first run; MLX 5:02 after a newer build). Maintainer closed it as a measurement error (durations, not tok/s). ⚠️ old, anecdotal.
- Harness matters more than the label: Ollama/LM Studio results are not raw `mlx_lm` vs `llama-bench`.

### C4. Batched / parallel throughput — **EVIDENCE IS THIN** ❓

I could not fetch a credible independent MLX-vs-GGUF batched-throughput comparison. What surfaced but was **not fetched/verified**:
- Contra Collective's referenced "continuous batching throughput teardown" post (2026) — ❓ not fetched.
- A HuggingFace model-card commit claiming MLX batching "2.6x standalone / 1.74x live-serve" — ❓ not fetched, single-author model card.
- The only structural, verified fact here: upstream mlx-lm **disables batching whenever `--kv-bits` is set** (`_is_batchable` returns False; SERVER.md states requests are served one at a time) ✅. Issue #1583 is exactly about the missing batched quantized cache. So "quantized KV **and** batched serving" is not available upstream as of 2026-09-19.
- llama.cpp/Ollama clearly batches; a 2026 arXiv paper on Apple Silicon inference at scale (arXiv 2601.19139) exists but was not fetched ❓.

### C5. Model load time

**Contra Collective, "Local LLM Cold Start" (2026-07-03)** — same ⚠️ consultancy-blog caveat. **M5 Max, 128 GB**, 4-bit, context 4096, batch 1, median of 5, file cache purged for cold.

| Model (Q4) | Runtime | On disk | Cold load | Warm load | Cold 1st token | Warm 1st token |
|---|---|---|---|---|---|---|
| 8B | MLX | 4.4 GB | 1.8 s | 0.3 s | 2.1 s | 0.5 s |
| 8B | llama.cpp | 4.6 GB | **0.4 s** | 0.2 s | **1.0 s** | 0.6 s |
| 32B | MLX | 17 GB | 6.9 s | 0.9 s | 7.5 s | 1.3 s |
| 32B | llama.cpp | 18 GB | **1.1 s** | 0.5 s | **2.4 s** | 1.6 s |
| 70B | MLX | 38 GB | 15.2 s | 2.1 s | 16.4 s | 3.0 s |
| 70B | llama.cpp | 40 GB | **2.3 s** | 1.4 s | **4.7 s** | 3.8 s |

Mechanism given: llama.cpp `mmap`s the GGUF and faults pages in lazily (defers cost into first prefill); MLX eagerly reads/deserializes safetensors. Warm starts converge. ⚠️ No scripts/repo; not independently reproduced. This is the **only** load-time comparison I found — treat as single-source.

### C6. KV-cache quantization: memory and speed effects (bonus, directly relevant)

| Claim | Numbers | Hardware | Source | Marker |
|---|---|---|---|---|
| mlx-lm cache size fp16→4-bit | 786.6 MB → 226.5 MB | Qwen1.5-0.5B-Chat-4bit, author's machine | [PR #1832](https://github.com/ml-explore/mlx-lm/pull/1832) (merged) | ⚠️ author-reported |
| Peak memory, 4-bit KV vs fp16 | prefill 2048: **+13.2% (worse)**; 1024: −16.8%; 512: −24.0%; 256: −26.2% | same | PR #1832 | ⚠️ author-reported — key nuance: quantization can *increase* peak memory at large prefill steps |
| Max context on 24 GB M4 Pro | FP16 ~33K (~16 GB KV @32K); Q8 ~40K (~8 GB); Q4 ~70K+ (~4 GB) | Ornith-1.5-9B 4-bit, M4 Pro 24 GB | [PR #1854](https://github.com/ml-explore/mlx-lm/pull/1854) (closed, unmerged) | ⚠️ author-reported |
| K8/V4 decode crossover | 1K: 20.4→19.3 t/s; 16K: 12.5→13.3 t/s (1.06x); 32K: 8.0→8.1 vs Q4 10.8 | Llama-3-8B-Instruct-4bit, "Apple Silicon, 32 GB" | [PR #1074](https://github.com/ml-explore/mlx-lm/pull/1074) comment | ⚠️ author-reported, PR closed unmerged |
| K8/V4 memory saving | 62% of FP16 (38% saving); 128K: 16.0 GB → 10.0 GB | same | PR #1074 | ⚠️ author-reported |
| K↔V asymmetry | "K quantization destroys greedy decode at 4-bit and below (even MLX's native `kv_bits=4`)"; "V quantization is safe at 3-bit" → K8/V4 recommended | Qwen2.5-7B, 32K | [turboquant-mlx README](https://github.com/arozanov/turboquant-mlx/blob/main/README.md) | ⚠️ project author |
| That project's own numbers | fp16: 6.21 GB / 35.75 t/s; K8+V4: 5.08 GB (−18%) / 25.84 t/s; K8+V2: 4.97 GB / 25.52 t/s | Qwen2.5-7B 32K | same | ⚠️ project author (note: *slower*, not faster) |
| KVSplit tuple API | `bits=(8,4)` accepted by `QuantizedKVCache`/`QuantizedRotatingKVCache` on PR #1074; independent reviewer on M2 Ultra 128 GB confirmed K8/V4 architecture, negligible gain from Hadamard rotation at K8, ~1.8x latency overhead with `mx.compile` fusion | M2 Ultra 128 GB | PR #1074 comments | ⚠️ reviewer-reported |

Gap: no independent, reproducible KV-quant vs FP16 quality/perf benchmark (perplexity, long-context retrieval) was found for upstream mlx-lm. All KV-quant numbers above are author-reported in PRs or project READMEs.

---

## Bottom line

1. **Upstream mlx-lm has quantized KV cache**, but only for the **non-batched / single-request** path: `QuantizedKVCache(group_size=64, bits=8)`, `KVCache.to_quantized(group_size=64, bits=4)`, and `generate_step(kv_bits=..., kv_group_size=64, quantized_kv_start=5000)`. The server exposes it since **2026-09-09** (PR #1832 merged) as `--kv-bits` / `--kv-group-size` / `--quantized-kv-start`, and **disables batching** when set. ✅
2. **`RotatingKVCache.to_quantized()` and `BatchRotatingKVCache.to_quantized()` are still `NotImplementedError` stubs** on `main`. So `--max-kv-size` + `--kv-bits` and `BatchGenerator` + `kv_bits` remain broken/unreachable. Issue #1583 is **open**; the two PRs that fixed it (#1074, #1584) plus the duplicate server PR (#1854) were all **closed unmerged**, two of them by a maintainer explicitly citing review-capacity limits. ✅
3. **The "private fork" era is over for the basics** — the same three flag names are now upstream, and LM Studio's mlx-engine uses the identical `kv_bits`/`kv_group_size`/`quantized_kv_start` names. Genuinely different private implementations exist for *other codecs*: oMLX `--kv-cache-quant` (TurboQuant 3/6/8-bit), arozanov's fork `--kv-cache-quantization K,V`, and rMLX's `--kv-quant` codec matrix. ✅ / ⚠️
4. **MLX-vs-GGUF is genuinely contested and hardware-dependent.** Independent batch-1 evidence favours MLX on M4 Max for hybrid Mamba-2 models (~1.7–2x) but the M2 Max/Ollama test favours GGUF by ~1.4x on prefill with decode tied; a single consultancy blog claims MLX +10–25% at long context. **No independently reproducible, current (2026) benchmark covering batch>1 throughput exists in what I could fetch** — batched throughput is the biggest evidence gap, followed by model load time (single consultancy source only). ⚠️/❓

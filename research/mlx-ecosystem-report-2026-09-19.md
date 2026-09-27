# MLX on Apple Silicon for a 24/7 local AI gateway — 2026-09-19

**Headline.** MLX core is healthy and no longer Apple-only (Linux CUDA/CPU wheels exist). The **Python layer is the weak link**: `mlx-lm`'s newest release (v0.31.3, 2026-04-22) is ~5 months behind core (v0.32.2) despite an active `main`, and its KV quantization is *shipped but crippled* — `--kv-bits` silently disables continuous batching, sliding-window caches still `raise NotImplementedError`, and the server has no model pool. The **Swift layer (`mlx-swift-lm`) has overtaken Python** on KV compression. **Don't build on `mlx_lm.server`: drive `oMLX` over HTTP for resident+on-demand multi-model serving, and give the long-context/concurrent role to llama.cpp — the only stack where quantized KV and batching coexist.**

## Q1 — Core + language layers

| Component | Latest | Date | Stars | Maintained | Lag vs core |
|---|---|---|---|---|---|
| `mlx` (core) | **v0.32.2** | 2026-08-25 | 28,478 | ✅ 2026-09-17 | — |
| `mlx-lm` | **v0.31.3** | 2026-04-22 | 7,065 | ✅ 2026-09-18 | **~5 mo** |
| `mlx-swift` | **0.31.6** | 2026-07-02 | 2,035 | ✅ 2026-09-17 | ~2 mo |
| `mlx-swift-lm` (new) | **3.31.4** | 2026-06-30 | — | ✅ 2026-09-15 | Swift 3.x |
| `mlx-c` | **v0.6.0** | 2026-03-20 | 243 | ✅ 2026-09-14 | **~6 mo** |
| `mlx-rs` | **0.32.0** | 2026-09-12 | 373 | ✅ 2026-09-15 | current |
| `mlx-vlm` | 0.7.1 | 2026-09-14 | — | ✅ | current |

✅ Versions/dates from GitHub release feeds + PyPI JSON.

**Drift is bidirectional.** Downward: `mlx-lm`/`mlx-swift`/`mlx-c` all trail core, and server fixes (e.g. "Fix mlx_lm.server model-swap leak", 2026-09-11) are **unreleased**. Upward: `MLXLMCommon/MLXLLM/MLXVLM/MLXEmbedders` moved into a new `mlx-swift-lm` repo whose `main` is a **breaking 3.x** line ✅.

**MLX is no longer Apple-only** ✅: 0.32.2 offers `pip install mlx[cuda]` (Linux CUDA) and `mlx[cpu]`; the API has `mlx.core.cuda` and `fast.cuda_kernel`. But `mlx-lm`'s README still says "on Apple silicon", so the *inference* layer stays Apple-first.

## Q2 — `mlx-c` and bindings

| Aspect | Finding |
|---|---|
| Maturity | ✅ v0.6.0 (2026-03-20), 9 open issues, **pre-1.0** (0.2→0.6 since 2025-04); no stability promise |
| Core coverage | ✅ Broad: 249 decls in `ops.h`; `io/memory/distributed/export/linalg/fft/random.h` present |
| `mx.fast` | ✅ `rms_norm`, `layer_norm`, `rope`, `sdpa`, `cross_entropy`, **plus Metal *and* CUDA kernel authoring** |
| Autograd | ✅ `value_and_grad`, `vjp`, `jvp`, `closure_custom_vmap`, `checkpoint`; `compile.h` |
| **Missing** | ✅ **No `nn.h`/`optimizers.h` (404)** — each language rebuilds those layers |
| Users | ✅ `mlx-swift` ("uses MLX C"); ✅ `mlx-rs` (`mlx-sys =0.6.0`) |

`mlx-c` is a **thin-but-honest core binding, not a thin API surface**; the gap is the high-level layer. Layers patch **ahead of mlx-c tags**: `mlx-rs` 0.32.0 pins "mlx-c `c74db530` (v0.6.0 **plus seven commits**), MLX 0.32.2" ✅.

## Q3 — `mlx-rs`

**Not a toy, not production-grade.** ✅ v0.32.0 (2026-09-12), 373★, 73 open issues; **27 reverse deps, all pinned `^0.25`** — ecosystem hasn't adopted current.

Real inference **is** now there ✅: safetensors load/save; new `io` module for GGUF ("Q4_0, Q4_1 and Q8_0 tensors load as quantized triplets"); `quantization` + `nn` modules; commits 2026-09-13→15 add "generation, sampling, and prompt-cache reuse" and "real checkpoints". So yes — load safetensors/GGUF, run a quantized LLM, generate tokens.

Maturity flags ✅: `Compiled` is `!Send`; streams/compiled closures are **thread-affine, cannot move across threads**; self-reported **data racing** in parallel tests; autodiff closures can **segfault**; **docs.rs fails to build every version**; 0.32.0 was breaking. **No production users found** ❓.

## Q4 — Serving layer options on macOS

| Server | Lang | Multi-model | Pin + LRU/TTL | OpenAI | Anthropic | Quant KV |
|---|---|---|---|---|---|---|
| **oMLX** (jundot) | Python + SwiftUI | ✅ LLM+VLM+embed+rerank | ✅ **LRU + pinning + per-model TTL** | ✅ | ✅ `/v1/messages` | ❌ none found |
| LM Studio `mlx-engine` | Python | ⚠️ app-level | ✅ JIT `ttl` 60 min + Auto-Evict; `lms load` = resident | ✅ | ✅ | ❓ |
| Ollama (MLX engine) | Go | ✅ `OLLAMA_MAX_LOADED_MODELS` (3) | ✅ `keep_alive` (negative = pin) | ✅ | ✅ | ✅ `OLLAMA_KV_CACHE_TYPE` ⚠️ MLX applicability unstated |
| `mlx_lm.server` | Python | ❌ one model, swaps | ❌ (LRU = *prompt* cache only) | ✅ | ❌ no route | ✅ `--kv-bits` (disables batching) |
| mistral.rs | Rust | ✅ TOML | ❌ manual unload/reload/status | ✅ | ✅ | ✅ `--pa-cache-type` ⚠️ off on Metal |
| llama.cpp Metal | C/C++ | ✅ router `--models-max` (4) | ⚠️ partial; ❌ no TTL/LRU | ✅ | ✅ | ✅ `-ctk`/`-ctv` |

**`oMLX` is the only MLX server natively implementing your requirement**: "**LRU eviction**…", "**Model pinning**: keep them always loaded", "**Per-model TTL**", plus vLLM-inspired hot/SSD block KV cache with prefix sharing and CoW. Caveat: `--pin` is only in a docstring, not argparse (admin-panel driven) ✅. **mistral.rs has no MLX support at all** ✅ (Metal/Candle).

## Q5 — KV quantization: merged, but crippled

**Your four references aren't what they appear** ✅: **#1583 is an ISSUE, still OPEN**; **#1074 is a PR, CLOSED unmerged**; **#1832 is a PR, MERGED 2026-09-09**; **#1854 is a PR, CLOSED** in favour of #1832.

**Merged upstream: YES.** ✅ `QuantizedKVCache(group_size=64, bits=8)`, `KVCache.to_quantized(bits=4)`, `generate_step(..., kv_bits, kv_group_size, quantized_kv_start=5000)` — **shipped in released v0.31.3** (confirmed in the `v0.31.3` tag), not just `main`.

**Three hard limits** ✅:
1. `RotatingKVCache`/`BatchRotatingKVCache.to_quantized()` → `raise NotImplementedError(...NYI)`; `BatchKVCache` has none at all.
2. `_is_batchable()` returns `... and self.cli_args.kv_bits is None` → **`--kv-bits` turns off continuous batching**. `SERVER.md`: "A quantized KV cache does not support batching."
3. `--max-kv-size` + `--kv-bits` are effectively mutually exclusive.

**Server flags are main-only**: `--kv-bits`, `--kv-group-size`, `--quantized-kv-start` are in `main`, **not** in released v0.31.3 ✅.

**Other implementations**: `mlx-swift-lm` exposes `kvBits`/`kvGroupSize`/`quantizedKVStart` **plus a `kvScheme` selector with asymmetric K/V** — `affine4/8`, `turbo0v4` (FP16 K + 4-bit V), `turbo8v3` ("recommended default"), `varn4v2` (4-bit K + 2-bit V) ✅. **This is the K/V-split capability Python lacks.** `oMLX` documents no KV quantization ✅ (tiered paging, not lower precision).

**llama.cpp (premise correction)**: master allows the **same nine types for K and V** — `f32,f16,bf16,q8_0,q4_0,q4_1,iq4_nl,q5_0,q5_1` via `-ctk`/`-ctv` (default f16), independently settable ✅. V is **not** limited to f16/q8_0/q4_0 anymore.

**Quality**: no independent perplexity/retrieval benchmark exists for upstream mlx-lm KV quant ❓ — all figures are author-reported. ⚠️ Cache 786.6→226.5 MB (fp16→4-bit) but **peak prefill memory got worse at 2048** (+13.2%): quantized attention is unfused and keeps a `prefill_step_size × context` score matrix. ⚠️ `turboquant-mlx` finds K-quant destroys greedy decode at ≤4-bit; recommends K8/V4.

## Q6 — MLX vs GGUF, 2026

**Independent evidence is genuinely contested — not settled.**

| Dimension | Finding | Source quality |
|---|---|---|
| Size ~4-bit | MLX smaller: Nemotron-30B-A3B 17.8 vs 24.6 GB | ⚠️ community bench, M4 Max — **not equal bits/weight** (4.5 vs 4.8) |
| Batch-1 decode | **MLX ~1.7–2×**: 159.7 vs 86.2; Granite-1B 275.8 vs 144.0 tok/s | ⚠️ independent, M4 Max, 2026-09 |
| Prefill | **GGUF ~1.4×**: 156 vs 114 tok/s; decode tied (~13.7 vs 14.0) | ⚠️ independent blogger, **M2 Max** via Ollama |
| Long context | MLX +10–25%; Q8 KV ≈1.8× throughput ≥512K; Q4 KV ≈1.4× but degrades retrieval | ⚠️ **consultancy blog, old stack (MLX 0.21)** — weakest |
| Decode at depth | 150.8 tok/s @4K → 129.7 @8K (−14%) | ⚠️ M5 Max, Qwen3-4B-4bit |
| **Batched throughput** | **No credible independent source** | ❓ gap |
| Load time | llama.cpp mmap 0.4 s vs MLX 1.8 s cold (8B); warm converges | ❓ single blog |
| Academic | MLX highest sustained throughput | ⚠️ arXiv 2511.05502, M2 Ultra, abstract only |

**Vendor flag:** Ollama's "NVFP4 ~20% faster than q4_K_M" is vendor-sourced and was **rebutted by an independent blogger** who measured GGUF *faster*. The popular "MLX 1.7–2×" figure is independent but covers hybrid Mamba-2, not dense Transformers, and its author flags an MLX Falcon-H1 4-bit **coherence problem**. Treat batch-1 as MLX-favouring, prefill as GGUF-favouring, **batched throughput as unproven**.

## Q7 — What MLX cannot do today

1. **KV quant + batching are mutually exclusive** in mlx-lm ✅.
2. **Rotating/sliding-window caches can't quantize** — `NotImplementedError` stubs; **#1583 open** after fix PRs were mass-closed for "review capacity" ✅.
3. **No K/V-split bits in Python** (Swift has them) ✅.
4. **No model pool**: `mlx_lm.server` loads one model and swaps; no pin/TTL/max-models; its own code warns it "is not recommended for production as it only implements basic security checks" ✅.
5. **Release cadence**: `mlx-lm` v0.31.3 (Apr) vs core v0.32.2 (Aug); server fixes unreleased ✅.
6. **Unfused quantized attention** reverses memory savings at prefill 2048 ✅⚠️.
7. **`mlx-c` pre-1.0** with no `nn`/`optimizers` headers ✅.
8. **`mlx-rs`**: pre-1.0, 373★, no production users, broken docs.rs, thread-affine `!Send` compiled closures — hostile to a multi-threaded gateway — plus self-reported data races ✅.
9. **Observability**: no per-model metrics/Prometheus in the MLX Python server ✅.
10. **No ANE use** (GPU/Metal only); **Apple Core AI** (macOS 27) is a competing runtime ✅.
11. ~~No Linux/Windows~~ — **outdated**: core MLX ships Linux CUDA/CPU wheels ✅.

## Q8 — Recommendation

**Take (b) external MLX server over HTTP for the MLX roles, plus (c) llama.cpp for long-context/high-concurrency. Do not embed MLX in-process (a) if the gateway is Rust.**

| Role | Choice | Decisive constraint |
|---|---|---|
| 2–3 **resident/pinned** chat + tool-calling | **oMLX** (HTTP) | Only MLX server with native pin + per-model TTL + LRU + OpenAI **and** Anthropic `/v1/messages`; resident/on-demand semantics for free |
| **On-demand** small/VLM | **oMLX** (same pool) | Same eviction policy; one unified memory pool + prefix/SSD cache |
| **Long-context / RAG** | **llama.cpp**, `-ctk q8_0 -ctv q8_0` | **Decisive constraint of this report:** quantized KV and batching are mutually exclusive in MLX Python; llama.cpp gives *both* independent K/V types *and* batching |
| **Embeddings / rerank** | **oMLX** (`/v1/embeddings`, `/v1/rerank`) | Already in the MLX pool; avoids a second runtime |
| **In-process** (gateway Python/Swift only) | `mlx-lm` git `main` + own pool, or `mlx-swift-lm` | Only if you accept the batching-vs-KV-quant trade and pin `main` |
| **Avoid** | `mlx-rs` for a 24/7 Rust gateway | Thread-affine `!Send` closures + pre-1.0 churn + no production users |

**Decisive constraint:** *`--kv-bits` disables batching, and sliding-window KV quantization is unimplemented* — MLX cannot today serve 3–5 long-context models concurrently with quantized caches. Split the fleet: MLX/oMLX for pinned high-value moderate-context models; llama.cpp for long-context concurrency. Revisit when #1583 closes and K/V-split lands in Python.

## Sources

Core/layers: [mlx v0.32.2](https://github.com/ml-explore/mlx/releases/tag/v0.32.2) · [install docs](https://ml-explore.github.io/mlx/build/html/install.html) · [PyPI mlx-lm](https://pypi.org/pypi/mlx-lm/json) · [mlx-swift-lm](https://github.com/ml-explore/mlx-swift-lm) · [mlx-c](https://github.com/ml-explore/mlx-c)
Bindings: [ops.h](https://raw.githubusercontent.com/ml-explore/mlx-c/main/mlx/c/ops.h) · [mlx-rs docs](https://oxiglade.github.io/mlx-rs/mlx_rs/) · [mlx-rs CHANGELOG](https://docs.rs/crate/mlx-rs/latest/source/CHANGELOG.md) · [crates.io](https://crates.io/crates/mlx-rs)
Serving: [oMLX](https://github.com/jundot/omlx) · [mlx-engine](https://github.com/lmstudio-ai/mlx-engine) · [Ollama MLX](https://ollama.com/blog/mlx-performance) · [mlx-lm server.py](https://raw.githubusercontent.com/ml-explore/mlx-lm/main/mlx_lm/server.py) · [mistral.rs](https://github.com/EricLBuehler/mistral.rs) · [llama.cpp server](https://raw.githubusercontent.com/ggml-org/llama.cpp/master/tools/server/README.md)
KV quant: [#1583](https://github.com/ml-explore/mlx-lm/issues/1583) · [#1074](https://github.com/ml-explore/mlx-lm/pull/1074) · [#1832](https://github.com/ml-explore/mlx-lm/pull/1832) · [#1854](https://github.com/ml-explore/mlx-lm/pull/1854) · [cache.py](https://raw.githubusercontent.com/ml-explore/mlx-lm/main/mlx_lm/models/cache.py) · [Evaluate.swift](https://raw.githubusercontent.com/ml-explore/mlx-swift-lm/main/Libraries/MLXLMCommon/Evaluate.swift)
Benchmarks: [apple-silicon-llm-bench](https://github.com/john-rocky/apple-silicon-llm-bench) · [mlx-lm #1847](https://github.com/ml-explore/mlx-lm/discussions/1847) · [ax-engine](https://github.com/defai-digital/ax-engine) · [arXiv 2511.05502](https://arxiv.org/abs/2511.05502)

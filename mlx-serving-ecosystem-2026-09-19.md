# MLX / Apple-Silicon local-inference serving ecosystem — evidence report

**Research date: 2026-09-19.** Every claim below was checked against a page I actually fetched (GitHub
raw READMEs, source files, official docs, commit/release Atom feeds). Markers:

- ✅ **verified** — exact text is on the fetched official page (quoted).
- ⚠️ **secondary** — fetched, but from a non-primary surface (commit log, app-level docs for a different
  repo, issue tracker, docs of a *different* project), or only implied.
- ❓ **unverified** — could not confirm from fetched pages. Explicitly flagged, never inferred.

Absence claims ("no flag exists") are verified only against the specific files/pages I fetched; that scope
is named in each case.

---

## 0. Summary matrix (one row per serving option)

| # | Server | Multiple models at once? | Pinned + on-demand LRU/TTL eviction? | OpenAI `/v1/chat/completions` | Anthropic `/v1/messages` | Quantized KV cache | Language | Last commit | Latest release |
|---|--------|--------------------------|--------------------------------------|-------------------------------|--------------------------|--------------------|----------|-------------|----------------|
| 1 | **oMLX** (`jundot/omlx`) | ✅ multi-model server, LLM+VLM+embed+rerank in one process | ✅ LRU eviction + model pinning + per-model TTL (admin panel/settings; `--pin` appears only in a docstring, **not** in argparse) | ✅ | ✅ | ❌ no KV-precision flag found in README/`cli.py`/`config.py` (GDN state dtype only) ❓ | Python (server) + Swift/SwiftUI (macOS app) | ✅ 2026-09-18T06:39Z | ✅ `v0.7.0.dev4`, 2026-09-18 (dev pre-release) |
| 2 | **LM Studio `mlx-engine`** (engine repo) | ⚠️ TTL/auto-evict/pinning is an **LM Studio app** feature; the engine repo itself documents no model pool | ✅ app-level: JIT load + `ttl` + Auto-Evict; non-JIT loads are "not affected" = resident/pinned | ✅ (LM Studio server) | ✅ (LM Studio server) | ❓ not documented; open feature request issue #31 | Python (engine); LM Studio app is separate | ✅ 2026-08-21T15:39Z | ✅ **no releases published** (empty release feed) |
| 3 | **Ollama MLX path** | ✅ general server: `OLLAMA_MAX_LOADED_MODELS` (default 3× GPUs / 3 CPU) | ✅ `keep_alive` / `OLLAMA_KEEP_ALIVE` TTL, negative = keep loaded (pin); older idle models are unloaded to make room | ✅ | ✅ | ✅ `OLLAMA_KV_CACHE_TYPE` (f16/q8_0/q4_0) — ⚠️ MLX applicability not stated | Go (`go.mod`; `mlx`/`mlxrunner` packages) | ✅ 2026-09-19T00:15Z | ✅ `v0.34.3-rc1` 2026-09-19; stable `v0.34.2` 2026-09-17 |
| 4 | **`mlx_lm.server`** (mlx-lm built-in) | ❌ one model resident; requests swap it (LRU **prompt** cache only, not a model pool) | ❌ no pin/TTL/max-models flags; explicit model swap unloads the previous one | ✅ | ❌ not routed (`do_POST` map has no `/v1/messages`) | ✅ `--kv-bits`, `--kv-group-size`, `--quantized-kv-start` | Python | ✅ 2026-09-18T15:27Z | ✅ `v0.31.3`, 2026-04-22 (large release lag) |
| 5 | **mistral.rs** | ✅ TOML multi-model, one engine per model | ❌ manual `POST /v1/models/unload|reload|status`; no pin/TTL/LRU/idle-timeout documented | ✅ | ✅ (`/v1/messages` + `/v1/messages/count_tokens`) | ✅ `--pa-cache-type` ("KV cache quantization type") — ⚠️ PagedAttention is `auto` = **disabled on Metal**; accepted types on Metal ❓ | Rust | ✅ 2026-09-08T03:18Z | ✅ `v0.9.3`, 2026-09-08 |
| 6 | **llama.cpp on Metal** | ✅ router mode: `--models-max` (default 4, 0=unlimited) + `--models-autoload` | ⚠️ partial: preset `load-on-startup` = resident/pinned; `stop-timeout` after requested unload; **no TTL/LRU/auto-evict flag documented** | ✅ | ✅ (+ `/v1/messages/count_tokens`) | ✅ `-ctk/--cache-type-k`, `-ctv/--cache-type-v` — both accept f32, f16, bf16, q8_0, q4_0, q4_1, iq4_nl, q5_0, q5_1 | C/C++ | ✅ 2026-09-19T16:16Z | ✅ build tag `b11053`, 2026-09-19T13:26Z (nightly tags) |

**Headline correction to the premise:** the common belief that llama.cpp's V cache is limited to
`f16/q8_0/q4_0` is **outdated**. Current master lists the *same nine* types for K and V
(`f32, f16, bf16, q8_0, q4_0, q4_1, iq4_nl, q5_0, q5_1`).

---

## 1. oMLX (`jundot/omlx`)

Source: <https://raw.githubusercontent.com/jundot/omlx/main/README.md> ·
<https://raw.githubusercontent.com/jundot/omlx/main/omlx/cli.py> ·
<https://raw.githubusercontent.com/jundot/omlx/main/omlx/config.py> ·
<https://raw.githubusercontent.com/jundot/omlx/main/pyproject.toml> ·
<https://github.com/jundot/omlx/commits/main.atom> ·
<https://github.com/jundot/omlx/releases.atom>

**1a. Multiple models + pin + LRU/TTL — ✅ verified**
- README, "Multi-Model Serving": *"**LRU eviction**: Least-recently-used models are evicted automatically when memory runs low."* / *"**Model pinning**: Pin frequently used models to keep them always loaded."* / *"**Per-model TTL**: Set an idle timeout per model to auto-unload after a period of inactivity."* / *"**Process memory enforcement**: Total memory limit (default: system RAM - 8GB) prevents system-wide OOM."*
- README architecture block: `EnginePool (multi-model, LRU eviction, TTL, manual load/unload)` and `ProcessMemoryEnforcer (total memory limit, TTL checks)`.
- ⚠️ **Exact flag caveat:** no `--pin` / `--ttl` / `--max-models` argument exists in the argparse `serve` parser in `omlx/cli.py` (flags present: `--model-dir`, `--max-concurrent-requests`, `--memory-guard`, `--paged-ssd-cache-dir`, `--hot-cache-max-size`, `--initial-cache-blocks`, `--no-cache`, …). `--pin` appears **only** in the module docstring example (`omlx serve --model-dir /path/to/models --pin llama-3b,qwen-7b`), and the code says: *"Note: pinned_models and default_model are managed via admin page (model_settings.json)"*. So pin/TTL are real features but configured in the admin panel/settings, not via CLI in the fetched revision.

**1b. HTTP APIs — ✅ verified**
- README "API Compatibility": *"Drop-in replacement for OpenAI and Anthropic APIs."* Table lists `POST /v1/chat/completions`, `POST /v1/completions`, `POST /v1/messages` — *"Anthropic Messages API"*, `POST /v1/embeddings`, `POST /v1/rerank`, `GET /v1/models`.

**1c. Quantized KV cache — ❌ not found (scope-limited) / ❓**
- No `--kv-bits`, `--cache-type-k`, or equivalent in the README CLI section, in `omlx/cli.py`'s argparse, or in `omlx/config.py`'s `SchedulerConfig` / `PagedSSDCacheConfig`. The cache flags that exist (`--paged-ssd-cache-dir`, `--hot-cache-max-size`, `--hot-cache-write-through`, `--no-cache`) are about KV *reuse tiers*, not precision.
- ⚠️ Related but **not** KV-cache quantization: `config.py` validates `OMLX_GDN_SIDECAR_STATE_DTYPE` ∈ `{fp32, bf16, int8, rht_int8, rht_int16}` — that is hybrid GDN *recurrent-state sidecar* storage.
- ❓ Could not rule out an undocumented per-model admin-panel setting (admin UI not fetched).

**1d. Language — ✅** Python package (`pyproject.toml`: `name = "omlx"`, deps `fastapi`, `uvicorn`, `mlx==0.32.2`, `mlx-lm` git pin; entry point `omlx = "omlx.cli:main"`); macOS menubar app is *"Native Swift / SwiftUI menubar app (not Electron)"* (README).

**1e. Maintenance — ✅ active.** Last commit `14194fe` "chore: bump version to 0.7.0.dev4", 2026-09-18T06:39Z (<https://github.com/jundot/omlx/commits/main.atom>). Latest release `v0.7.0.dev4`, published 2026-09-18T11:29Z, explicitly a dev pre-release: *"I plan to test this version for 1-2 days, then proceed with an RC followed by a stable release."* (<https://github.com/jundot/omlx/releases.atom>)

---

## 2. LM Studio `mlx-engine`

Source: <https://raw.githubusercontent.com/lmstudio-ai/mlx-engine/main/README.md> ·
<https://github.com/lmstudio-ai/mlx-engine/commits/main.atom> ·
<https://lmstudio.ai/docs/developer/core/ttl-and-auto-evict> ·
<https://raw.githubusercontent.com/lmstudio-ai/docs/28fdbdd42f42d2d789a4f6f0cb11f295a3ec5c85/1_developer/0_core/ttl-and-auto-evict.md> ·
<https://lmstudio.ai/docs/developer/anthropic-compat/messages> ·
<https://github.com/lmstudio-ai/mlx-engine/issues/31>

**Scope note — ⚠️ important:** `mlx-engine` is the MLX *inference engine* bundled into LM Studio
(*"LM Studio 0.3.4 and newer for Mac ships pre-bundled with mlx-engine"*), not a standalone model-serving
product. Its README documents `demo.py` and development setup only — **no model pool, TTL, or pin config**.
Pool behavior belongs to the LM Studio app, so those claims are marked ⚠️ even though they are official docs.

**2a. Multiple models + TTL + resident/pinned — ⚠️ app docs (verified text, app-level attribution)**
- LM Studio docs "Idle TTL and Auto-Evict": *"`Idle TTL` … defines how long a model can stay loaded in memory without receiving any requests. When the TTL expires, the model is automatically unloaded from memory. You can set a TTL using the `ttl` field in your request payload. `[Default: 60 minutes]`"*
- *"`Auto-Evict` is a feature that unloads previously JIT loaded models before loading new ones."*
- *"When Auto-Evict is ON (default): At most `1` model is kept loaded in memory at a time (when loaded via JIT) / Non-JIT loaded models are not affected"* — i.e. the pin/resident tier is the non-JIT (`lms load`) tier.
- CLI/API surface: *"`lms load <model> --ttl 3600`"*, request field `"ttl": 300`, and *"By default, models loaded with `lms load` do not have a TTL, and will remain loaded in memory until you manually unload them."*
- ⚠️ The commit log shows the engine repo gaining a standalone server: "Add experimental server with chat completions endpoint (#353)", committed 2026-08-10 (<https://github.com/lmstudio-ai/mlx-engine/commits/main.atom>). The commit page I fetched rendered only GitHub chrome, so the endpoint schema is ❓.

**2b. HTTP APIs — ✅ / ⚠️** LM Studio server exposes both: OpenAI compatibility docs and Anthropic compatibility (`POST /v1/messages`, documented with streaming SSE events `message_start`, `content_block_delta`, …, <https://lmstudio.ai/docs/developer/anthropic-compat/messages>). The TTL doc confirms the `ttl` field *"works for requests targeting both the OpenAI compatibility API and LM Studio's REST API."*

**2c. Quantized KV cache — ❓ unverified.** Not in the `mlx-engine` README. A feature request exists: "Add KV cache quantization feature · Issue #31 · lmstudio-ai/mlx-engine" (<https://github.com/lmstudio-ai/mlx-engine/issues/31>); the fetched page body did not render, so I cannot confirm open/closed status or any implemented flag. Related issues also appear in search results (`#82`, `lmstudio-bug-tracker#186`) but were not fetched.

**2d. Language — ✅** Python: README requires `python3.11`, `pip install -U -r requirements.txt`, and runs `python demo.py …`.

**2e. Maintenance — ✅ active, no releases.** Last commit 2026-08-21T15:39Z ("Add MLX disk cache control (#369)"). The releases Atom feed for the repo is **empty** (no published releases) — <https://github.com/lmstudio-ai/mlx-engine/releases.atom>.

---

## 3. Ollama's MLX path

Source: <https://ollama.com/blog/mlx> ·
<https://raw.githubusercontent.com/ollama/ollama/main/docs/faq.mdx> ·
<https://raw.githubusercontent.com/ollama/ollama/main/docs/api/openai-compatibility.mdx> ·
<https://raw.githubusercontent.com/ollama/ollama/main/docs/api/anthropic-compatibility.mdx> ·
<https://raw.githubusercontent.com/ollama/ollama/main/go.mod> ·
<https://github.com/ollama/ollama/commits/main.atom> ·
<https://github.com/ollama/ollama/releases.atom>

**MLX is the Apple-Silicon engine — ✅ / ⚠️**
- ✅ Official blog, 2026-03-30: *"Ollama is now powered by MLX on Apple Silicon in preview"* — *"Ollama on Apple silicon is now built on top of Apple's machine learning framework, MLX."*
- ⚠️ Commit log (2026-09-16, "mlx, mlxrunner: move the MLX engine out of x/"): *"The MLX runner is the only Go inference runner left and is no longer experimental, so its packages leave x/."*
- ✅ Release notes: v0.34.1 — *"MLX safetensors `ollama create` no longer experimental."*; v0.34.3-rc1 — *"Nemotron H vision models are now supported on Apple Silicon with MLX"*.

**3a. Multiple models + TTL + pinning — ✅ for the server (FAQ), ⚠️ MLX-specific not restated**
- FAQ "How does Ollama handle concurrent requests?": *"If your system has sufficient available memory … then multiple models can be loaded at the same time."* *"As prior models become idle, one or more will be unloaded to make room for the new model."*
- *"`OLLAMA_MAX_LOADED_MODELS` - The maximum number of models that can be loaded concurrently provided they fit in available memory. The default is 3 * the number of GPUs or 3 for CPU inference."* Also `OLLAMA_NUM_PARALLEL` and `OLLAMA_MAX_QUEUE`.
- TTL / keep-loaded: *"By default models are kept in memory for 5 minutes before being unloaded."* / *"`keep_alive` … any negative number which will keep the model loaded in memory (e.g. -1 …)"* / *"`OLLAMA_KEEP_ALIVE` environment variable"*. Negative `keep_alive` is the documented way to pin a model resident; `ollama stop` unloads.
- ⚠️ The FAQ says these settings apply "on most platforms"; it does **not** explicitly say the MLX runner honors them, and the MLX blog post doesn't restate them. Treated as secondary for the MLX path. ❓ Exact MLX-runner behavior for `OLLAMA_MAX_LOADED_MODELS` unverified.

**3b. HTTP APIs — ✅**
- OpenAI-compatible: `POST /v1/chat/completions`, `/v1/completions`, `/v1/models`, `/v1/embeddings`, `POST /v1/responses` (docs/api/openai-compatibility.mdx).
- Anthropic-compatible: `POST /v1/messages` at `http://localhost:11434/v1/messages` (docs/api/anthropic-compatibility.mdx), with a documented unsupported list including `/v1/messages/count_tokens`, prompt caching, PDFs.

**3c. Quantized KV cache — ✅ flag exists, ⚠️ MLX applicability unverified**
- FAQ "How can I set the quantization type for the K/V cache?": *"`OLLAMA_KV_CACHE_TYPE` - The quantization type for the K/V cache. Default is `f16`."* / *"Currently this is a global option - meaning all models will run with the specified quantization type."* Types: *"`q8_0` - 8-bit quantization … very small loss in precision, this usually has no noticeable impact"*, *"`q4_0` - 4-bit quantization … a small-medium loss in precision that may be more noticeable at higher context sizes"*, *"Models that have a high GQA count (e.g. Qwen2) may see a larger impact on precision"*. Requires Flash Attention (`OLLAMA_FLASH_ATTENTION=1` forces it).
- ⚠️/❓ The FAQ is not engine-specific; whether it applies to the MLX runner on Apple Silicon is not stated on any page I fetched.

**3d. Language — ✅ / ⚠️** Go: `go.mod` is `module github.com/ollama/ollama`, `go 1.26.0`; the MLX path is Go packages `mlx/`, `mlxrunner/` (commit log). MLX itself is called via the `mlx` bindings package ("The bindings become a top-level `mlx` package beside the carried patches in `mlx/compat`").

**3e. Maintenance — ✅ very active.** Last commit 2026-09-19T00:15Z. Latest release `v0.34.3-rc1` (2026-09-19T08:10Z); latest stable `v0.34.2` (2026-09-17T23:04Z, includes *"Fixed excessive memory growth during long generations with MLX speculative decoding"*).

---

## 4. `mlx_lm.server` (mlx-lm's built-in server)

Source: <https://raw.githubusercontent.com/ml-explore/mlx-lm/main/README.md> ·
<https://raw.githubusercontent.com/ml-explore/mlx-lm/main/mlx_lm/server.py> ·
<https://github.com/ml-explore/mlx-lm/commits/main.atom> ·
<https://github.com/ml-explore/mlx-lm/releases.atom>

**4a. Multiple models — ❌ no pool; explicit swap** In `mlx_lm/server.py`:
- `class ModelProvider: """Load models on demand and persist them across the whole process."""`
- `_load()` begins: *"Remove the old model if it exists. Dropping the refs returns the weights to MLX's buffer pool…"* then calls `self.reset()`, `gc.collect()`, `mx.clear_cache()` — i.e. one resident model, replaced on request.
- No `--pin`, `--ttl`, `--max-models`, or idle-timeout flag exists in the `main()` argparse block (flags: `--model`, `--adapter-path`, `--draft-model`, `--prompt-cache-size`, `--prompt-cache-bytes`, `--kv-bits`, `--kv-group-size`, `--quantized-kv-start`, `--decode-concurrency`, `--prompt-concurrency`, `--prefill-step-size`, …).
- ⚠️ "LRU" does exist, but only for prompt/KV caches: `from .models.cache import LRUPromptCache, make_prompt_cache` and `prompt_cache = LRUPromptCache(model_provider.cli_args.prompt_cache_size)`. That is **not** a multi-model pool.
- Warning in code: *"`mlx_lm.server` is not recommended for production as it only implements basic security checks."*

**4b. HTTP APIs — ✅ OpenAI only, ❌ no Anthropic** From the `do_POST` dispatch map: `"/v1/completions"`, `"/v1/chat/completions"`, `"/chat/completions"`; `do_GET` handles `"/v1/models"` and `"/health"`. There is no `/v1/messages` route in either map (explicit absence in the fetched file). README's Server section does not document an Anthropic API.

**4b-bis. Quantized KV cache — ✅ verified flags**
```
--kv-bits            "Number of bits for KV cache quantization (e.g., 4 or 8). Reduces memory
                      usage for long contexts. Disables batching, so requests are served one at
                      a time. Default: None (full precision)"
--kv-group-size      "Group size for KV cache quantization (default: 64)"
--quantized-kv-start "Token position to start KV cache quantization"
```
Also enforced in code: `_is_batchable()` returns False when *"`self.cli_args.kv_bits is None`"* is violated (batching disabled under KV quantization).

**4c. Language — ✅** Python (`pip install mlx-lm`; `from mlx_lm import load, generate`; stdlib `ThreadingHTTPServer`).

**4d. Maintenance — ✅ active but with release lag.** Last commit 2026-09-18T15:27Z. Latest release `v0.31.3`, published **2026-04-22** (~5 months behind HEAD). Recent server-relevant commits include "Fix mlx_lm.server model-swap leak and dead generation thread (#1837)" (2026-09-11), so server fixes are on master but unreleased.

---

## 5. mistral.rs (`EricLBuehler/mistral.rs`)

Source: <https://raw.githubusercontent.com/EricLBuehler/mistral.rs/master/README.md> ·
<https://docs.mistralrs.dev/guides/serve/multiple-models/> ·
<https://docs.mistralrs.dev/reference/cli/serve/> ·
<https://github.com/EricLBuehler/mistral.rs/commits/master.atom> ·
<https://github.com/EricLBuehler/mistral.rs/releases.atom>

**5a. MLX support — ❌ none; Apple Silicon is served via Metal.** The README's backend/feature text says *"Metal on Apple Silicon"* (install section) and *"PagedAttention … for high throughput continuous batching on CUDA or Apple Silicon"*; the word "MLX" does not appear in the README. So mistral.rs is not an MLX server — it is Candle/Metal.

**5b. Multiple models — ✅; pin/TTL/LRU — ❌ (manual only).**
- Doc "Serve multiple models from one process": *"`mistralrs serve -m <model>` loads exactly one model. To host multiple models in one server, use a TOML config and `mistralrs from-config`."* / *"Each `[[models]]` entry is one loaded model, running on its own engine."*
- Unload/reload section: *"`POST /v1/models/unload` frees a model's memory, `POST /v1/models/reload` brings it back, and `POST /v1/models/status` queries its state; each takes `{"model_id": "..."}`."*
- No pin, TTL, idle-timeout, LRU, or max-loaded-models config appears on that page or the CLI reference. Explicit-eviction only. ❌ (absence in fetched pages).

**5c. HTTP APIs — ✅** README "Latest": *"**Anthropic Messages API**: `mistralrs serve` now exposes Anthropic-compatible `/v1/messages` and `/v1/messages/count_tokens` endpoints alongside the OpenAI-compatible `/v1` API."* Quick Start: *"OpenAI-compatible clients use `http://localhost:1234/v1`; Anthropic-compatible clients use `http://localhost:1234`."*

**5d. Quantized KV cache — ✅ flag exists; ⚠️ Metal applicability doubtful.**
- `mistralrs serve` reference: `--pa-cache-type <CACHE_TYPE>` default `auto`, description **"KV cache quantization type"** (also `--pa-context-len`, `--pa-memory-mb`, `--pa-block-size`).
- ⚠️ Same reference: `--paged-attn <MODE>` default `auto`, *"auto: enabled on CUDA, disabled on Metal/CPU (default) - on: force enable (fails if unsupported)"*. Since `--pa-cache-type` is a PagedAttention knob, on Apple Silicon it is disabled by default and would require forcing `--paged-attn on`.
- ❓ Which quantization types `--pa-cache-type` accepts, and whether Metal supports any of them, is not stated on the fetched CLI page. No `--kv-bits`-style flag exists.

**5e. Language — ✅** Rust (README: `cargo add mistralrs`; crates.io link; prebuilt binaries per platform).

**5f. Maintenance — ✅ active.** Last commit 2026-09-08T03:18Z ("docs: sync OpenAPI version with v0.9.3"). Latest release `v0.9.3`, published 2026-09-08T02:58Z.

---

## 6. llama.cpp on Metal

Source: <https://raw.githubusercontent.com/ggml-org/llama.cpp/master/README.md> ·
<https://raw.githubusercontent.com/ggml-org/llama.cpp/master/tools/server/README.md> ·
<https://github.com/ggml-org/llama.cpp/commits/master.atom> ·
<https://github.com/ggml-org/llama.cpp/releases.atom>

**6a. Metal — ✅** README: *"Apple silicon is a first-class citizen - optimized via ARM NEON, Accelerate and Metal frameworks"*; backend table: `[Metal](docs/build.md#metal-build) | Apple Silicon`. Recent commits confirm active Metal work: "metal : add MoE and SSM_CONV fusion optimizations (#28948)" and "metal : fix FA support checks (#29122)", both 2026-09-19.

**6b. Multiple models — ✅ partial; pin/resident — ⚠️ partial; TTL/LRU — ❌ not documented.**
- Server README "Using multiple models": *"`llama-server` can be launched in a **router mode** that exposes an API for dynamically loading and unloading models."* Start with no `-m`; sources via `LLAMA_CACHE`, `--models-dir`, or `--models-preset`.
- Capacity/autoload: `--models-max N` — *"for router server, maximum number of models to load simultaneously (default: 4, 0 = unlimited)"*; `--models-autoload, --no-models-autoload` — *"whether to automatically load models (default: enabled)"*; per-request `?autoload=true|false`.
- Resident/pinned: preset-only option *"`load-on-startup` (boolean): Controls whether the model loads automatically when the server starts."*
- Unload: preset option *"`stop-timeout` (int, seconds): After requested unload, wait for this many seconds before forcing termination (default: 10)"*.
- ❌ No TTL / LRU / idle-eviction config: a grep of the full fetched server README for `ttl|lru|pin|evict` returned no such option. The closest idle knob is `--sleep-idle-seconds SECONDS` — *"number of seconds of idleness after which the server will sleep (default: -1; -1 = disabled)"* — which puts the **server** to sleep; it is not per-model eviction. ❓ Whether an explicit "unload model" HTTP endpoint exists: the fetched README text was truncated before that section, so I could not confirm it (the doc does refer to "requested unload", implying one).

**6c. HTTP APIs — ✅** Server README features: *"OpenAI API compatible chat completions, responses, and embeddings routes"* and *"Anthropic Messages API compatible chat completions"*. Documented routes: `POST /v1/chat/completions`, `POST /v1/completions`, `POST /v1/responses`, `POST /v1/embeddings`, and under "Anthropic-compatible API Endpoints": `POST /v1/messages`, `POST /v1/messages/count_tokens`.

**6d. KV cache quantization — ✅ exact flags and allowed types (this is the direct answer to the task's question).**
- *"`-ctk, --cache-type-k TYPE` | KV cache data type for K — allowed values: f32, f16, bf16, q8_0, q4_0, q4_1, iq4_nl, q5_0, q5_1 (default: f16) (env: LLAMA_ARG_CACHE_TYPE_K)"*
- *"`-ctv, --cache-type-v TYPE` | KV cache data type for V — allowed values: f32, f16, bf16, q8_0, q4_0, q4_1, iq4_nl, q5_0, q5_1 (default: f16) (env: LLAMA_ARG_CACHE_TYPE_V)"*
- ⇒ **V is not limited to f16/q8_0/q4_0** in current master; it accepts the same nine types as K.
- Speculative-decoding draft model gets its own pair: `--spec-draft-type-k, -ctkd, --cache-type-k-draft TYPE` and `--spec-draft-type-v, -ctvd, --cache-type-v-draft TYPE` (same nine values, default f16).
- **Quality caveats in official llama.cpp docs: ❓ not found.** The fetched server README documents no precision/quality caveat for these flags (grep for "quantiz" near the cache flags returned only the flag rows themselves). The nearest *official* caveat text I could fetch is from a **different project** (⚠️ Ollama FAQ): *"q4_0 … a small-medium loss in precision that may be more noticeable at higher context sizes"* and *"Models that have a high GQA count (e.g. Qwen2) may see a larger impact on precision."* Treat llama.cpp-specific quality guidance as unverified here.

**6e. Language — ✅** README: *"Plain C/C++ implementation without any dependencies"*.

**6f. Maintenance — ✅ extremely active.** Last commit 2026-09-19T16:16Z. Latest release is a build tag: `b11053`, 2026-09-19T13:26Z (the project ships rolling `b...` builds; the README badge also references `v*` release tags, which I did not enumerate).

---

## 7. Cross-cutting notes / explicit non-findings

- **`--pin`-style flag naming:** only llama.cpp (`load-on-startup` preset key) and oMLX (docstring `--pin`, not implemented as a CLI arg) use pin-like vocabulary. LM Studio calls its resident tier "non-JIT loaded models"; Ollama uses negative `keep_alive`; mlx-lm and mistral.rs have no pin concept at all.
- **TTL/idle eviction:** present in oMLX (per-model TTL), LM Studio (`ttl`, Auto-Evict), Ollama (`keep_alive`/`OLLAMA_KEEP_ALIVE`). Explicitly **not documented** for llama.cpp, mlx_lm.server, mistral.rs.
- **Model-pool size caps:** llama.cpp `--models-max` (4); Ollama `OLLAMA_MAX_LOADED_MODELS`; oMLX process memory ceiling (`--memory-guard`, `--memory-guard-gb`); LM Studio Auto-Evict (≤1 JIT model); none for mlx-lm/mistral.rs.
- **Quantized KV cache summary:** ✅ mlx-lm (`--kv-bits`), llama.cpp (`-ctk/-ctv`), Ollama (`OLLAMA_KV_CACHE_TYPE`), mistral.rs (`--pa-cache-type`, Metal caveat); ❌/❓ oMLX and mlx-engine (no documented flag found).
- **Unverified gaps (restated):** (i) whether Ollama's env-var pool/KV settings govern the MLX runner; (ii) mistral.rs accepted `--pa-cache-type` values on Metal; (iii) llama.cpp's explicit model-unload endpoint and any KV-quant quality guidance; (iv) any undocumented oMLX admin-panel KV quantization; (v) mlx-engine issue #31 status and the experimental server's endpoint schema.

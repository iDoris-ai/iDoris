# U0 Spike 执行日志

> 开始：2026-07-30 ｜ 目的：验证 iDoris 三能力统一网关的**技术可行性/连通性/性能基线**（不是产品化）。
> 关联规划：[`../../docs/05-集成与技术栈协调规划.md`](../../docs/05-集成与技术栈协调规划.md) §6/§8。

## 环境事实（探测于测试机）

- **测试机 = Apple M1 Max / 64GB RAM**（≠ 部署目标 Mac mini M4 24GB）。→ U0 在 64GB 上验证**架构与连通性**；**24GB 约束交给动态推荐模块用 profile 校验**（不在此机复现内存极限）。符合"测试在本机、部署到 mac mini"约定。
- runtime：node v22.22 ✓；python 3.9.6（⚠️ mlx-lm 训练可能需 3.10+，训练类 spike 再处理）
- 本地模型编排：**oMLX 已装（模型在 `~/.omlx/models/`）但未在跑**；llama-swap / ollama / llama-server **未装**。
- 订阅 CLI：`claude`(~/.local/bin) + `codex`(~/.bun/bin) **都在**。
- npx 可达：ClawRouter `@blockrun/clawrouter` 0.12.235。
- `iogpu.wired_limit_mb=0`（默认，64GB 上 GPU 可用约 42–48GB）。

### 现成本地模型清单（可直接用于 spike，免下载）
| 用途 | 模型 |
|---|---|
| 推理核心(9B级) | **Qwen3-8B-4bit**（Ornith-9B 的 spike 替身）|
| coding/MoE | **Qwen3-Coder-30B-A3B-MLX-8bit**、Qwen2.5-Coder-32B-8bit、Qwen3-Coder-Next-4bit |
| 35B MoE | **Qwen3.6-35B-A3B-6bit**（HF cache；Agents-A1 的 spike 替身）|
| 视觉 | **Qwen2.5-VL-7B-8bit** |
| ASR | Qwen3-ASR-0.6B-8bit、faster-whisper-small/tiny、Fun-ASR-Nano |
| TTS | Fun-CosyVoice3-0.5B、higgs-tts-3-4b |
| 其它 | GLM-OCR、Lance-3B-Video、FLUX（图像）|

> 结论：无需下载 Ornith/Agents-A1 即可跑通机制——用同族替身（Qwen3-8B 当常驻、Qwen3-Coder-30B-A3B / Qwen3.6-35B 当临时大模型）验证 llama-swap 常驻/临时切换。真模型待部署到 mac mini 时再拉。

## 验证结果

### ✅ U0-③ 能力①订阅中转（subprocess relay）—— 通过
- `claude -p "reply with exactly: IDORIS_RELAY_OK"` → 返回 `IDORIS_RELAY_OK`（订阅态非交互中转可行）。
- `codex exec`（非交互，别名 `e`）+ `codex mcp-server`（codex 可作 stdio MCP server）→ 第二条中转路径可行。
- **判定**：能力① 机制成立。落地即 `agent-cli-to-api` 式薄封装（spawn CLI → 封 OpenAI-compat）。按 R1/S3：**能力①标为可选/best-effort，不作核心正确性的必需 fallback**。

### ✅ U0-② 本地模型编排（常驻/临时）—— 通过（用 oMLX，非 llama-swap）
**关键决策变更**：探测发现 `omlx serve` 本身就是 *"multi-model server with LRU-based memory management"* —— 我原打算用 llama-swap 做的"常驻/临时+一个URL+内存受控"，**oMLX 原生已具备**。故**不装 llama-swap**。

实测（`omlx serve --memory-guard balanced --memory-guard-gb 16`，在 64GB 机上模拟 24GB 预算）：
- ✅ 一个 URL(`:8088/v1`)按模型名调多模型（14+ 模型自动发现）。
- ✅ **常驻/热保持**：Qwen3-8B 冷加载 3.8s → 第三次请求 **0.39s**（热命中，留内存=常驻）。
- ✅ **按需加载**：VL-7B 第二次请求时才 load。
- ✅ **内存受控**：16GB guard 下 8B(4.48GB)+VL-7B(8.50GB)=13.25GB 共存，精确核算；`Process memory enforcer ceiling=16.0GB` 生效。
- ✅ **KV 估算内置**：8B "9.00 MB/64token"（36层/8KVhead/128head_dim）——**与 07 §1 公式一致**，验证动态推荐模块算法。
- ✅ **模型级自动驱逐已实测确认**（`--memory-guard aggressive --memory-guard-gb 14`）：载 8B(4.48G)+VL-7B(总13.25G)→`压力 ok→soft`→`Evicting 'Qwen3-8B'`；再载 Llama-3-8B→`14.07>14.00 ceiling`→`Evicting 'Qwen2.5-VL-7B' to fit`。压力分级 ok/soft/hard/ceiling，LRU 驱逐未 pin 模型守住 guard。
  - 澄清："eviction disabled" 仅指 **KV-cache 块**(交给 mlx-lm)，**模型级驱逐正常工作**（ProcessMemoryEnforcer + engine_pool）。
- ⚠️ v0.4.3 小限制：VLM 引擎 guard 传播有告警(`could not resolve scheduler for VLMBatchedEngine`)——不阻塞，若在意等新版 .app。

### oMLX 对外 API 全貌（v0.4.3，确认清楚）
| 端点 | 用途 | 对 iDoris 的意义 |
|---|---|---|
| `/v1/chat/completions` `/v1/completions` | OpenAI-compat 对话 | 主调用面 |
| **`/v1/messages` `/v1/messages/count_tokens`** | **Anthropic Messages 格式** | 本地模型的 Anthropic 兼容**白拿**(能力②的一部分本地就有) |
| `/v1/embeddings` `/v1/rerank` | 向量/重排 | RAG/联邦要的都有 |
| `/v1/responses` | OpenAI Responses API | — |
| **`/v1/models/{id}/load` `/unload`** | **显式装载/卸载** | 常驻/临时**精确可控**(不只隐式 LRU) |
| `/v1/models/status` `/api/status` | 已载/内存(`model_memory_max`) | 动态推荐模块(07)读它做 admission |
| `/admin/settings` | 运行时调设置 | 调 guard/pin |

- **常驻/临时机制(确认)**：`model_settings.is_pinned=true` = 常驻(不驱逐)；unpinned = 临时(压力下自动驱逐)；`/load`+`/unload` 显式控制；`get_pinned_model_ids()`。
- **升级**：Mac .app v0.4.3，无内置 update；升级=手动换 .app(用户操作)。当前功能齐全够用。

### oMLX vs llama-swap —— 对比与决策（回应用户点2）

| 维度 | **oMLX**（选它）| llama-swap |
|---|---|---|
| 本质 | MLX 推理**服务器**（自跑模型）| **代理/编排器**（front 别的后端，自己不跑）|
| 引擎 | 仅 MLX（Apple 原生，M 系最优）| 引擎无关（llama.cpp/vLLM/whisper…）|
| 多模型/一个URL | ✅ 原生 | ✅ 代理层 |
| 常驻/临时 | ✅ LRU + memory-guard（**实测通过**）| ✅ per-model TTL |
| 模拟 24GB 预算 | ✅ `--memory-guard-gb`（**实测**）| ❌ 靠 OS |
| KV 估算 | ✅ 内置（与07一致）| 靠后端 |
| MCP/工具 | ✅ `--mcp-config` + `omlx launch codex` | 部分 |
| 与 Agent24 | ✅ **已是默认运行时**，模型现成 | 需新装(Go) |
| 多引擎混合(非MLX ASR/TTS) | ❌ 仅 MLX | ✅ 强项 |

**目标澄清**：我要的不是"装 llama-swap"，而是"**一个稳定 URL 背后：常驻核心 + 按需临时 + 内存受控**"。oMLX 原生满足且实测通过。
**决策**：
- **capability③ 本地模型层 = oMLX**（不装 llama-swap）。纯 MLX(Apple Silicon)场景 llama-swap 是多余一层。
- **llama-swap 仅当**将来要把**非 MLX 引擎**(whisper.cpp/某些 TTS)纳入同一 URL 编排时才评估；即便那样也可由 iDoris Router 直接编排 oMLX + 其它端点。
- 二者都在 [06 §10.3 LoadPolicy 抽象](../../docs/06-组件接口契约与互换标准.md)之后 → 可换；oMLX LRU = `mode: resident|on_demand` 的参考实现。
- **anti-over-engineering**：少一层、更 Apple 原生。

### ⛔→🔽 U0-① 外部 API —— 降级为可选（回应用户点1，local-first）
- 用户偏好 **本地订阅优先，不重度依赖外部 API**。`.env` 只有 `OPENAI_API_KEY`（+NVIDIA/ARK/BAILIAN），**无 Anthropic/无 Gemini key**。
- **"用它们测试的理由"**：仅为验证网关能否忠实封装非-OpenAI-compat provider。**"没它们就做不了"的硬理由：不存在**——核心全部 local-first，外部只是罕用逃生口。
- **决策**：U0-① **移出关键路径**，外部 API 降为 optional/best-effort（同能力①）。只做一次 **OpenAI-compat 冒烟**（用 .env 的 OPENAI key，OpenAI 本就是 compat，trivial）证明"外部槽位可接"；Anthropic/Gemini 保真度**等真实消费者再做**（先有消费者再有提供者）。

## 能力优先级（本轮定稿）
`③ 本地模型(oMLX,核心) > ① 订阅中转(可选) > ② 外部API(罕用逃生口,最小)` —— 全系 local-first。

### ⛔ U0-① 三路由保真度矩阵（OmniRoute/ClawRouter/LiteLLM）—— 需用户凭证
- Anthropic Messages / Gemini generateContent 的保真度矩阵**需要 Anthropic + Gemini 的 API key**（用户凭证）。
- 无 key 可先测：ClawRouter 免费层（8 免费模型，钱包身份，无需注册）+ 本地模型经路由。
- **待用户提供** Anthropic/Gemini key（或确认只测免费层 + 本地）。

### ⏳ U0-④ 统一 URL 级联 + 性能基线 + 控制面 header —— 待 ②/① 就绪
- 计划：`iDoris Router(薄) → 路由 → llama-swap → 本地模型`，测 TTFT/吞吐 + `X-iDoris-*` 控制面 header 透传（R1/S4）。

## 下一步
1. U0-② 本地编排（免下载，可立即做）。
2. U0-① 需你给 Anthropic/Gemini key（或确认"只测免费层+本地"）。
3. U0-④ 待前两项就绪后串起来测。

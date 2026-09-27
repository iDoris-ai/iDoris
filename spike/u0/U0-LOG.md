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

## 0.6.4 复测（2026-09-27）

> 触发：FU-16（`config/components/omlx.yaml` pin 的是 `omlx@0.6.4`，但适配器与本日志此前只覆盖过 v0.4.3）。
> 环境：本机 `/Applications/oMLX.app` `CFBundleShortVersionString = 0.6.4`；`omlx serve` 已在跑（未带 `--memory-guard`，`:8088`，鉴权 `Authorization: Bearer $OMLX_API_KEY`，key 读自本机 `Agent24/omlx.sh`，未写入任何文件/日志）。
> 原则：不卸载用户当前已加载的模型（复测前 `loaded_models = ["GLM-OCR-bf16", "Qwen3.8-27B-OptiQ-4bit"]`）；只对未加载的小模型（`Qwen3-0.6B-4bit`）做 load/unload 并在结束后恢复原状。
> **修订记录**：本节初版发出后，PR #44 收到 Opus 验收 CHANGES_REQUESTED，指出 model_memory_max/used 单位算错、字段解析会静默吞错误、pressure 缺失时 fail-open 当 "ok"、on_demand 也会误触发必 401 的 pin 调用等问题；以下表格与结论已按修复后的最终版本更新，不代表本次 curl 实测重新跑过——字节数、404/401 等原始观测值不变，变的是**适配器如何解读这些观测值**。

### 端到端结果表

| 端点 | 0.4.3 行为（本日志此前记录/适配器假设） | 0.6.4 实测行为 | 是否变化 |
|---|---|---|---|
| `GET /v1/models` | `{data:[{id,...}]}` | 同形，`data[].id` 不变（18 个模型，含 builtin `MarkItDown`） | 否 |
| `POST /v1/models/{id}/load` | 显式装载 | `curl -X POST .../v1/models/Qwen3-0.6B-4bit/load` → `200 {"status":"ok","model_id":...,"message":"Loaded ..."}`  | 否 |
| `POST /v1/models/{id}/unload` | 显式卸载 | `curl -X POST .../v1/models/Qwen3-0.6B-4bit/unload` → `200 {"status":"ok","model_id":...}` | 否 |
| `GET /api/status` 的 `model_memory_max`/`model_memory_used` | 内存核算字段 | 字段名不变，实测 `model_memory_max=55662788608` `model_memory_used=22722550241`——**单位是字节**（响应自带 `model_memory_max_formatted="51.84GB"` 印证 55662788608/1024³≈51.84）。**修复前适配器直接把这两个字节数塞进契约的 `*Gb` 字段**（`modelMemoryMaxGb=55662788608`），数值被夸大 2^30 倍，**这一行在本节初版里被误判成"无变化、可放心复用"，实际是一个字节/GiB 单位 bug，已在 `status()` 里改成 ÷1024³** | 字段名否，**但适配器的单位换算是新 bug，已修** |
| `GET /api/status` 已加载模型列表 | 适配器/本日志假设字段名 `loaded` | **实测字段名是 `loaded_models`**（`["GLM-OCR-bf16","Qwen3.8-27B-OptiQ-4bit"]`），响应里**没有 `loaded` 键**。修复前适配器对"两个字段都缺失/类型不对"这类看不懂的响应会静默返回 `[]`，导致 `admission()` 把"响应解析失败"误判成"这个模型确实没加载"；已改成：`loaded_models` 非 null 时优先，为 `null` 才退回 `loaded`，两者都缺失或选中值不是数组一律**抛错** | **是（字段改名，已修；解析策略也从"静默兜底"改成"看不懂就报错"）** |
| `GET /api/status` 的 `pressure`（ok/soft/hard/ceiling） | 06 §10.3 抽象对应字段 | 本次实测实例未带 `--memory-guard` 启动，响应中**不含 `pressure` 字段**；是否在 0.6.4 的 memory-guard 模式下字段名不变，本次未验证（重启会连带卸载用户正在用的两个模型，按约束未做，见 FU-18）。修复前字段缺失时适配器 fail-open 地当成 `"ok"`；已改成缺失返回显式 `"unknown"`。不在 `ok/soft/hard/ceiling` 白名单里的值**不抛错**——warn 一行（固定脱敏文案，只报告类型，不带原始值）后按 `"unknown"` 处理，避免一个不认识的 pressure 值连带让同一次 `status()` 里已解析好的 `loaded` 也拿不到（此前一度改成抛错，Codex 复审指出 H2 日志/错误不能带载荷后连同这条一起改成 warn+unknown，见结论第 4 条） | 未定（字段是否存在/改名需 FU-18 补测；**适配器缺失时的默认行为从 fail-open 的 "ok" 改成了显式 "unknown"，这本身是本轮修的 bug**） |
| `GET /v1/models/status`（补充端点，openapi 里仍存在） | U0 表中列过但未展开 | `{final_ceiling, current_model_memory, model_count, loaded_count, models:[{id,loaded,pinned,estimated_size,...}]}`；每模型 `pinned` 字段与 `is_pinned` 语义一致 | 否（形状更丰富，字段兼容） |
| pin/unpin (`is_pinned`) | `POST /admin/settings` body `{model_settings:{id:{is_pinned}}}` | **`POST /admin/settings` → `404 Not Found`（路由已移除）**。经 `GET /openapi.json` 核对，新路由是 `PUT /admin/api/models/{id}/settings`，body 拍平为 `{is_pinned: bool}`（`ModelSettingsRequest` schema——**这个 body 形状只是照 schema 编的，从未实测跑通过**，见下）；实测 `PUT /admin/api/models/Qwen3-0.6B-4bit/settings` 用同一把推理 API key 调用返回 **`401 {"detail":"Admin authentication required"}`**——`/admin/api/*` 在 0.6.4 上要求独立 admin 会话认证（另有 `/admin/api/login`），推理 API key 不适用，请求从未真正到达 handler，**body 是否真的会让 is_pinned 生效完全没有实测证据**。适配器已切到新端点，且只在 `mode:"resident"` 时才调用（`on_demand`/`evict_to_load`/无 policy 一律不调），调用失败时抛专门的 `OmlxPinUnavailableError`（模型已加载、pin 失败、需要 admin 会话），不会把"装入成功但没 pin 住"这种部分成功状态吞掉 | **是（端点搬家 + 新增鉴权门槛 + body 未实测；pin 语义在 0.6.4 上完全不可用，记为 FU-17，不是简单改端点能解决的）** |
| `POST /v1/chat/completions`（非流式） | `{choices:[{message:{content}}]}` | 同形，`curl` 实测 `choices[0].message.content` 正常返回 | 否 |
| `POST /v1/chat/completions`（流式 `stream:true`） | 未在 0.4.3 记录中展开 | SSE `data: {...choices:[{delta:{...}}]}` + 末尾 `data: [DONE]`，标准 OpenAI-compat chunk 形状（含 `reasoning_content` delta，adapter 目前不支持流式，行为未受影响） | 否（首次记录，形状是标准 OpenAI-compat） |

### 复测后状态核对（确认无残留改动）
- 复测前：`loaded_models=["GLM-OCR-bf16","Qwen3.8-27B-OptiQ-4bit"]`，`model_memory_used=22722550241`。
- 复测后：`loaded_models=["GLM-OCR-bf16","Qwen3.8-27B-OptiQ-4bit"]`，`model_memory_used=22722550241`（与复测前完全一致；`Qwen3-0.6B-4bit` 已按预期卸载，pin 尝试因 401 未生效，无残留状态）。

### 结论
1. **`GET /v1/models`、显式 `load`/`unload`、chat completions（非流式/流式）在 0.4.3 → 0.6.4 之间语义未变**。`model_memory_max`/`model_memory_used` 字段名也未变，但**单位是字节、需要换算成 GiB**——这一条**不能说"放心复用"**，本节初版这么写过，是错的，已在适配器里修正并在此更正。
2. **`/api/status` 已加载列表字段名从假设的 `loaded` 变成 `loaded_models`**——`packages/adapters/omlx/omlx-backend.ts` 的 `status()` 已改为 `loaded_models` 非 null 时优先、`null` 才回退 `loaded`，两者都缺失或类型不对时**抛错**（不再静默返回 `[]`）。这修复了两个实质 bug：① 修复前 `admission()` 会因为读不到已加载列表而对已加载模型也一律误判成 `requires_eviction`；② "看不懂响应" 和 "这个模型确实没加载" 被之前的实现混为一谈。
3. **`is_pinned` 的设置端点搬家**（`POST /admin/settings` → `PUT /admin/api/models/{id}/settings`），适配器已切到新端点，且收窄成只在 `mode:"resident"` 时才调用；但新端点在 0.6.4 上要求独立 admin 会话认证，**仅凭推理 API key 无法完成 pin**——这是一个新增的能力缺口，不是简单的路径改名能解决的，**pin 语义在 0.6.4 上完全不可用**，已拆成 FU-17 单独跟进。调用失败时适配器抛 `OmlxPinUnavailableError`（模型已加载但未 pin），不会把"部分成功"状态吞掉或伪装成"整体失败"。**这个缺口是双向的**：`on_demand`/`evict_to_load` 加载完成后，适配器会读 `GET /v1/models/status`（已实测确认推理 API key 就能读，见下方「第二轮补充复测」）核对该模型是否被外部 pin 住，一致性被打破时抛 `OmlxUnexpectedlyPinnedError`，但**检测到之后同样没有能力去 unpin**——pin 得上读不出真状态是一种缺口，pin 上了想摘不掉是另一种，根因都是缺 admin 会话（FU-17）。
4. `pressure` 字段（ok/soft/hard/ceiling）本次未在 memory-guard 模式下复测（避免重启导致用户正在用的模型被卸载），拆成 FU-18 单独跟进；适配器对"字段缺失"的默认行为已从 fail-open 的 `"ok"` 改成显式的 `"unknown"`。对"字段值不在白名单里"的情况，**几经调整**：最初改成抛错，第二轮 Opus 评审指出这样会连带让同一次 `status()` 调用里已经解析好的 `loaded` 列表也拿不到（一个不认识的 pressure 值不该阻塞已经解析成功的其它字段），所以改成 **warn 一行 + 按 `"unknown"` 处理**，不抛错。`"unknown"` 要求消费方按保守方向处理（至少视同 `soft`），但这**只是类型注释里的约定，没有编译期/运行期机制强制**——记入 FU-18。
5. **第三轮（Codex 第一档评审，CHANGES_REQUESTED）指出 H-a 的检测本身是 fail-open 的**：最初的 `isActuallyPinned()` 找不到条目/字段缺失/类型不对时一律默认"未被 pin"（`false`），如果模型在 `POST .../load` 成功之后、读 `GET /v1/models/status` 之前被并发 unload 掉，`load()` 会误判成"成功且未被 pin"直接放行。已改写成 `verifyModelState()`：要求 `models` 是数组、恰好一条 `id` 匹配的条目、`loaded`/`pinned` 都是合法 boolean 且 `loaded===true`，任何一步不满足都抛新增的 `OmlxVerificationError`（reason 枚举：`models_missing`/`model_not_found`/`duplicate_model_entries`/`loaded_field_invalid`/`not_loaded`/`pinned_field_invalid`），resident 分支的 PUT 成功后也复用它复核 `pinned===true` 才算数（之前是"PUT 返回任意 2xx 就当成功"）。同一轮还指出所有错误信息/`console.warn`（`parseLoaded`、`parseMemoryGb`、`parsePressure`）之前把后端原始值 `JSON.stringify` 进了消息里，违反"日志与错误不带载荷"，已全部改成只报告字段名和实际类型/下标（补了塞 `SECRET_SENTINEL` 进响应、断言不出现在任何抛出信息里的哨兵测试）；`parseMemoryGb` 之前用 `Number(value)` 宽松转换会接受数字字符串/布尔/数组/负数，已改成必须严格是 `number` 类型、有限、`>=0`；`OmlxPinUnavailableError`/`OmlxUnexpectedlyPinnedError`/`OmlxVerificationError` 三个错误类型此前只在 `omlx/omlx-backend.ts` 内部可见，已从 `packages/adapters/src/index.ts`（包根）导出，各带稳定的 `code` 字段。**这一轮的"日志与错误不带载荷"其实没堵干净**——见下面第四轮。
6. **第四轮（Codex 第二档评审，CHANGES_REQUESTED）用探针实测到第三轮遗漏的一处泄露**：`OmlxPinUnavailableError` 的构造函数当时仍然拼了 `cause instanceof Error ? cause.message : String(cause)` 进自己的 `message` 里——第三轮堵的是 `parseLoaded`/`parseMemoryGb`/`parsePressure` 这些直接解析字段值的地方，但没堵"把下游抛出的错误对象转述一遍"这条路，而 `fetchImpl` 是外部可注入的，下游错误的 `message` 完全可能携带不该出现在日志里的内容。修法：`OmlxPinUnavailableError`/新增的 `OmlxPinStateUnverifiedError` 都只保留安全的结构化元数据字段——`causeErrorName`（cause 的构造函数名或 `typeof`）、`causeHttpStatus`（仅当 cause 是新增的内部类型 `OmlxHttpError` 时才有值）、`causeReason`（仅 `OmlxPinStateUnverifiedError` 有，复用 `OmlxVerificationError.reason` 的固定枚举）——`message` 本身不再插值任何 cause 相关内容。为此把 `post()`/`json()` 底层的通用 `Error` 换成了内部的 `OmlxHttpError`（只带 method/path/status），这样"HTTP 失败的状态码"变成一个可读的结构化字段，不用去解析错误消息文本。补了哨兵测试：settings 请求直接 `throw`（模拟网络层错误）且消息里塞 `SECRET_SENTINEL`，断言它不出现在 `message`、`String(err)`、`JSON.stringify(err)` 里。
7. **第四轮同一轮还指出两处收尾问题**：① `verifyModelState()` 读到的顶层响应如果是 `null`/数字/字符串/数组，直接访问 `.models` 要么抛 `TypeError` 要么静默拿到 `undefined` 被误判成"models 缺失"——已补上 `isPlainRecord()` 顶层校验，统一抛 `OmlxVerificationError(reason="response_invalid")`；`status()` 对 `/api/status` 的顶层响应做了同样的校验。② resident 分支里，PUT 成功之后如果"复核"这一步本身失败（响应畸形、模型被并发卸载等），之前会被包成 `OmlxPinUnavailableError`——相当于把"不知道"错误地说成了"已经确认失败/被 admin 认证拒绝"。已改成：只有 PUT 请求本身被拒绝、或复核后读到明确的 `pinned===false`，才算"已确认失败"抛 `OmlxPinUnavailableError`；复核这一步本身失败，改抛新增的 `OmlxPinStateUnverifiedError`（`code=OMLX_PIN_STATE_UNVERIFIED`），把"未知"和"已确认失败"分开。

### 第二轮补充复测（2026-09-27，回应 Opus 第二轮 CHANGES_REQUESTED）

> 触发：H-a——`on_demand`/`evict_to_load` 不再调用 unpin 之后，如果模型本来就被外部 pin 住（用户在 oMLX 管理页手动 pin、pin 状态跨重启持久化、或曾经切换成过 resident），`load()` 会静默成功，调用方拿不到任何信号。修法要求先实测确认：能否用推理 API key 读 `GET /v1/models/status` 来检测 pinned 状态（只读，不改变任何状态）。

- **实测结果**：`curl -H "Authorization: Bearer $OMLX_API_KEY" http://127.0.0.1:8088/v1/models/status` → `HTTP 200`，与 `/admin/api/*` 的 401 形成对照——**这个端点不受 admin 会话限制，推理 API key 就能读**。已据此在 `OmlxBackend.verifyModelState()`（Codex 复审后从最初的 `isActuallyPinned()` 改名并收紧为严格校验，见「第三轮」）里实现 H-a 的检测逻辑。
- **顺带观测**（复测时刻的真实状态，不是本次操作造成的）：这次读到的 `loaded_models` 比第一轮复测结束时多了 `Qwen3-0.6B-4bit`、`Qwen3-8B-4bit`——两次复测之间用户/其它进程加载了这些模型，本次全程只做了这一次只读 `GET`，**没有 load/unload 任何模型，没有改变任何状态**。当时读到的全部已加载模型 `pinned` 字段都是 `false`（`GLM-OCR-bf16`、`Qwen3-0.6B-4bit`、`Qwen3-8B-4bit`、`Qwen3.8-27B-OptiQ-4bit`、`MarkItDown`），与下面 L-b 的观测一致，没有发现被外部 pin 住的模型。
- **L-b 补记（第一轮复测时的观测，此前只记在代码注释里，这里正式记录进日志）**：第一轮复测中，`POST /v1/models/Qwen3-0.6B-4bit/load` 成功后，立刻 `GET /v1/models/status` 核对该模型条目，**`pinned` 字段是 `false`**——这是「`POST /v1/models/{id}/load` 本身不会隐式把模型 pin 住」这一判断的直接实测依据，`load()` 里"on_demand/evict_to_load 不主动调用 unpin"这段设计正是建立在这条观测之上。

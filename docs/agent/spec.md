# iDoris 统一模型服务 · 规格 — 落地细节

> 「建成什么样」。精确到能照着实现。架构与边界见 [`architecture.md`](architecture.md)。
> 契约语义来源：[`../06-组件接口契约与互换标准.md`](../06-组件接口契约与互换标准.md) §10；内存公式来源：[`../07-模型量化内存评估与动态推荐.md`](../07-模型量化内存评估与动态推荐.md)。
> 记录日期：2026-09-07

## 产品定义

iDoris = 一层 AI 网关，有两种形态：**个人 AI 网关**（`deploy_mode: personal`）与**组织大脑**（`deploy_mode: tenant`）。对外一个 OpenAI-compat URL，对内把 ①本地订阅中转 ②外部 API ③本地模型（常驻+临时）三类能力，按业务意图路由、组合、降级。

- **personal**：Agent24 / 微信 bridge / blog / banner 脚本只配一个地址，就能在隐私分级约束下用到本机全部 AI 能力。
- **tenant（组织大脑）**：为多个租户托管，每次调用带 `tenant`，用量 / 预算 / 审计**按 tenant 硬隔离**。多租户只作用于能力②③；**能力① 在 tenant 模式下拒绝注册**（loopback + 单用户红线不因多租户松动）。

**仓库形态**：pnpm workspace（TypeScript）。
```
packages/
  contracts/     # 五份契约的 TS 类型 + zod schema + 校验器（无运行时依赖）
  router/        # iDoris Router 进程：HTTP server + policy 引擎 + registry
  adapters/
    omlx/        # LoadPolicy → oMLX knob 映射
    subscription/# 能力① spawn CLI 封 OpenAI-compat
    openai-compat/ # 能力② 通用上游槽位
  recommender/   # M2 HardwareAwareModelRecommender
  tenancy/       # TenantContext / 预算 / 用量聚合 / 审计（tenant 模式）
config/
  routing-policy.yaml    # 声明式路由策略（版本化）
  components/*.yaml      # 组件卡
  catalog.yaml           # 模型目录（recommender 输入）
```

## 数据模型

### ProviderDescriptor（06 §10.1）
所有路由/隐私/fallback/计费/发现读同一个描述符。

| 字段 | 类型 / 取值 | 约束 |
|:---|:---|:---|
| `id` | string | 全局唯一，registry 主键 |
| `family` | `idoris\|claude\|openai\|local\|other` | 必填 |
| `tier` | `local\|remote\|lora` | 必填；隐私判据 |
| `capabilities` | `(chat\|reasoning\|vision\|asr\|tts\|coding\|embedding\|rerank)[]` | 非空 |
| `privacy_class` | `local_only\|any` | 必填；该 provider 可承载的最高隐私级 |
| `cost` | `{input_per_m: number, output_per_m: number}` | 本地 = 0 |
| `locality` | `loopback\|lan\|remote` | 必填；**决定 tier 是否可信 local**（`tier=local` 且 `locality=remote` 为非法组合，注册时拒绝）|

### ComponentCard（06 §10.2）— 强制策略字段
每个组件接入必须提供；**缺任一强制字段 → 不注册，启动即报错**。

| 字段 | 类型 / 取值 | 说明 |
|:---|:---|:---|
| `provider` | ProviderDescriptor | 内嵌 |
| `form` | `http_service\|spawn_cli\|bundled_binary\|nostr_node\|mitm_proxy\|batch_job` | 06 §3 六形态 |
| `endpoint` | string | HTTP 形态必填；spawn 形态填 argv 模板 |
| `version_pin` | string | 二进制版本号 / 容器 tag / commit，**必填** |
| `privacy_class` | `local_only\|any` | 强制 |
| `allowed_egress` | `(none\|loopback\|lan\|internet)[]` | 强制 |
| `fallback_policy` | `fail_closed\|next_in_chain` | 强制 |
| `fail_closed` | bool | 强制；`privacy_class=local_only` 时必须为 `true` |
| `load_policy` | LoadPolicy | 能力③组件必填 |

### LoadPolicy / ModelLease（06 §10.3）
| 字段 | 取值 | oMLX 实现映射 |
|:---|:---|:---|
| `mode` | `resident\|on_demand\|evict_to_load` | `is_pinned=true` / unpinned 首请求加载 / ProcessMemoryEnforcer 压力驱逐 |
| `keepalive.pinned` | bool | `model_settings.is_pinned` |
| `keepalive.idle_ttl_s` | number | unpinned + LRU |
| `admission` | `coexist\|requires_eviction` | `GET /api/status` 的 `model_memory_max` / loaded 核算 |

显式控制映射：`POST /v1/models/{id}/load` / `/unload`；内存预算 `--memory-guard {safe\|balanced\|aggressive}` + `--memory-guard-gb`。
> llama-swap 的 per-model TTL 是同契约的另一种实现；Ollama / vLLM 亦可。**契约不变，实现可换。**

### RoutingPolicy（06 §10.6）— 声明式，版本化
```yaml
routing_policy:
  version: 1
  rules:
    - if: {privacy: local_only}   then: {tiers: [local, lora], fail_closed: true}
    - if: {intent: banner}        then: {capability: vision, load: on_demand}
    - if: {intent: coding}        then: {capability: coding}
    - if: {complexity: complex}   then: {tiers: [local, remote]}
  default: {tiers: [local], fail_closed: true}   # local-first 默认
```
规则**按序匹配、首条命中即用**；`default` 必填。换 Router = 换解释引擎，策略配置照搬。

### TaskProfile（控制面 → 内部表示，06 §10.5）
| 字段 | 来源 header | 默认 |
|:---|:---|:---|
| `privacy` | `X-iDoris-Privacy` | `local_only`（**保守默认**：未声明按最严处理）|
| `intent` | `X-iDoris-Intent` | `chat` |
| `complexity` | `X-iDoris-Complexity` | `simple` |
| `capabilities` | `X-iDoris-Capabilities`（逗号分隔）| `[chat]` |
| `fallback` | `X-iDoris-Fallback` | 取自 policy 命中规则 |

映射 Agent24 `TaskProfile{privacy, complexity}`，对齐产品隐私分级 0–3。

### TenantContext（`deploy_mode: tenant` 必备）
| 字段 | 类型 | 约束 |
|:---|:---|:---|
| `tenant_id` | string | 来自 `X-iDoris-Tenant`；**tenant 模式下缺失即 400**，不得回落到「默认租户」 |
| `budget.limit_minor` | int | 账期预算上限（最小货币单位，避免浮点）|
| `budget.spent_minor` | int | 本账期已花费 |
| `budget.scope` | `paid_only\|all` | **见下方「预算作用域」**；默认 `paid_only` |
| `billing_timezone` | IANA 时区名 | **必填、显式配置**，如 `Asia/Bangkok`；**不得取服务器本地时区** |
| `quota.rpm` / `quota.tpm` | int? | 可选限流 |

**预算作用域（本仓库对 R1 的一处细化）**：上游需求原文是「预算硬停排在任务匹配之前」。对纯外部 API 的网关这是对的，但 iDoris 有**零成本的本地模型**——一刀切会让预算耗尽的租户连不花钱的本地推理都用不了。故：
- `scope: paid_only`（默认）：预算只闸住 `cost > 0` 的候选；本地模型（`cost = 0`）不受影响，租户超预算后仍可用本地能力。
- `scope: all`：组织要求「超预算就完全停」时使用，任何调用一律拒绝。
两种都是显式选择，**不留给实现推断**。

### 租户数据隔离（硬隔离，不是查询时加条件）
用量 / 预算 / 审计三类数据的访问层**必须带 tenant 作用域**；**缺 tenant 上下文的查询直接抛错，而非返回全量**。这条是「A 租户查不到 B 租户任何一条」的唯一可靠保证——靠调用方每次记得加 `where tenant_id = ?` 是失败开放。

### AdapterManifest（M3，06 §10.4）
`base_model_digest` / `tokenizer_digest` 不匹配 → **拒绝聚合/挂载**。其余字段：`framework(mlx|peft|unsloth)`、`tensor_format`、`rank`、`target_modules`、`quantization`、`license`、`privacy_treatment(raw|dp_noised|secure_agg)`、`aggregation_compat(fedavg|fedit|flora)`。

### 模型目录 catalog（07 §5.2，recommender 输入）
每条：`id`、`family`、`params_total_b`、`params_active_b`（MoE 内存按 total 算、速度按 active 算）、`arch{n_layers,n_kv_heads,head_dim}`、`modality`、`capability{reasoning,coding,...}` 评分、`quant_options[{label,bpp,quality}]`、`min_ram_gb`。

**内存公式**：`footprint = params_total_b × bpp + KV(ctx) + 开销`；Apple 可用预算按 `RAM×0.66 ~ (RAM−6)` 三档（conservative/moderate/aggressive）。
**已算清的基线（M4 24GB）**：常驻 `ornith-1.0-9b @ q6_k, 32K ctx ≈ 9.8GB`；可共存临时 `qwen2.5-vl-3b@q4 ≈2GB` / `whisper-turbo ≈1GB` / `kokoro-82m ≈0.4GB`；`qwen2.5-coder-14b@q4 ≈8GB` 临界共存；**`agents-a1-35b@q4 在 24GB 判 BLOCKED（需 32GB+）`**。推荐 `iogpu.wired_limit_mb=18432`。

## 状态机

### 请求路由状态机（Router 核心）
```
RECEIVED ─▶ PROFILED ─▶ POLICY_MATCHED ─▶ BUDGET_CHECK ─▶ CANDIDATES ─▶ ADMITTED ─▶ DISPATCHED ─▶ DONE
                                              │               │           │            │
                              over budget ────┘               │           │            └─▶ FAILED ─┐
                                    │                         │           └─▶ EVICT_THEN_LOAD ─────┤
                                    ▼                         ▼                                     │
                          REJECTED_BUDGET (终态,402)     NO_CANDIDATE                                │
                                                              │                                     ▼
                                              fail_closed? ───┴── yes ─▶ REJECTED_LOCAL_ONLY   DEGRADE
                                                              └── no ──▶ next_in_chain ──▶ (回 CANDIDATES)
```
- `REJECTED_LOCAL_ONLY` 是**终态**，返回 HTTP 503 + `{"error":{"type":"local_only_unavailable"}}`，**不得**转向任何 `locality != loopback` 的 provider。
- `REJECTED_BUDGET` 是**同级终态**，返回 HTTP 402 + `{"error":{"type":"budget_exceeded"}}`。预算检查在 `POLICY_MATCHED` 之后、`CANDIDATES` 之前——**先于任何候选选择**，因为再便宜的候选也是花钱。
- `DEGRADE` 仅在 `fallback_policy: next_in_chain` 且 `privacy != local_only` **且未触发预算终态**时可达。

### 路由决策的执行顺序（顺序即语义）
```
1. 隐私判定    privacy=local_only → 候选收窄到 tier ∈ {local, lora}
2. 预算闸门    tenant 模式且 budget 超限 → 按 scope 决定是否终态拒绝
3. 意图/能力匹配  intent/capabilities → 选具体 provider
4. admission   容量核算 → coexist / requires_eviction
5. 降级链      仅技术性失败可达，受 fallback_policy 与隐私复核约束
```
**第 1 步必须在第 3 步之前**：反过来的话，「做 banner → 视觉模型（可能是外部 API）」这条意图规则会先命中，隐私判定就没机会了。顺序即语义，且这条要有测试，不是靠代码走查。
**第 2 步在第 3 步之前**：再便宜的候选也是花钱，预算不该等选完模型才检查。

### 模型生命周期（LoadPolicy）
```
UNLOADED ──load──▶ LOADING ──▶ LOADED(warm) ──idle_ttl/pressure──▶ EVICTING ──▶ UNLOADED
                                    │
                              pinned=true → 不进 EVICTING（常驻）
```
压力分级 `ok → soft → hard → ceiling`（oMLX 实测语义）；`ceiling` 时按 LRU 驱逐未 pin 模型。

### 组件注册
```
DISCOVERED ──schema 校验──▶ VALID ──策略字段校验──▶ REGISTERED ──health──▶ HEALTHY ⇄ COOLDOWN
                │                      │
                └─▶ REJECTED_SCHEMA    └─▶ REJECTED_POLICY（缺 privacy_class 等）
```

### 审计记录（AuditRecord）— 只存元数据
**允许**的字段，穷举：`request_id` / `tenant_id` / `component` / `intent` / `privacy` / `tier` / `provider_id` / `model_id` / `tokens_in` / `tokens_out` / `cost_minor` / `latency_ms` / `status` / **`reason`** / `ts_utc`。

- **`reason` 必须非空**，且能区分四类：`privacy_enforced` / `budget` / `intent_match` / `degraded`。一句「routed」不合格——客户问「这次为什么用了贵的那个模型」答不上来就没法收钱，我们自己排查路由错误也只能靠猜。
- **绝不记录内容**：prompt / completion / 任何客户数据。两道防线：① 写入前对字段名比对黑名单 frozenset（`prompt` `prompts` `input` `content` `text` `body` `messages` `document` `file` `payload` …），命中即抛 `ContentLeakError` **拒绝写入**（不是静默丢弃——静默丢弃会让人以为内容被存下来了）；② 单字段 500 字符上限（长文本出现在元数据里，本身就是「有人把内容塞进来了」的信号）。
- **`ts_utc` 存 UTC epoch，账期聚合用 `billing_timezone`**，见下方时区规则。

### 用量聚合与账期时区
月度聚合的月份边界**必须用租户显式配置的 `billing_timezone`**，不得使用服务器本地时区。
> 真实踩过的坑（iDoris-website 移交）：月份边界用本地时区而时间戳存 UTC，同一笔「曼谷 10-01 06:00」的调用在 UTC 算 9 月、在曼谷算 10 月——**换台机器部署客户账单就变，且没有任何东西报错**。更阴的是当时测试也用本地时区造时间戳，两边一起漂，在任何时区都自洽地全绿。故**回归测试必须真的切换进程时区**（`TZ` + `tzset()`）跑同一批数据，断言三个时区结果完全一致；只在测试内部造时间戳的写法抓不到这个 bug。

## 错误处理 / 幂等

- **失败分类**：`upstream_timeout` / `upstream_5xx` / `oom` / `model_load_failed` / `local_only_unavailable` / `budget_exceeded` / `tenant_missing` / `policy_violation`。前四类可触发降级链（受 `fallback_policy` 约束），后三类**终态不重试**。
- **两个拒绝终态的共性（重要）**：`local_only_unavailable` 与 `budget_exceeded` 在状态机里**同级**，共性是「都不是重试或降级能解决的问题」。前者是隐私约束；后者是**商业约束**——客户设了预算上限就是不想再花钱，自动降级等于替他决定「继续花，只是花得少些」，而悄悄降级的账单等客户看到时钱已经花了。技术性降级（忙 / OOM / 超时）与商业性拒绝必须走不同出口。
- **重试**：仅对幂等的 `POST /v1/chat/completions`（非 streaming）做最多 2 次退避重试（250ms → 1s，jitter）。streaming 请求**已开始吐 token 后不重试**，直接以 SSE error 事件终止。
- **幂等键**：可选 `X-iDoris-Request-Id`；相同 id 在 60s 窗口内命中缓存的终态结果，避免中转 CLI 被重复 spawn。
- **health / cooldown**：复用 Agent24 `ModelRouter` 语义——连续 3 次失败进 `COOLDOWN 30s`，期间该 provider 不参与候选。
- **驱逐竞态**：`evict_to_load` 必须持有 per-backend 互斥锁，防止两个请求同时驱逐彼此需要的模型（活锁）。锁等待超时 10s → 返回 `oom` 而非无限等待。
- **subprocess 中转**：`claude -p` / `codex exec` 必须设超时（默认 120s）并在超时/取消时 `SIGTERM → 5s → SIGKILL`，绝不留孤儿进程。

## 测试策略

| 层 | 覆盖 | 机器可验证 |
|:---|:---|:---|
| **单测** | 内存公式、量化 bpp 表、policy 规则匹配顺序、TaskProfile header 解析（含缺省与非法值）| `pnpm test` |
| **契约测试（zod schema）** | 五份契约的合法/非法样例；**非法样例必须被拒绝**（缺 privacy_class、tier=local+locality=remote 等）| `pnpm test:contract` |
| **隐私回归测试（最高优先级）** | 停掉全部本地 provider，发 N 条 `local_only` 请求，断言 **N 条全部 503、0 条出站**（用假上游 + 出站计数器断言）| `pnpm test:privacy` |
| **出网启动断言** | 进程启动后、处理任何请求前，socket 层打桩拒绝所有非本机连接，断言零出网。**必须配正对照**（一个刻意出网的用例要被探针抓到）—— 抓不到出网的探针，它报的「零出网」什么都不证明 | `pnpm test:egress` |
| **审计内容闸门** | 字段名黑名单比对 + 单字段长度上限，命中即**抛错拒绝写入**（不是静默丢弃 —— 静默丢弃会让人以为内容被存下来了）| `pnpm test:audit` |
| **租户隔离** | 造两个 tenant 的用量/预算/审计，断言 A 查不到 B 的**任何一条**；且**缺 tenant 上下文的查询抛错**而非返回全量 | `pnpm test:tenancy` |
| **账期时区** | 同一批数据在 `TZ=UTC` / `TZ=Asia/Bangkok` / `TZ=Pacific/Midway` 下聚合，断言月度结果完全一致。**必须真的切换进程时区**（`TZ` + `tzset()`），不是在测试内部造时间戳 | `pnpm test:billing` |
| **路由顺序** | 构造「`privacy=local_only` 且意图明确指向外部能力」的请求，断言走本地或报错、**绝不出设备**——证明隐私判定确实排在意图匹配之前 | `pnpm test:privacy` |
| **黄金一致性测试（L3）** | 同一组请求分别打到 oMLX 适配器与 mock 适配器，断言 LoadPolicy 语义等价 —— 这是「可替换」承诺的唯一凭证 | `pnpm test:golden` |
| **集成测试** | 真起 oMLX（若本机可用）跑 load/evict/warm-hit 序列；不可用时 skip 并明确打印 SKIPPED，**不得静默通过** | `pnpm test:integration` |
| **冒烟** | `curl /v1/models`、标准 openai SDK 调通、streaming 首 token | `pnpm smoke` |

**门禁**：`pnpm lint && pnpm typecheck && pnpm build && pnpm test` 全绿才可开 PR；`test:privacy` 失败视为**阻断级**，不接受「已知问题」标注。

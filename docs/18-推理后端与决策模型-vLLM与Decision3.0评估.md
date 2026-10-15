# 18 · 推理后端与决策模型：vLLM 与 Decision 3.0 评估

> 状态：**方向已确认，计划修订待审核**（2026-10-15）· prdaemon Codex astra 审核通过后，仍须 owner 明确允许才启动开发；本 PR 不授权实现或实机 spike
> 基线：`origin/main`（2026-10-15）。外部事实为当日快照，引用处附来源。

两个问题：

1. **推理后端**：是否用 vLLM（Mac 上为 vllm-metal）替换默认的 oMLX，以获得跨平台和统一接口？
2. **决策模型**：vLLM Semantic Router 团队发布的 Decision 3.0（d3 系列）能不能用？用的话现有 routing / decide 代码怎么融合？

一句话结论：

- **不换 oMLX。** 在 iDoris 自己的 `RuntimeAdapter` 契约后面补 vLLM（Linux 的进程托管 runtime kind）和 Ollama（Windows 的服务 adapter）。
- **评测 Decision 3.0，达标后只当提议者（proposer）。** 经 oMLX 的 `/v1/systemone` 接入三个单向阀插槽，不替换确定性的 `decide()` 管线。

---

## 1. 核实过的事实

### 1.1 代码现状

- **iDoris 不自己加载权重。** 除 `vendor/` 外，代码里没有 mlx / torch / transformers / llama_cpp / vllm 导入。推理全部走 loopback HTTP。iDoris 只在配置要求时以子进程方式拉起 `mlx_lm.server` / `llama-server`（`crates/idoris-upstream/src/local_runtime.rs`）。
- **抽象层已经存在。**
  - `RuntimeAdapter`（`crates/idoris-backend/src/adapter.rs`）加 Supervisor。Supervisor 负责加载、驱逐、内存准入和 load fence，与具体后端无关。
  - 已有实现：
    - `OmlxAdapter`（`crates/idoris-upstream/src/omlx/`）。依赖 oMLX 专有端点：`/v1/models/{id}/load|unload`、`/v1/models/status`、`/admin/api/*` pin。
    - `LocalHttpRuntimeAdapter`（`local_http_runtime.rs`）。一进程一模型，只用 `/v1/models` 和 `/v1/chat/completions`。
    - `ChatProxy`（`crates/idoris-router/src/proxy.rs`）。针对 `form: http_service` 的纯透传，**不发 Authorization 头**。
  - 工厂 `crates/idoris-upstream/src/factory.rs` 只认 `"omlx"`。
  - `detect.rs` 里有 vLLM / Ollama 槽位但没有实现；`detect_backend` 没接进工厂；NVIDIA 探测硬编码为 `false`。
- **方向早已定调。** `docs/05 §③`、`docs/06 §10.3`、`docs/agent/roadmap.md` F1.2 都写了「不写死 oMLX」：macOS→oMLX，Win/Linux+NVIDIA→vLLM，其余→llama.cpp / Ollama，全部在 OpenAI-compat + LoadPolicy 契约之后。
- **路由今天全是确定性的。** 依次是 header 解析 → YAML first-match 规则（`crates/idoris-router/src/routing_policy.rs`）→ 纯 Rust `decide()` 过滤链（`crates/idoris-policy/src/pipeline/`）。
  - 意图识别是 FNV 哈希「伪 embedding」加固定中英话术，阈值 0.65（`crates/idoris-router/src/intent/mod.rs`），不是真正的模型。
  - 复杂度没有分类器，只靠 `X-iDoris-Complexity` header；现有契约只有 simple/complex 两档。
- **已有缺口。** `config/routing-policy.yaml` 没有任何规则引用检测出的 4 个 intent（web_search / coding / email / agent_task）。对仓库默认策略，这四类检测结果不改变路由；自定义策略可以引用 intent，BM 仍需覆盖兼容性与发布说明。

### 1.2 外部调研

| 项 | 结论 |
|---|---|
| vLLM 跨平台 | Linux（CUDA / ROCm / XPU / CPU）有官方支持。**Windows 没有官方原生支持**，官方路径是 WSL2（或 Docker Model Runner + WSL2），原生 Windows 只有社区构建。边缘 / 端侧（手机、Core ML）不是 vLLM 的领域。 |
| vllm-metal | vllm-project 下的「社区维护硬件插件」，要求 macOS 15+ 和 Apple Silicon。底层仍是 MLX，由 `mlx_lm` 提供层实现。paged attention 标为 experimental。`vllm serve` 范式是**一进程一模型**。 |
| oMLX | 仅支持 macOS Apple Silicon。能力：原生多模型、LRU 驱逐、pin、手动 load / unload、内存上限、SSD 二级 KV cache；同时提供 OpenAI 端点和 Anthropic `/v1/messages`，以及 embeddings、rerank。**已支持 `POST /v1/systemone`，并点名支持 Decision 3.0 全系**（d3 / flash / mini / nano / lite / edge）。 |
| Ollama | Win / Mac / Linux 原生。官方 FAQ 确认 `/api/ps`/`keep_alive` 对应状态和加载/卸载、无限空闲驻留；是否满足 iDoris 的 pin 后置条件须实测。先前关于 0.35 `/v1/systemone` 的依据仅为二手报道，**本轮未核实具体版本和 d3 支持，不作为实施前提**。 |
| Decision 3.0 | 来自 vLLM-SR 团队，Apache-2.0，基于 Qwen3.5 系。输入是 state（文本 / JSON / 图片 / 视频）加 typed questions（`choice` / `noul`（布尔概率） / `score`），输出选项概率而不是生成文本。HTTP 形态为 `/v1/systemone`（TypeSafe System One 线格式）。 |
| d3 规模 | edge 0.59B、lite 0.85B、nano 2.21B、mini 4.54B、flash 8.39B、d3 26.09B。edge 在新域任务上只有 12.9 分（d3 为 60.5），只适合窄而经过验证的类目。 |
| d3 在 Mac 上 | 在 M5 Pro 上，Core ML 版 d3-lite 约 53ms、d3-nano 约 69ms（JevBench 中位数）。下载体积：lite 3.7GB、nano 10.5GB。 |
| d3 风险 | **中文能力没有证据**。Engine 处于 dev channel。博文作者自测时出现过排序错误（「今天」没有排在 urgency 首位）。 |

---

## 2. 结论一：不把 oMLX 换成 vllm-metal

### 理由

1. **统一点在 iDoris 自己的契约，不在引擎。** `RuntimeAdapter` 加 OpenAI-compat 已经把接口统一了。换引擎只是换掉一个实现，接口不会因此更统一。以后要换 Ollama，也仍需验证后端能力、加载后置条件、身份、认证和预算；HTTP 格式一致不代表这些语义已统一。
2. **vLLM 的跨平台程度不如设想。** 它是「Linux 一等公民 + Mac 社区插件 + Windows 只能走 WSL2」。真正在三个平台都原生可用的是 Ollama 和 llama.cpp。「边缘计算」这条理由也不成立。
3. **oMLX 覆盖当前已验证的工作流**：多模型常驻、LRU、pin、SSD KV，以及 `/v1/systemone` 加 d3。Anthropic/rerank 是额外 API 能力，不是每个 RuntimeAdapter 都必须实现的 Supervisor 能力。一进程一模型可以由现有 Supervisor 管理，不能单凭这一点否决 vllm-metal；两者都使用 MLX 也不能推导性能相同。保留 oMLX 的依据是现有集成成本与尚无替换收益证据，性能由 A3 同条件实测。
4. **换掉默认代价大、收益不明。** 这等于推翻 U0 的实测结论（`spike/u0/U0-LOG.md`），pin、内存压力和 K09/K11 的适配器语义必须重新验收；通用 Supervisor 修复不必因此全部重写。

### 按平台分配默认后端

| 平台 | 默认 | 备选 |
|---|---|---|
| macOS Apple Silicon | **oMLX**（不变） | vllm-metal（仅作可选 runtime kind，用于 benchmark 对比）、Ollama |
| Linux + NVIDIA | **vLLM**（计划新增，需 Linux 实机验收） | llama.cpp、Ollama |
| Linux + AMD | vLLM/ROCm 显式配置候选；本轮不自动探测 | llama.cpp、Ollama |
| Windows | **Ollama**（新增，原生） | vLLM via WSL2、llama.cpp |
| 无 GPU / 低配 | llama.cpp | Ollama |

### 实施切片（基于 `origin/main`，另行批准）

- **R1 `LocalRuntimeKind::Vllm`**
  - 在 `local_runtime.rs` 增加第三种 kind，argv 为 `vllm serve <model> --host 127.0.0.1 --port N --served-model-name <id>`。
  - 复用 `ManagedRuntimeProcess`、`/v1/models` 就绪探测和 `LocalHttpRuntimeAdapter`。
  - 该 kind 不做 `.gguf` 校验；请求中的 `model` 用 served name。
  - vllm 和 vllm-metal 用同一个 CLI，一次实现同时覆盖两者。
- **R2 `OllamaAdapter` 实现 `RuntimeAdapter`**
  - list 用 `/api/tags`，status 用 `/api/ps`；load / unload / pin 用 `/api/generate` 加 `keep_alive`；chat 用 `/v1/chat/completions`。
  - `factory.rs` 增加 `"ollama"` 分支。
- **R3 接通探测**：把 `detect_backend` 接进默认 card 的选择逻辑，并实现 NVIDIA 探测。
- **R4 `ChatProxy` 支持可选 Bearer**，并给进程 runtime 的 readiness/chat 及 SystemOneClient 接通同一认证；只修 ChatProxy 不足以支持受保护的托管 vLLM。
- **后续里程碑（不在本轮）**：
  - Linux / Windows 发布目标；
  - 非 Apple 的内存模型（`crates/idoris-recommender/src/memory.rs`）；
  - Windows 进程 owner；
  - Supervisor 流式。

---

## 3. 结论二：用 Decision 3.0，只当提议者

### 为什么能用，而且合适

- **规划里早已留了位置。**
  - `Role::Decide`（`crates/idoris-policy/src/role.rs`）；
  - 接口规范中的 M6 `POST /v1/systemone`（Jev 兼容 judge，见 `docs/interfaces/iDoris-Agent24-边界与接口规范.md`）；
  - 总体规划里「System-1 决策模型」排在训练顺序第一。
- **d3 正是这个形状的现成开源件。** oMLX 已原生支持它的 `/v1/systemone`，不需要新引擎，也不违反「不进程内加载权重」。
- **d3 补上了最弱的一环。** 它的 `choice` / `score` 问题天然覆盖 intent 和 complexity（还支持多模态输入），而现在这两项分别是哈希伪 embedding 和纯 header。

### 为什么不能全面替换

- **项目不变量不允许**（`docs/iDoris-总体规划.md` §不变量）：
  - #1 顺序固定；
  - #2 隐私只能收紧；
  - #8 judge 不可用时必须拒绝并返回 `judge_unavailable`；
  - #10 模型只提议，代码做决定。

  d3 输出的是概率，会误判，不能决定隐私、预算或准入。
- **只用模型，不引入 vLLM-SR 的 Go 路由器本体。** `docs/10` 和 `docs/research/rust-base/02` 已经因为它绑定 vLLM、语言栈不符而否决过，这个结论不变。
- **d3-edge 不用于开放域**（新域只有 12.9 分）；中文能力必须先评测。

### 三种调用模式（D-5）与独立的角色/字段优先级

意图识别不再对每条请求都跑，只在调用方明确要求「交给 iDoris 选」时才运行。

**不重猜显式角色对齐已有契约，auto 的新选择接线仍须单独验收。** 接口规范 `docs/interfaces/iDoris-Agent24-边界与接口规范.md` §2.5 已写明「声明角色的调用不从内容重猜 task/role；显式 auto 才委托角色选择」，§3 写明「auto 不作为默认猜测」。现在的代码对 `idoris/daily` 这类已声明角色的请求也做意图识别（`crates/idoris-router/src/intent/mod.rs` 的 `resolve_profile_with_detector` 不看角色），与该方向不一致。

| 模式 | 怎么进入 | 意图从哪来 | 是否调用意图/复杂度判定 |
|---|---|---|---|
| **聊天模式**（默认，即不自动判定） | 无意图头，且 `model` 为具体角色（如 `idoris/daily`）、不透明模型 ID 或不填 | intent 默认 chat；保留原角色/能力及明确模型 ID | 否 |
| **自动路由模式** | `model: "idoris/auto"`，且没有 `X-iDoris-Intent` 头 | off/shadow 的哈希仅诊断；active 使用通过评测的 d3 | active 调用，shadow 旁路采样 |
| **调用方声明模式** | 带 `X-iDoris-Intent` 头（任意 `model`） | 用调用方声明的值 | 否 |

规则：

- **隐私检查与模式无关。** INSPECTED（闸一）和 `local_only` 过滤对三种模式都照常执行，不变量 #1 的顺序不变；跳过的只是「意图」这一步的识别。
- **聊天模式保留既有选择约束。** 具体角色在该角色内选；不透明模型 ID 或不填时，不受角色约束，走默认规则。写错的 `idoris/<角色>` 仍然返回 400 `unknown_role`。
- **判定来源与角色选择分开。** 显式角色不被 intent 覆盖，例如 daily + coding 仍在 daily 内选择；只有 auto 委托角色选择。调用方声明模式不是新的角色权限。B1 必须补确定性的选择接线，不能只改 YAML。
- **意图/复杂度 d3 只影响自动路由模式。** 判定模型不可用时，只有无意图头且依赖 active judge 的 `idoris/auto` 请求返回 503 `judge_unavailable`（不变量 #8 原文，不用修改），调用方可以改用具体角色重试。聊天模式和调用方声明模式不依赖这个意图 judge；B7 若启用 active 内容隐私 judge，须另行适用其失败策略。

为什么这样分：

1. 普通聊天不依赖意图判定模型；该 judge 挂了仍可聊天。B7 的 active 隐私判定是另一个独立约束。
2. 聊天模式省去一次意图判定调用；具体节省取决于 M1 Max/oMLX 实测，不能用 M5 Pro/Core ML 的中位延迟外推。不用 auto、也未启用隐私 judge 的用户不必为本方案常驻 d3。
3. 和 Agent24 的边界一致：「这是什么任务、用哪个角色」由 Agent24 判断（接口规范 §2.3「Agent24 决定 task→role」），iDoris 只在角色内选模型；auto 是给不想自己判断的调用方的可选服务。
4. 在仓库默认策略下，四类检测结果不改变选择；BM 仍会改变非 auto 请求的 profiled intent，并可能影响引用它的自定义策略。发布说明必须指出这个边界，不能宣称所有部署路由行为都不变。

代价：普通聊天里问编程问题不会被自动升级到 deep。要升级，可以明确选 `idoris/deep`，或选 `idoris/auto`（可附意图头）；给 `idoris/daily` 单独附 coding 不能获得跨角色权限。

**测试与契约影响**：现有 `conformance/tests/intent-detection.test.ts:30` 和 `crates/idoris-router/tests/intent_wiring.rs:45` 用 `idoris/daily` 并期望触发识别，这本身与规范 §2.5 的角色边界不符，需要改成 `idoris/auto`。因为是向规范对齐，不需要升契约版本，但对外可见的行为有变化，要写进 release note。

**一处有意收窄**：规范 §2.5 还允许「未声明角色的其他调用方可由静态规则/判定器选角色」（「可」，不是「必须」）。本决定选择不做：不透明模型 ID 或不填 `model` 一律按聊天模式处理，不做识别，保持简单。以后如果出现确实需要的调用方，再单独提出。

### 融合方式：三个受确定性边界约束的插槽（接口基础存在，执行接线尚未交付）

1. **意图 / 复杂度提议者**
   - 新增 `SystemOneDetector`，实现 `ProfileIntentDetector`（`crates/idoris-router/src/intent/mod.rs`），locality 为 `DetectorLocality::Loopback`（oMLX 在本机回环上，`local_only` 下也允许运行；`resolve_profile` 只会跳过 `Remote` detector）。
   - 调用 oMLX 的 `/v1/systemone`，模型用 d3-lite 或 d3-nano（按评测结果选）。
   - 问题设计：`intent: choice[chat, web_search, coding, email, agent_task]`，`complexity: choice[simple, complex]`；若用 score，必须定义到现有两档契约的映射与置信度，不能直接写入 medium。
   - **先 shadow，后生效**（总体规划 M6「入口判定层」要求）：先与现行哈希 detector 并跑，通过有界异步队列和同 record_id 的追加事件记录标签，不回写已发出的 profiled 事件、不改路由；与弱哈希的一致率只作诊断，以独立标注集的质量、校准、覆盖率和资源评测决定生效。
   - 生效后置信度低于阈值：视为「未命中」，保留默认 `chat`（与现有 miss 语义一致）。模型给的阈值/置信度只能收紧，按不变量 #8 做 clamp。
   - 只在自动路由模式（`idoris/auto`）下运行，见上面的「三种调用模式」。
   - 生效后 judge 不可用或超时：`idoris/auto` 请求按**不变量 #8 拒绝**，返回 503 `reason_code: judge_unavailable`，不回落、不静默。聊天模式和调用方声明模式不调用意图 judge，不受其故障影响。见决策点 D-1。
   - 配套改动：
     - 扩展 detector 的输出，让它能写 `complexity`；
     - `resolve_profile` 改为按配置选择 detector（现在硬编码 `default_detector()`）；
     - header 显式给出的值始终优先。intent header 存在时整个 intent/complexity judge 不运行，缺失 complexity 保留默认；只有 complexity header 时可检测 auto 的 intent，但不得覆盖 complexity。需要保存各字段来源，当前 ParsedProfile 尚不能区分显式 simple 与默认 simple。
2. **L1 隐私收紧器**（M5 合并后）
   - 用 d3 的 noul（布尔概率）问题判断「是否含个人 / 敏感数据」。
   - 结果只能把 privacy 往 `local_only` 方向收紧，写入 `content_tightening` 或 INSPECTED 的有效隐私。
   - L0 规则和 L2 `decide_action` 不变。
   - REDACT 仍按 e12dd19 的决定执行：每个请求都要在本地控制台单独确认。
   - off/shadow 不改变 L0；active 时需要 L1 的请求遇到判定故障必须显式拒绝，不能退回 L0 后继续出站。只能收紧不等于故障放行。此判断与意图模式独立，先对齐 M5 最终契约。
3. **对外 `/v1/systemone`（M6）**
   - router 增加该路由，经认证、scope、准入和 SystemOneClient 调用承载 `Role::Decide` 的模型，供 Agent24 使用；不能把现有仅 chat 的 ChatProxy 直接当作通用透传。
   - judge 不可用时按不变量 #8 返回 `judge_unavailable`。

### 前置缺口

B1 不是补几条 YAML：当前 tier 只有 local/remote/lora，deep 是角色；first-match 的 local_only 首条遮住后续意图规则，capability/load 输出也只是元数据。必须在隐私硬过滤后的角色/能力阶段接入确定性 auto 选择，保持显式角色及真实模型 override，不把工具能力写成不存在的枚举。对齐 PR #510 的显式 role→model 绑定和 load/chat/response 身份后，才可承诺 auto 选 deep。

**上线时机：与 d3 生效（B5）同一版本。** 如果提前上线，驱动这些规则的仍是不准的哈希识别器，误判会直接把请求送错模型。shadow 运行不需要这些规则。即使同版本上线，off/shadow 仍必须禁止哈希结果进入新语义规则；Header 来源可用，ActiveJudge 需通过评测和校准门槛。

### 评测门槛（默认开启前必须通过）

- **数据集**：至少 200 条中英分层标注样本，含独立留出的测试集；不只扩写哈希模板，分别评估 intent 与两档 complexity。
- **对比对象**：默认/规则、HashingEmbedder、d3-lite、d3-nano、常驻聊天模型模拟判定。模拟评测胜出也不自动授权生产模拟；仍遵守显式许可与 judge_mode/calibrated。
- **指标**：准确率、p50 / p95 延迟（M1 Max 本机 oMLX）、常驻内存。
- **一致性**：选项顺序置换后结论不变（M6 要求的置换一致性测试）；按置信度分箱报告校准误差（ECE）。
- **通过线**：准确率显著高于哈希基线，p95 < 150ms，内存能与主聊天模型共存。
- **未通过**：只保留插槽，默认不开启。

---

## 4. 工作计划与决策点

拆分后的工作包与任务卡见 [`agent/plans/runtime-portability-and-decision-model.md`](agent/plans/runtime-portability-and-decision-model.md)，台账索引见 [`agent/tasks.md`](agent/tasks.md) 的「F1.2+ / F2.4+」一节。

- **WP-A 跨平台推理后端**：A1 ChatProxy Bearer → A2 vLLM runtime kind → A3 vllm-metal 对比 spike → A4 OllamaAdapter → A5 后端探测接线 → A6 跨平台发布（里程碑占位）→ A7 oMLX 依赖风险与退出路径。
- **WP-B 决策模型提议者**：BM 模式分离（意图识别只在 `idoris/auto` 下运行）→ B0 d3 spike → B2 评测集与评测脚本 / B3 `/v1/systemone` 契约类型 → B4 shadow detector → B5 切为生效（B1 路由规则补 intent/complexity 与它同一版本上线）→ B6 对外 `/v1/systemone` → B7 L1 隐私收紧器（M5 后）→ B8 自有决策模型（M7 占位）。

已确认的方向与本轮修订边界：

| 编号 | 问题 | 建议 |
|---|---|---|
| D-1 | d3 生效后 judge 不可用时怎么办？ | **已确认（2026-10-15，依赖 D-5）**：只有无意图头且依赖 active judge 的 `idoris/auto` 请求返回 503 `judge_unavailable`，不变量 #8 不修改；聊天模式和调用方声明模式不调用意图 judge，不受其故障影响。不再提供 `fallback_hashing` 配置 |
| D-2 | M6 原设想「复用常驻小模型做 logit 型 System-1」，d3 是专用模型，要多占内存 | 先评测固定 MLX revision 的 d3-lite（Core ML 发布物约 3.7GB 不是其内存预算）；B2 评测同时测「常驻聊天模型 + `/v1/systemone` 模拟判定」作对照，用数据决定 |
| D-3 | Windows 默认后端选 Ollama（vLLM 无官方原生 Windows，WSL2 增加安装与运维步骤） | **已确认（2026-10-15）**。代价：Ollama 用 GGUF，与 MLX 权重不通用，catalog 按后端分列模型 ID；Ollama 能否跑 d3 未核实，Windows 上的决策模型暂时空缺 |
| D-4 | 路由规则何时生效 | B1/B5 同版本上线，同时按字段来源隔离 off/shadow，不能只依赖发布时间 |
| D-5 | 聊天模式与意图路由分开？ | **已确认（2026-10-15）**：分成聊天 / 自动路由 / 调用方声明三种模式，意图识别只在 `idoris/auto` 下运行，见 §3「三种调用模式」 |

## 5. 本轮独立复核与修订依据（2026-10-15）

本轮核对的是 `origin/main@01133d46e797615d9baf0703f60463899ec0078f` 和 PR #524 原 head `83cdb7b514396c7138d6fa417cb93ab1410eaf5c`。以下是文档修订依据，不是实现验收，也不替代 prdaemon 的独立审批。

| 发现 | 修订与验收归属 |
|---|---|
| B1 混淆 tier 与 role；first-match 的隐私规则遮住意图规则；capability/load 仍是元数据 | B1 扩为隐私硬门后的确定性 auto 选择接线，并与 #510 角色绑定核对；验证本地 deep 的真实执行身份 |
| 同版本交付仍允许 off/shadow 的哈希结果驱动新规则 | B1/B5 按 Header/ActiveJudge 来源限制规则输入，补运行模式切换负例 |
| B3 的数组/options/bool 不是上游 System One 协议 | 固定上游源码及 B0 fixture 后冻结对象/criteria/noul 类型，B3 不再与 B0 无依赖并行 |
| 显式角色与意图声明不是同一维度，complexity 来源尚未保留 | BM/B5 维护字段优先级，保留 simple/complex 两档，明确 daily+coding 不跨角色 |
| ChatProxy 认证不覆盖进程 runtime | A1 同时覆盖 readiness/chat，B3 复用认证；缺凭证失败、无凭证不泄露调用方 key |
| B7 的“L1 故障等于 L0”不能满足 active 判定的 fail-closed | active 的必要判定故障显式拒绝；off/shadow 单独处理，对齐 M5 后重新审核 |
| shadow、常驻 d3 可能绕开资源账；同端点能以聊天模型模拟 | B4/B5 有界执行且经过 Supervisor 准入；B3/B6 验证模型种类，未授权不模拟，明确 judge_mode/calibrated 的来源 |

**外部证据分级**：

- oMLX 已不只有 README 声称支持 d3：固定源码 [`33e0b0c` 的 DecisionEngine](https://github.com/jundot/omlx/blob/33e0b0c20811b41928cfd85b7e0fe346c7b7c1ed/omlx/engine/decision.py) 注册了 D3Model；[请求类型](https://github.com/jundot/omlx/blob/33e0b0c20811b41928cfd85b7e0fe346c7b7c1ed/omlx/api/systemone_models.py) 定义 questions 对象及 noul/choice/score；[端点实现](https://github.com/jundot/omlx/blob/33e0b0c20811b41928cfd85b7e0fe346c7b7c1ed/omlx/server.py) 区分专用 decision engine 与聊天模型。**源码存在不证明本机安装版本具备此能力**；B0 须验证版本、量化、实际 wire 和中英文质量。
- [vLLM 官方安装文档](https://docs.vllm.ai/en/stable/getting_started/installation/gpu/) 确认不原生支持 Windows；[vllm-metal 官方文档](https://docs.vllm.ai/projects/vllm-metal/en/latest/) 确认 MLX 计算后端和实验性 paged attention。保留 oMLX 是当前成本/证据下的选择，不是 vllm-metal 性能较差的既定结论。
- [Ollama FAQ](https://docs.ollama.com/faq) 的无限 keep_alive 是驻留策略；内存压力下是否能兑现 pin 仍要 A4 验证。Windows 默认 Ollama 是方向，A6 发布与真实 Windows 验收仍未完成。
- [Apple Silicon 实测](https://vllm-semantic-router.netlify.app/blog/decision-3-0-on-apple-silicon/) 的 53/69ms 与 3.7/10.5GB 属于 M5 Pro/Core ML，不是 M1 Max/oMLX 的性能或常驻内存。B2 应评估端到端收益，不能仅凭胜过弱哈希基线就默认启用 d3。

不变量逐项判断：#1 由隐私→角色/能力→预算→容量的执行顺序保证；#2 要求候选隐私只收紧；#8 要求必要 judge 故障显式失败、模拟须许可；#10 要求模型仅提供标签/分布，代码执行角色、身份和准入判决。B7 的 active 行为会影响聊天隐私检查，不能与 D-5 的意图免判定保证混为一谈。

## 附录 A · Spike 记录

2026-10-15 的 B0 没有跑成延迟和准确率。本机正在使用的 oMLX 是 0.6.4（`/Applications/oMLX.app`，`~/.omlx/bin/omlx --version`），其 `server.py` 没有 `POST /v1/systemone`，包内也没有 `systemone` 字符串。`127.0.0.1:8088` 在听，未带凭证的 `/v1/models` 返回 401。`~/.omlx/models` 里没有 d3 权重。上游 commit `33e0b0c` 的 DecisionEngine 不在这份安装里。

因此没有请求样例、p50/p95 或常驻内存数字。依赖 d3 的 B2/B3/B4/B5 先暂停。BM 和 WP-A 继续。升级本机 oMLX 或下载 d3 会改动正在使用的推理服务，本次没有做。细节见 [`agent/evidence/b0-d3-omlx-spike.md`](agent/evidence/b0-d3-omlx-spike.md)。

## 附录 B · 后端比较记录

（待 A3 填写：固定模型与量化、引擎版本、相同请求集、冷/热缓存条件、TTFT、decode、并发吞吐、内存与冷启动。这里没有性能结论。）

## 来源

- vllm-metal：[文档](https://docs.vllm.ai/projects/vllm-metal/en/latest/) · [GitHub](https://github.com/vllm-project/vllm-metal) · [Docker Model Runner 博文](https://www.docker.com/blog/docker-model-runner-vllm-metal-macos/)
- vLLM 的 Windows 现状：[官方安装要求](https://docs.vllm.ai/en/stable/getting_started/installation/gpu/)
- oMLX：[GitHub](https://github.com/jundot/omlx)
- Decision 3.0：[Mushroom 博文](https://blog.mushroom.cv/blog/decision30-multimodal-system-one/) · [Apple Silicon 博文](https://vllm-semantic-router.netlify.app/blog/decision-3-0-on-apple-silicon/) · [HF d3](https://huggingface.co/vllm-sr/d3) · [HF d3-edge](https://huggingface.co/vllm-sr/d3-edge) · [vllm-project/semantic-router](https://github.com/vllm-project/semantic-router) · [PingWest 报道](https://www.pingwest.com/w/318299)
- Ollama `/v1/systemone`：[runtimewire](https://runtimewire.com/article/ollama-local-decision-models-systemone)

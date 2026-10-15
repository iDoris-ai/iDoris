# 跨平台推理后端 + Decision 3.0 决策模型提议者 —— 工作计划

> 来源决策：[`../../18-推理后端与决策模型-vLLM与Decision3.0评估.md`](../../18-推理后端与决策模型-vLLM与Decision3.0评估.md)（2026-10-15 owner 接受）
> 基线：`origin/main@01133d4`。路线图位置：WP-A 属于 F1.2 的后续，WP-B 属于 F2.4 的后续和 M6「入口判定层」。
> 协作规则见 [`../COLLAB.md`](../COLLAB.md)。编码由本机子 agent 完成，主会话负责规划、测试和验收。

## 执行约定

- 每个任务开一个分支 `feat/<wp>-<id>-<短名>`（例如 `feat/runtime-a2-vllm-kind`），PR 直接提到 `main`，单个 PR 不超过 300 行（不含测试 fixture）。
- 每个修复或新行为必须带回归测试，并说明「去掉实现后测试会失败」。
- 改到契约（`packages/contracts/schema/*.json`）时，同步运行 `node scripts/gen-contracts.mjs` 和 `node scripts/check-contract-drift.mjs`；对 Agent24 可见的变更要升契约版本（`scripts/contract-version.mjs`）。
- 依赖真实引擎的任务（标 🖥）在本机 M1 Max 上做实机验证，结果贴进 PR。CI 只跑 wiremock 和 mock。
- 公共门禁：`cargo fmt --check && cargo clippy --workspace -- -D warnings && cargo test --workspace`；涉及 TS 或 conformance 时再跑 `pnpm -r test` 和 `scripts/conformance-rust.sh`。

## 依赖图

```
WP-A  A1 ─┐
          ├─> A2 ──> A3🖥
          │
          A4 ──> A5 ──> A6(里程碑占位)
          A7（文档/护栏，独立）

WP-B  BM（模式分离）──────────────┐
      B0🖥 ─┬─> B2 ──────────────────┤
            └─> B3 ──> B4(shadow) ────┴─> B5 + B1（同一版本上线）──> B6
                                            └──> B7（依赖 M5 闸一合并）
                                            B8（M7 占位）
```

建议顺序：先做 BM、A1、A4、B0、B3（互不依赖，可并行），再做 B2、B4，之后 A2 → B1 与 B5（同一版本）→ A5 → B6，A3/A7 穿插进行，B7 等 M5 合并后再开。

---

## WP-A 跨平台推理后端

### A1 ChatProxy 支持可选 Bearer  `READY`
- **优先级**：high（A2 的前置，也是 vLLM `--api-key` 的前置）
- **目标**：resident `http_service` 卡可以声明凭证引用，`ChatProxy` 转发时带上 `Authorization: Bearer`。
- **开发范围**：
  - 组件卡契约增加可选字段 `auth: { kind: bearer, env: <VAR> }`，只允许引用环境变量名，不允许写明文。
  - `crates/idoris-router/src/proxy.rs` 在构造上游请求时注入该头；缺失的环境变量在启动时就拒绝（不变量 #8：配置写了但不生效，必须拒绝启动）。
- **明确不做**：不接 Keychain（属于闸二 / M5）；不改 Supervisor 路径。
- **依赖**：无
- **验收**：wiremock 断言上游收到的 Bearer 正确；卡声明了 `auth`、但环境变量缺失时启动退出码非 0；未声明 `auth` 的卡上游收不到 `Authorization` 头（回归测试）。
- **涉及文件**：`crates/idoris-contracts/src/component_card.rs`、`packages/contracts/schema/component-card.schema.json`、`crates/idoris-router/src/proxy.rs`、启动校验
- **风险/回滚**：纯新增可选字段，回滚只需删掉该字段。

### A2 `LocalRuntimeKind::Vllm`  `READY`
- **优先级**：high
- **目标**：iDoris 能自己拉起并托管 `vllm serve`（Linux CUDA；Mac 上的 vllm-metal 用同一个 CLI），一个进程一个模型。
- **开发范围**：
  - `crates/idoris-upstream/src/local_runtime.rs`：增加 `Vllm` 变体；argv 为 `serve <model> --host 127.0.0.1 --port N --served-model-name <model_id>`。
  - `model` 允许是绝对路径，也允许是 HF repo id。现有构造函数要求绝对路径，需要按 kind 分别校验；repo id 只允许 `[A-Za-z0-9._-]+/[A-Za-z0-9._-]+` 这种形式。
  - 不做 `.gguf` 校验。
  - `crates/idoris-router/src/runtime_launch_config.rs` 增加 `kind: vllm`。
  - `local_http_runtime.rs` 发请求时，`model` 字段用 served name。
- **明确不做**：不用 vLLM 的 sleep/wake 端点（只在 dev 模式下可用）；不做多 GPU 或 tensor parallel 参数。
- **依赖**：A1（选用 `--api-key` 时）
- **验收**：argv 快照测试（不经 shell、绑定固定为 loopback）；repo id 和路径各有正例和负例；用伪造的 vllm 可执行文件（对 `/v1/models` 返回 200 的脚本）跑通启动 → 就绪 → chat → 停止。
- **涉及文件**：`local_runtime.rs`、`local_http_runtime.rs`、`runtime_launch_config.rs`、`config/examples/multibackend/`
- **风险/回滚**：枚举新增变体，回滚即删除；当前已有的 match 必须保持穷尽（clippy 会检查）。

### A3 🖥 vllm-metal 与 oMLX 实机对比 spike  `BACKLOG`
- **优先级**：medium
- **目标**：拿数据检验结论一，而不是只靠推理论证。
- **开发范围**：在 M1 Max 上用同一个模型（MLX 8-bit 版）分别跑 oMLX 和 vllm-metal，比较：
  - TTFT、decode tok/s、并发 4 路时的吞吐；
  - 常驻内存；
  - 长上下文（32k）下的 TTFT，用来检验 oMLX SSD KV cache 的实际效果；
  - 冷启动时间。
- **明确不做**：不改代码；不改默认后端。
- **依赖**：A2
- **交付物**：结果写进 docs/18 附录 B。
- **复审触发条件**：如果 vllm-metal 在两项及以上指标上领先超过 30%，就重开结论一（另走一个提案 PR）。

### A4 `OllamaAdapter`  `READY`
- **优先级**：medium（Windows 默认后端）
- **目标**：Supervisor 能管理 Ollama 的加载、卸载、pin 和状态。
- **开发范围**：新增 `crates/idoris-upstream/src/ollama/`，实现 `RuntimeAdapter`：
  - `list`：`GET /api/tags`
  - `status`：`GET /api/ps`（含显存和内存大小）
  - `load`：`POST /api/generate {model, keep_alive}`，空 prompt
  - `unload`：`keep_alive: 0`
  - pin：`keep_alive: -1`
  - `chat`：`/v1/chat/completions`
  
  同时在 `factory.rs` 增加 `"ollama"` 分支。变更类请求超时一律映射为 `LoadUnconfirmed`（沿用 K09 语义）。
- **明确不做**：不做 Ollama 模型拉取（`/api/pull`），模型由用户安装；不支持 rerank（Ollama 没有这个端点），能力表里标为不支持。
- **依赖**：无（与 A1/A2 并行）
- **验收**：用 wiremock 跑完 `RuntimeAdapter` 的全部方法；load 超时返回 `load_unconfirmed`；unload 后用 `/api/ps` 确认的语义与 K11 一致（确认失败就拒绝准入）。
- **涉及文件**：`crates/idoris-upstream/src/ollama/*`、`factory.rs`、`crates/idoris-upstream/tests/ollama_*.rs`
- **风险/回滚**：新模块，回滚即删除。

### A5 后端探测接线 + NVIDIA 探测  `BACKLOG`
- **优先级**：medium
- **目标**：首次启动时按平台给出默认后端卡：Mac 用 oMLX，Linux + NVIDIA 用 vLLM，Windows 用 Ollama，其余用 llama.cpp。
- **开发范围**：
  - `crates/idoris-upstream/src/detect.rs:84`：实现 NVIDIA 探测（`nvidia-smi --query-gpu=name,memory.total --format=csv,noheader`，带超时；找不到命令视为没有 GPU）。
  - 更新 `detect_backend` 的矩阵：Windows 默认 Ollama，`implemented` 改为真实值。
  - 让 router 启动时在没有显式组件目录的情况下使用 `detect_backend` 生成的默认卡；显式配置始终优先。
- **明确不做**：不自动安装任何引擎；不探测 AMD/Intel GPU（留给 A6）。
- **依赖**：A2、A4
- **验收**：注入 host facts 的表驱动测试覆盖 4 个平台分支；`nvidia-smi` 超时或不存在时视为没有 GPU；`IDORIS_COMPONENTS_DIR` 存在时不走探测。

### A6 跨平台发布（里程碑占位）  `BACKLOG`
- 包括：Linux x86_64/aarch64 和 Windows 的 release target；非 Apple 内存模型（`crates/idoris-recommender/src/memory.rs`）；Windows 进程 owner（目前是 `cfg(not(unix))` 存根）；Supervisor 流式（`supervisor_stream.rs`）。
- 等到出现真实的 Linux/Windows 消费者时再拆任务（「先有消费者再有提供者」）。

### A7 oMLX 依赖风险与退出路径  `READY`
- **优先级**：low
- **目标**：把「不换 oMLX」的风险写成可执行的护栏，而不是停留在论证里。
- **开发范围**：
  - 在 conformance 里标出哪些用例只依赖 OpenAI-compat 子集，哪些依赖 oMLX 专有端点，并在 docs/18 附上依赖清单。
  - 写一份「oMLX 不可用时的退路」runbook：Mac 上改用 Ollama 或 `mlx_lm.server` 的卡配置样例，放在 `config/examples/`。
  - `version_pin` 升级流程照 FU-16 的复测清单执行。
- **验收**：样例配置能通过 `idoris` 的启动校验（`cargo test` 里加载样例卡的测试）。

---

## WP-B Decision 3.0 决策模型提议者

### BM 模式分离：意图识别只在 `idoris/auto` 下运行  `READY`
- **优先级**：high（B4/B5 的前置；先做它，d3 的影响范围从一开始就只限于 auto 模式）
- **目标**：落实 docs/18 §3「三种调用模式」（D-5）：聊天模式不识别意图，自动路由模式识别，调用方声明模式用 header。同时修复现有代码与接口规范 §2.3 第 115 行（「声明角色的调用不从内容重猜」）的不一致。
- **开发范围**：
  - `crates/idoris-router/src/intent/mod.rs` 的 `resolve_profile_with_detector`：在现有的跳过条件里加上「`parsed.role` 不是 `Some(Role::Auto)` 就跳过」。
  - TS 参考实现 `packages/router/src/intent.ts` 同步修改。
  - 把 `conformance/tests/intent-detection.test.ts`、`crates/idoris-router/tests/intent_wiring.rs` 里期望触发识别的用例改成 `idoris/auto`，并为聊天模式补反例。
  - 向规范对齐，不升契约版本；对外可见的行为变化写进 release note。
- **明确不做**：不改隐私检查和 `local_only` 过滤；不改聊天模式的选模型逻辑；不改规则引擎。
- **依赖**：无
- **验收**：
  - `idoris/daily`、不透明模型 ID、不填 `model` 三种情况下 detector 调用 0 次（mock 计数）；
  - `idoris/auto` 且无意图头时 detector 被调用；带意图头时任意 `model` 都调用 0 次；
  - 三种模式下 `local_only` 都只剩本地候选（隐私检查不随模式跳过）；
  - 聊天模式的 `decided` 结果与改动前一致（对照测试）。
- **风险/回滚**：对外可见的行为变化仅限「不带意图头的非 auto 请求不再自动识别意图」；由于现有路由规则不引用识别出的意图，路由结果不变。回滚即删掉这一个跳过条件。

### B0 🖥 d3 在 oMLX 上的实机 spike  `READY`
- **优先级**：high（B2、B4 的前置）
- **目标**：确认 `/v1/systemone` 的线格式，以及 d3-lite / d3-nano 对中文输入的实际表现。
- **开发范围**：
  - 在本机 oMLX 上加载 d3-lite（固定 revision）。
  - 用 curl 发中英文各 10 条请求，题型包括 `choice`、`bool`、`score`。
  - 记录请求和响应样例、p50/p95、常驻内存、和聊天主模型同时常驻时的内存压力。
- **明确不做**：不改代码。
- **交付物**：填写 docs/18 附录 A（oMLX 版本、d3 revision、样例、数字）。
- **否决条件**：中文 `choice` 的准确率接近随机，或者 p95 > 300ms，则 WP-B 暂停，在附录里写明原因。

### B1 路由规则补上 intent 和 complexity  `BACKLOG`（与 B5 同一版本上线）
- **优先级**：high（不做这一步，任何分类器都不会产生可观测的效果）
- **上线时机**：PR 可以提前写好，但必须和 B5 在同一版本合并。提前上线的话，驱动规则的仍是哈希识别器，误判会直接把请求送错模型；B4 的 shadow 运行只记录一致率，不需要这些规则。
- **目标**：`config/routing-policy.yaml` 的规则引用检测到的 intent（`coding`、`agent_task` 等）和 `complexity: complex`，并映射到对应的层级。
- **开发范围**：
  - 增加规则，例如 `coding` → deep，`agent_task` → deep，`web_search` → 需要工具能力；`local_only` 规则仍然排在第一（不变量 #1、#2）。
  - 更新 `conformance/tests/default-routing-policy.test.ts`。
- **明确不做**：不改规则引擎（`routing_policy.rs`）；不改 detector。
- **依赖**：B5（同一版本上线）
- **验收**：conformance 断言：`coding` 意图选中 deep 层；`local_only` 加 `coding` 时仍然只剩本地候选；`X-iDoris-Reason` 带上匹配到的 `rule_id`。
- **风险**：改的是默认路由行为，属于用户可见的变化，要写进 release note。

### B2 评测集与评测脚本  `BACKLOG`
- **优先级**：high
- **目标**：用数据决定 d3 是否生效、选哪个尺寸。
- **开发范围**：
  - 在 `eval/decision/` 下建立标注集：至少 200 条中英混合样本，覆盖 5 类 intent 和 3 档 complexity。每类至少 20 条，并包含负对照和易混样本。
  - 编写离线评测脚本，同时跑三类对象：哈希 detector、d3-lite / d3-nano（通过 `/v1/systemone`）、对照组「常驻聊天模型模拟判定」（对应决策点 D-2）。
  - 输出准确率、混淆矩阵、ECE、选项置换一致率、p50/p95。
- **明确不做**：不进 CI 的必跑项（依赖真实模型）；不使用真实用户数据。
- **依赖**：B0
- **验收**：评测脚本在 mock 后端上跑通，CI 里有冒烟测试；实机报告写进 docs/18 附录 A。
- **通过线**：准确率比哈希基线高 15 个百分点以上，并且在配对比较（McNemar）下跨过 docs/16 ST-2 的统计门槛（n=200 时非配对比较分辨不出 7pp 以内的差距）；p95 < 150ms；置换一致率 ≥ 95%。

### B3 `/v1/systemone` 契约类型  `READY`
- **优先级**：medium
- **目标**：把 TypeSafe System One 线格式定义为 iDoris 契约，供 B4（客户端）和 B6（服务端）共用。
- **开发范围**：
  - 新增 JSON Schema `packages/contracts/schema/systemone.schema.json`，内容包括：
    - 请求：`model`、`state`、`questions[{id, type: choice|bool|score, options}]`
    - 响应：每个问题的概率分布或期望索引
    - 错误体：`reason_code: judge_unavailable`，以及 `judge_mode`、`calibrated`
  - 生成 Rust 和 TS 类型。
  - 在 `crates/idoris-upstream` 增加 `SystemOneClient`：只做 loopback，带超时，复用 K01 的 `redirect(none)` 和 `no_proxy` 设置。
- **明确不做**：不做多模态输入（图片、视频留到后续）；不暴露对外路由（那是 B6）。
- **依赖**：B0 确认的线格式（可以先按公开格式写，B0 完成后再校正）
- **验收**：契约漂移检查通过；客户端用 wiremock 测试正常、超时、5xx、格式错误四种情况；外部重定向不被跟随。

### B4 `SystemOneDetector`（shadow 模式）  `BACKLOG`
- **优先级**：medium
- **目标**：d3 与现行哈希 detector 并行运行，只记录结果，不改变路由（对应总体规划 M6 的「shadow 模式」）。
- **开发范围**：
  - 在 `crates/idoris-router/src/intent/` 新增 `SystemOneDetector`，实现 `ProfileIntentDetector`，locality 为 `Loopback`。
  - `resolve_profile` 改成按配置选择 detector（现在硬编码 `default_detector()`），配置项为 `intent_judge: { mode: off|shadow|active, model, timeout_ms, min_confidence }`。
  - shadow 模式下 d3 异步运行，不阻塞请求。`profiled` 事件新增 `shadow_intent`、`shadow_agree` 两个标量字段（需要扩充 `crates/idoris-tenancy/src/event_log/mod.rs` 的 SCALAR_KEYS 白名单），只记录标签，不记录内容（不变量 #5、#15）。
- **明确不做**：shadow 模式不改变 `task.intent`；不做 complexity（在 B5 做）。
- **依赖**：B3、BM
- **验收**：
  - shadow 模式下，路由结果和请求头与 `off` 模式逐字节相同（对照测试）；
  - judge 超时不影响响应延迟（mock 延迟 5s，响应仍在基线范围内）；
  - 事件里不出现消息内容（沿用现有的内容键黑名单测试）；
  - header 显式给出 intent 时不调用 judge（mock 断言调用 0 次）；
  - 聊天模式（非 `idoris/auto`）下不调用 judge（mock 断言调用 0 次）。shadow 的一致率数据只来自 auto 流量，样本不足时以 B2 的离线评测为准。

### B5 切为生效：intent + complexity  `BACKLOG`
- **优先级**：medium
- **目标**：在 `mode: active` 下，d3 的结论写进 `task.intent` 和 `task.complexity`，由 B1 的规则真正影响路由。
- **开发范围**：
  - 扩展 detector 输出，使其也能给出 complexity（`IntentHit` 增加可选的 `complexity`）。
  - 置信度低于 `min_confidence` 时视为未命中，intent 保持默认 `chat`；配置里的阈值只能调高，不能低于代码下限（clamp）。
  - judge 不可用时（D-1）：`idoris/auto` 请求返回 503 `judge_unavailable`，不回落；聊天模式和调用方声明模式不调用 judge。
  - 启用 auto 时，在 oMLX 里把 d3 设为常驻（pin），启动时检查它是否就绪，降低被内存压力挤掉的概率。
- **明确不做**：不让 d3 影响 privacy、预算或准入（不变量 #1、#10）。
- **依赖**：B4，以及 B2 达到通过线；与 B1 同一版本上线
- **验收**：
  - `local_only` 加 `active` 时，d3 仍可运行（Loopback），远程候选仍为 0；
  - 低置信度时结果与 miss 相同；
  - judge 不可用时：`idoris/auto` 返回 503 和 `judge_unavailable`；同一时刻 `idoris/daily` 请求正常返回 200；
  - property test：任何 d3 输出都不会让候选集合多出非 local 的卡。

### B6 对外 `/v1/systemone`（M6）  `BACKLOG`
- **优先级**：low
- **目标**：Agent24 的审批门、Evolver 等业务模块可以通过 iDoris 使用决策端点。
- **开发范围**：
  - 在 router 增加 `POST /v1/systemone` 路由，走 auth 和 virtual key scope，新增 scope `systemone`。
  - 选中承载 `Role::Decide` 的卡，请求转发给它。
  - 写入 Event Log（只记标签和概率摘要）。
  - judge 不可用时返回 `judge_unavailable`。只有调用方显式声明接受模拟判定时，才用宿主模型模拟，并在响应里标明 `judge_mode: simulated, calibrated: false`（不变量 #8）。
- **明确不做**：不做多模态输入；不做决策账本的回放 UI。
- **依赖**：B3、B5
- **验收**：
  - 没有对应 scope 的 key 返回 403；
  - 没有 Decide 卡时返回 503 和 `judge_unavailable`；
  - 不声明接受模拟时，绝不调用聊天模型（mock 调用 0 次）；
  - conformance 中 `known-spec-conflicts` 里关于 `/v1/systemone` 的条目移除；
  - Agent24 契约版本 +1。

### B7 L1 隐私收紧器（M5 之后）  `BLOCKED`（等 M5 闸一合并）
- **目标**：用 d3 的 `bool` 题「是否含个人或敏感数据」作为 L1，只能把 privacy 往 `local_only` 方向收紧。
- **开发范围**：结果写进 `content_tightening` 或 INSPECTED 的有效隐私；先 shadow 后生效，流程同 B4/B5。
- **明确不做**：不改 L0 规则和 L2 `decide_action`；REDACT 仍然遵守 e12dd19「每个请求在本地控制台确认」的决定。
- **验收**：property test 证明 L1 永远不会放宽隐私；L1 不可用时行为与「只有 L0」完全一致（L1 只能收紧，所以缺席就等于不收紧，这一点要在测试里明确断言）。

### B8 自有决策模型（M7 占位）  `BACKLOG`
- d3 是 Apache-2.0，可以作为 M7「先训练决策模型」的 base 或教师。等轨迹金库（M6）和策展流水线就绪后再拆任务。

---

## 决策点（PR 评审时请确认）

| 编号 | 问题 | 建议 |
|---|---|---|
| D-1 | d3 生效后 judge 不可用：拒绝还是回落？ | **已确认（2026-10-15，依赖 D-5）**：只有 `idoris/auto` 返回 503 `judge_unavailable`；不变量 #8 不修改；不提供 `fallback_hashing` |
| D-2 | d3 专用模型和「复用常驻小模型」之间的取舍 | B2 同时评测两者，用数据决定 |
| D-3 | Windows 默认用 Ollama | **已确认（2026-10-15）**；catalog 按后端分列模型 ID；Ollama 能否跑 d3 未核实，Windows 上的决策模型暂时空缺 |
| D-4 | B1 改变了默认路由行为 | **已确认（2026-10-15）**：写进 release note，并与 B5 同一版本上线 |
| D-5 | 聊天模式与意图路由分开 | **已确认（2026-10-15）**：三种调用模式，意图识别只在 `idoris/auto` 下运行，由 BM 落实 |

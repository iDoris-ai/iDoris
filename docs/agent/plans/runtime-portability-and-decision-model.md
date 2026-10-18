# 跨平台推理后端 + Decision 3.0 决策模型提议者 —— 工作计划

> 来源决策：[`../../18-推理后端与决策模型-vLLM与Decision3.0评估.md`](../../18-推理后端与决策模型-vLLM与Decision3.0评估.md)（2026-10-15 owner 接受）
> 基线：`origin/main@01133d4`。路线图位置：WP-A 属于 F1.2 的后续，WP-B 属于 F2.4 的后续和 M6「入口判定层」。
> 协作规则见 [`../COLLAB.md`](../COLLAB.md)。编码由本机子 agent 完成，主会话负责规划、测试和验收。

> **实施门禁（2026-10-15 更新）**：本 PR 仅提交设计供批准，不授权开工。必须先完成 prdaemon 的 Codex astra 审核，再获得 owner 明确的开发许可，才能启动实现、实机 spike 或实现子 agent。下列 `READY` 只表示任务依赖就绪，不代表已获开工许可。使用 default/standard 服务，禁止 Fast/priority。

## 执行约定

- 每个任务开一个分支 `feat/<wp>-<id>-<短名>`（例如 `feat/runtime-a2-vllm-kind`），PR 直接提到 `main`，单个 PR 不超过 300 行（不含测试 fixture）。
- 每个修复或新行为必须带回归测试，并说明「去掉实现后测试会失败」。
- 改到契约（`packages/contracts/schema/*.json`）时，同步运行 `node scripts/gen-contracts.mjs` 和 `node scripts/check-contract-drift.mjs`；对 Agent24 可见的变更要升契约版本（`scripts/contract-version.mjs`）。
- 依赖真实引擎的任务（标 🖥）在本机 M1 Max 上做实机验证，结果贴进 PR。CI 只跑 wiremock 和 mock。
- 公共门禁：`cargo fmt --check && cargo clippy --workspace -- -D warnings && cargo test --workspace`；涉及 TS 或 conformance 时再跑 `pnpm -r test` 和 `scripts/conformance-rust.sh`。

## 依赖图

```
WP-A  A1 -> A2 -> A3（Mac benchmark）
      A2 + A4 -> A5 -> A6（里程碑占位）
      A7 独立

WP-B  B0 + A1 -> B3
      B0 -> B2
      BM + B3 -> B4（shadow）
      B2 达标 + B4 + #510 契约对齐 -> B1/B5（同版本）-> B6
      B3/B5 + M5 闸一合并 + 失败语义复审 -> B7
      B8（M7 占位）
```

获得开发许可后的顺序：先并行 BM、A1、A4、B0；B3 的线格式以 B0 固定版本实测为前置，不再宣称与 B0 独立。B0 通过后并行 B2/B3，再做 B4；B2/B4 达标后交付 B1+B5 的完整切片。A2 在 A1 后，A5 在 A2/A4 后；A3/A7 穿插进行。B1/B5 还必须对齐角色绑定 PR #510 的最终契约。B0 否决只暂停依赖 d3 的后续工作，不阻塞独立的 BM 和 WP-A。A6/B8 是未拆分里程碑，不算本轮可交付任务。

---

## WP-A 跨平台推理后端

### A1 HTTP/runtime 共用可选 Bearer  `READY`
- **优先级**：high（A2 的认证前置）
- **目标**：resident HTTP 和进程托管 runtime 都能使用显式配置的 Bearer 凭证，不能只修 `ChatProxy` 却宣称 A2 已支持认证。
- **开发范围**：
  - 组件卡契约增加可选字段 `auth: { kind: bearer, env: <VAR> }`，只允许引用环境变量名，不允许写明文。
  - `crates/idoris-router/src/proxy.rs` 在构造上游请求时注入该头；缺失的环境变量在启动时就拒绝（不变量 #8：配置写了但不生效，必须拒绝启动）。
  - `LocalHttpRuntimeConfig` 和可信 runtime 启动配置传递凭证引用；`LocalHttpRuntimeAdapter` 的 `/v1/models` 探测与 chat 都使用同一凭证。A2 的引擎端认证配置与客户端一致；不得将密钥写入卡、日志或测试快照。
  - B3 复用同一凭证解析方式；不复制另一套秘密配置。
- **明确不做**：不接 Keychain（属于闸二 / M5）；不改变 Supervisor 的准入判决。
- **依赖**：无
- **验收**：resident HTTP、runtime readiness/chat 各自断言 Bearer；缺凭证启动失败；未声明时不发送 Authorization；错误凭证的就绪探测不得通过；请求中的调用方 Authorization 不得冒充上游凭证。
- **涉及文件**：`crates/idoris-contracts/src/component_card.rs`、`packages/contracts/schema/component-card.schema.json`、`crates/idoris-router/src/proxy.rs`、`runtime_launch_config.rs`、`crates/idoris-upstream/src/local_http_runtime.rs`、启动校验
- **风险/回滚**：纯新增可选字段，回滚只需删掉该字段。

### A2 `LocalRuntimeKind::Vllm`  `BACKLOG`
- **优先级**：high
- **目标**：iDoris 能自己拉起并托管 `vllm serve`（Linux CUDA；Mac 上的 vllm-metal 用同一个 CLI），一个进程一个模型。
- **开发范围**：
  - `crates/idoris-upstream/src/local_runtime.rs`：增加 `Vllm` 变体；argv 为 `serve <model> --host 127.0.0.1 --port N --served-model-name <model_id>`。
  - `model` 允许是绝对路径，也允许是 HF repo id。现有构造函数要求绝对路径，需要按 kind 分别校验；repo id 只允许 `[A-Za-z0-9._-]+/[A-Za-z0-9._-]+` 这种形式。
  - 不做 `.gguf` 校验。
  - `crates/idoris-router/src/runtime_launch_config.rs` 增加 `kind: vllm`。
  - `local_http_runtime.rs` 发请求时，`model` 字段用 served name。
  - 固定引擎版本、模型 revision/本地工件与 GPU 内存配置；`memory_gb` 不能只是一笔与实际 GPU 预留无关的账。明确模型下载的网络阶段，不把接受 HF repo id 等同于离线可运行。
- **明确不做**：不用 vLLM 的 sleep/wake 端点（只在 dev 模式下可用）；不做多 GPU 或 tensor parallel 参数。
- **依赖**：A1（选用 `--api-key` 时）
- **验收**：argv 快照测试（不经 shell、绑定固定为 loopback）；repo id 和路径各有正例和负例；用伪造的 vllm 可执行文件（对 `/v1/models` 返回 200 的脚本）跑通启动 → 就绪 → chat → 停止。
  - readiness 必须包含预期 served model，不能只检查 200；带认证的探测/chat、进程退出和端口释放有负例。Mac spike 不能替代 Linux CUDA 的实机验收；无 Linux 证据时只标记 adapter 已实现，不宣称该平台发布就绪。
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
  - `keep_alive: -1` 是无限空闲驻留，不等价于已证明的“内存压力下不可驱逐”。测试随后的 chat、并发加载和服务重启如何影响驻留；达不到 pin 契约时必须声明能力缺口或拒绝该策略，不能把 TTL 当成硬保证。
- **明确不做**：不做 Ollama 模型拉取（`/api/pull`），模型由用户安装；不支持 rerank（Ollama 没有这个端点），能力表里标为不支持。
- **依赖**：无（与 A1/A2 并行）
- **验收**：用 wiremock 跑完 `RuntimeAdapter` 的全部方法；load 超时返回 `load_unconfirmed`；unload 后用 `/api/ps` 确认的语义与 K11 一致（确认失败就拒绝准入）。
- **涉及文件**：`crates/idoris-upstream/src/ollama/*`、`factory.rs`、`crates/idoris-upstream/tests/ollama_*.rs`
- **风险/回滚**：新模块，回滚即删除。

### A5 后端探测接线 + NVIDIA 探测  `BACKLOG`
- **优先级**：medium
- **目标**：首次启动时按平台给出默认后端卡：Mac 用 oMLX，Linux + NVIDIA 用 vLLM，Windows 用 Ollama，其余用 llama.cpp。
  - 本轮不自动识别 AMD；Linux AMD/ROCm 保留显式配置与后续实机验证，不能同时宣称 A5 已支持该默认分支。
- **开发范围**：
  - `crates/idoris-upstream/src/detect.rs:84`：实现 NVIDIA 探测（`nvidia-smi --query-gpu=name,memory.total --format=csv,noheader`，带超时；找不到命令视为没有 GPU）。
  - 更新 `detect_backend` 的矩阵：Windows 默认 Ollama，`implemented` 改为真实值。
  - 让 router 启动时在没有显式组件目录的情况下使用 `detect_backend` 生成的默认卡；显式配置始终优先。
  - 探测结果只是推荐，必须同时解析实际 executable、endpoint、已安装模型和必要 runtime 配置；`factory.rs` 仍不支持的 provider 不得只凭生成卡标为可用。缺引擎/模型时给出明确安装或配置提示，不自动下载或悄悄换后端。
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
- **目标**：落实 docs/18 §3「三种调用模式」（D-5）：聊天模式不识别意图，自动路由模式识别，调用方声明模式用 header。同时修复现有代码与接口规范 §2.5（「声明角色的调用不从内容重猜」）的不一致。
- **开发范围**：
  - `crates/idoris-router/src/intent/mod.rs` 的 `resolve_profile_with_detector`：在现有的跳过条件里加上「`parsed.role` 不是 `Some(Role::Auto)` 就跳过」。
  - TS 参考实现 `packages/router/src/intent.ts` 同步修改。
  - 三种模式只控制判定调用，不覆盖角色、能力和复杂度：显式角色始终优先；`X-iDoris-Intent` 存在时不调用任何 intent/complexity judge；complexity 未声明则保留默认。单独声明 complexity 不关闭 auto 的 intent 检测，但判定结果不得覆盖该声明。为 complexity 保留 Header/Default 来源，B5 再加入 Detected 来源。
  - 把 `conformance/tests/intent-detection.test.ts`、`crates/idoris-router/tests/intent_wiring.rs` 里期望触发识别的用例改成 `idoris/auto`，并为聊天模式补反例。
  - 向规范对齐，不升契约版本；对外可见的行为变化写进 release note。
- **明确不做**：不改隐私检查和 `local_only` 过滤；不改聊天模式的选模型逻辑；不改规则引擎。
- **依赖**：无
- **验收**：
  - `idoris/daily`、不透明模型 ID、不填 `model` 三种情况下 detector 调用 0 次（mock 计数）；
  - `idoris/auto` 且无意图头时 detector 被调用；带意图头时任意 `model` 都调用 0 次；
  - 三种模式下 `local_only` 都只剩本地候选（隐私检查不随模式跳过）；
  - 仓库默认策略下聊天模式的选择与改动前一致（对照测试），不承诺自定义 intent 策略完全不变。
  - 覆盖 `daily + coding`、`auto + Intent`、`auto + Complexity`、缺少/非法 header 的组合；非法 header 仍为 400。其他显式角色（vision/embed/decide 等）不因“聊天模式”这个名称被改成 daily/chat 能力。
- **风险/回滚**：对外可见的行为变化仅限「不带意图头的非 auto 请求不再自动识别意图」；仓库默认路由不引用这四类识别意图，因此默认选择不变；自定义 intent 策略可能变化，必须写明迁移影响。回滚恢复此前检测调用条件；字段来源仅为内部元数据，不应改写调用方声明。

### B0 🖥 d3 在 oMLX 上的实机 spike  `READY`
- **优先级**：high（B2、B4 的前置）
- **目标**：确认 `/v1/systemone` 的线格式，以及 d3-lite / d3-nano 对中文输入的实际表现。
- **开发范围**：
  - 在本机 oMLX 上加载 d3-lite（固定 revision）。
  - 用 curl 发中英文各 10 条请求，题型包括 `choice`、`noul`（布尔概率）、`score`。使用按问题名索引的 `questions` 对象及 `criteria`；固定 oMLX commit、模型 revision、MLX 格式/量化和请求体。
  - 记录请求和响应样例、p50/p95、常驻内存、和聊天主模型同时常驻时的内存压力。
- **明确不做**：不改代码。
- **交付物**：填写 docs/18 附录 A（oMLX 版本、d3 revision、样例、数字）。
- **否决条件**：中文 `choice` 的准确率接近随机，或者 p95 > 300ms，则暂停依赖 d3 的后续链路，在附录里写明原因。
  - 上述“接近随机”必须列出样本数、正确数和 chance 基线；20 条仅为可行性筛查，不作为生效证据。否决只暂停 d3 链路；保留 BM/WP-A。Core ML/M5 Pro 的下载体积和延迟不是 M1 Max/oMLX 的内存与性能证据。

### B1 路由规则补上 intent 和 complexity  `BACKLOG`（与 B5 同一版本上线）
- **优先级**：high（不做这一步，任何分类器都不会产生可观测的效果）
- **上线时机**：PR 可以提前写好，但必须和 B5 在同一版本合并。提前上线的话，驱动规则的仍是哈希识别器，误判会直接把请求送错模型；B4 的 shadow 运行只记录一致率，不需要这些规则。
- **目标**：在隐私过滤后的角色/能力选择阶段，让可信的 intent/complexity 提议影响 auto 选择；`deep` 是角色，不是 `local/remote/lora` tier。
- **开发范围**：
  - 增加确定性的 auto 角色选择配置及执行接线，例如 coding/agent_task 提议 deep；复用现有角色枚举，不给 tier 增加 deep。`web_search` 不得映射到不存在的 tools 能力；先定义已有能力组合或明确当前不支持。
  - 隐私过滤是独立硬门，先剔除不允许的候选，再在剩余候选中选择角色/能力。现有 YAML 为 first-match，`local_only` 首条会遮住后续 intent 规则；不能只追加 YAML 并宣称本地 auto 已生效。
  - 显式 `idoris/daily` 加 coding 仍在 daily 内选择，不得跨角色升级；只有 auto 委托角色选择。不透明模型 ID 的明确 override 必须保持。对齐接口规范及 PR #510：区分请求角色与有效选择角色，保留显式绑定要求，load、chat、response.model 使用同一真实 ID；禁止 auto 回退到发现列表首项。该映射设计和 #510 的组合用例须在 B1 PR 中一起审核。
  - 新规则只消费 Header 或通过门槛的 ActiveJudge 来源；off/shadow 的哈希识别只作诊断，不参与新语义选择。判定模式切换为 off 时立刻恢复对应基线，不能靠“B1/B5 同版本”代替运行时隔离。
  - 更新 `conformance/tests/default-routing-policy.test.ts`。
- **明确不做**：不让模型直接指定 provider、突破隐私/预算或覆盖显式角色；不改 detector 算法。角色选择接线是本任务必做范围，不能再限定为 YAML-only。
- **依赖**：B2 达标、B4、PR #510 最终角色绑定契约；与 B5 同版本上线（发布约束，不是循环开发依赖）
- **验收**：真实路由边界断言 auto+coding 选中绑定的 deep 模型（含 local_only）；daily+coding 不改角色；无合格本地 deep 候选明确失败；off/shadow 的哈希命中不得激活新规则；显式 intent 规则仍可用；load/chat/response 的具体 ID 一致。新增选择规则标识进入审计/原因头，不能把 YAML 数组下标当成未定义的新稳定 rule_id。
- **风险**：改的是默认路由行为，属于用户可见的变化，要写进 release note。

### B2 评测集与评测脚本  `BACKLOG`
- **优先级**：high
- **目标**：用数据决定 d3 是否生效、选哪个尺寸。
- **开发范围**：
  - 在 `eval/decision/` 下建立标注集：至少 200 条中英混合样本，覆盖 5 类 intent 和现有 simple/complex 两档 complexity。每个 intent 至少 20 条，按语言分层，包含负对照和易混样本；保留独立测试集，不把哈希模板扩写当作唯一测试来源。
  - 编写离线评测脚本，同时跑三类对象：哈希 detector、d3-lite / d3-nano（通过 `/v1/systemone`）、对照组「常驻聊天模型模拟判定」（对应决策点 D-2）。
  - 输出准确率、混淆矩阵、ECE、选项置换一致率、p50/p95。
  - 分别报告 intent 与 complexity，不把哈希 detector 当作已有 complexity 分类器。额外比较默认 chat/规则基线；固定阈值后再测独立测试集。报告低置信度拒判的覆盖率、分语言/类别质量、错误路由代价；与主聊天模型并发时测延迟和内存，不从裸模型速度推算网关性能。
- **明确不做**：不进 CI 的必跑项（依赖真实模型）；不使用真实用户数据。
- **依赖**：B0
- **验收**：评测脚本在 mock 后端上跑通，CI 里有冒烟测试；实机报告写进 docs/18 附录 A。
- **通过线**：准确率比哈希基线高 15 个百分点以上，并且在配对比较（McNemar）下跨过 docs/16 ST-2 的统计门槛（n=200 时非配对比较分辨不出 7pp 以内的差距）；p95 < 150ms；置换一致率 ≥ 95%。

### B3 `/v1/systemone` 契约类型  `BACKLOG`
- **优先级**：medium
- **目标**：把 TypeSafe System One 线格式定义为 iDoris 契约，供 B4（客户端）和 B6（服务端）共用。
- **开发范围**：
  - 新增 JSON Schema `packages/contracts/schema/systemone.schema.json`，内容包括：
    - 请求：`model`、`state`、`questions: {name: {type: choice|noul|score, instructions, criteria}}`；choice 的 criteria 为选项映射，score 为有序数组。
    - 响应：`answers` 按问题名索引，按 type 分别校验 choice、noul、score 和各自概率形状；不得用期望索引冒充离散类别或校准置信度。
    - 错误体：`reason_code: judge_unavailable`，以及 `judge_mode`、`calibrated`
  - 生成 Rust 和 TS 类型。
  - 在 `crates/idoris-upstream` 增加 `SystemOneClient`：只做 loopback，带超时，复用 K01 的 `redirect(none)` 和 `no_proxy` 设置及 A1 的认证；文本输入限制、问题数和响应大小有界。
  - 明确 iDoris envelope 与上游 wire 的转换；`judge_mode/calibrated` 由已验证的模型/引擎种类和校准记录确定，不能信任模型自报。oMLX 同一个端点也能使用聊天模型：HTTP 200 不证明执行了专用 judge。
- **明确不做**：不做多模态输入（图片、视频留到后续）；不暴露对外路由（那是 B6）。
- **依赖**：B0 固定版本的请求/响应 fixture、A1 认证配置；取得 fixture 后再冻结公共类型。
- **验收**：契约漂移检查通过；客户端用 wiremock 测试正常、超时、5xx、格式错误四种情况；外部重定向不被跟随。

### B4 `SystemOneDetector`（shadow 模式）  `BACKLOG`
- **优先级**：medium
- **目标**：d3 与现行哈希 detector 并行运行，只记录结果，不改变路由（对应总体规划 M6 的「shadow 模式」）。
- **开发范围**：
  - 在 `crates/idoris-router/src/intent/` 新增 `SystemOneDetector`，实现 `ProfileIntentDetector`，locality 为 `Loopback`。
  - `resolve_profile` 改成按配置选择 detector（现在硬编码 `default_detector()`），配置项为 `intent_judge: { mode: off|shadow|active, model, timeout_ms, min_confidence }`。
  - shadow 模式下 d3 异步运行，不阻塞请求。新增判定完成事件，携带 `shadow_intent`、`shadow_agree` 两个标量字段（同步扩充事件类型及 `crates/idoris-tenancy/src/event_log/mod.rs` 的 SCALAR_KEYS 白名单），只记录标签，不记录内容（不变量 #5、#15）。
  - 异步结果通过同一 record_id 的追加事件记录，不能回写已发出的 profiled 事件；事件名和 scalar 白名单一并定义。设置有界队列、并发、采样和丢弃计数，避免 shadow 抢占主模型；成功/失败均不得悄悄加载预算账外的模型。
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
  - 保留各字段来源，显式 complexity 永远优先；无声明时使用现有 simple/complex 两档，若采用连续 score 必须固定确定性的映射和置信度算法，不引入未声明的 medium。
  - 置信度低于 `min_confidence` 时视为未命中，intent 保持默认 `chat`；配置里的阈值只能调高，不能低于代码下限（clamp）。
  - judge 不可用时（D-1）：`idoris/auto` 请求返回 503 `judge_unavailable`，不回落；聊天模式和调用方声明模式不调用 judge。
  - 通过既有 Supervisor 预算账和加载准入管理 judge 的常驻、pin、状态与恢复，不能绕过额度直接 load。缺 judge 不得导致所有聊天入口因全局启动检查而不可用：配置畸形启动失败，合法配置下 judge 运行期未就绪仅影响需要它的请求。
  - active 只允许验证过种类与 revision 的专用判定模型；指向聊天模型不得被当成正常 d3 成功。D-2 模拟组可以评测，但没有调用方显式许可不能据此投入生产模拟判定。
- **明确不做**：不让 d3 影响 privacy、预算或准入（不变量 #1、#10）。
- **依赖**：B4，以及 B2 达到通过线；与 B1 同一版本上线
- **验收**：
  - `local_only` 加 `active` 时，d3 仍可运行（Loopback），远程候选仍为 0；
  - 低置信度时结果与 miss 相同；
  - judge 不可用时：`idoris/auto` 返回 503 和 `judge_unavailable`；同一时刻 `idoris/daily` 请求正常返回 200；
  - property test：任何 d3 输出都不会让候选集合多出非 local 的卡。
  - 验证 off/shadow/active 切换、未就绪启动、失联/格式错误、judge 重载和内存不足；聊天/声明模式不调用 judge。多租户并发的 shadow 不突破内存/并发额度；端点返回模拟答案时必须被拒绝或按显式授权标记。

### B6 对外 `/v1/systemone`（M6）  `BACKLOG`
- **优先级**：low
- **目标**：Agent24 的审批门、Evolver 等业务模块可以通过 iDoris 使用决策端点。
- **开发范围**：
  - 在 router 增加 `POST /v1/systemone` 路由，走 auth 和 virtual key scope，新增 scope `systemone`。
  - 选中承载 `Role::Decide` 的卡，请求转发给它。
  - 写入 Event Log（只记标签和概率摘要）。
  - judge 不可用时返回 `judge_unavailable`。只有调用方显式声明接受模拟判定时，才用宿主模型模拟，并在响应里标明 `judge_mode: simulated, calibrated: false`（不变量 #8）。验证承载卡实际绑定的模型种类，不能把 oMLX 对聊天模型返回的 200 当成原生 judge。
- **明确不做**：不做多模态输入；不做决策账本的回放 UI。
- **依赖**：B3、B5
- **验收**：
  - 没有对应 scope 的 key 返回 403；
  - 没有 Decide 卡时返回 503 和 `judge_unavailable`；
  - 不声明接受模拟时，绝不调用聊天模型（mock 调用 0 次）；
  - conformance 中 `known-spec-conflicts` 里关于 `/v1/systemone` 的条目移除；
  - Agent24 契约版本 +1。

### B7 L1 隐私收紧器（M5 之后）  `BLOCKED`（等 M5 闸一合并与失败语义复审）
- **依赖**：B3/B5、M5 闸一最终契约；明确 active 隐私判定对三种调用模式的影响后另行审核。
- **目标**：用 d3 的 `noul` 题「是否含个人或敏感数据」作为 L1，只能把 privacy 往 `local_only` 方向收紧。
- **开发范围**：结果写进 `content_tightening` 或 INSPECTED 的有效隐私；先 shadow 后生效，流程同 B4/B5。
- **明确不做**：不改 L0 规则和 L2 `decide_action`；REDACT 仍然遵守 e12dd19「每个请求在本地控制台确认」的决定。
- **验收**：property test 证明 L1 永远不会放宽隐私。off/shadow 时仅观察、不改变 L0 结果；active 时对需要 L1 判定的请求，超时、不可用或格式错误返回 `judge_unavailable`，不得静默退回 L0 后继续出站。已 local_only 且无需进一步判定的旁路必须明示其条件和零远程调用证明。先对齐 M5 的实际 INSPECTED 契约再冻结此行为；“只能收紧”不能推出“故障时可放行”。
  - B7 与 D-5 的意图模式独立：如果启用内容隐私 L1，它可作用于三种模式。此前“聊天不调用 judge”的保证只指意图/复杂度 judge，不承诺未来的 active 隐私判定器永不影响聊天。

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

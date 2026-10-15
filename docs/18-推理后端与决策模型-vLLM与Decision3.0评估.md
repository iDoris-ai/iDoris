# 18 · 推理后端与决策模型：vLLM 与 Decision 3.0 评估

> 状态：**已拍板**（2026-10-15，owner 接受）· 实施切片另行批准
> 基线：`origin/main`（2026-10-15）。外部事实为当日快照，引用处附来源。

两个问题：

1. **推理后端**：是否用 vLLM（Mac 上为 vllm-metal）替换默认的 oMLX，以获得跨平台和统一接口？
2. **决策模型**：vLLM Semantic Router 团队发布的 Decision 3.0（d3 系列）能不能用？用的话现有 routing / decide 代码怎么融合？

一句话结论：

- **不换 oMLX。** 在 iDoris 自己的 `RuntimeAdapter` 契约后面补 vLLM（Linux）和 Ollama（Windows）两个并列适配器。
- **用 Decision 3.0，但只当提议者（proposer）。** 经 oMLX 的 `/v1/systemone` 接入三个单向阀插槽，不替换确定性的 `decide()` 管线。

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
  - 复杂度没有分类器，只靠 `X-iDoris-Complexity` header。
- **已有缺口。** `config/routing-policy.yaml` 没有任何规则引用检测出的 4 个 intent（web_search / coding / email / agent_task）。检测结果只进 Event Log，**不改变路由**。

### 1.2 外部调研

| 项 | 结论 |
|---|---|
| vLLM 跨平台 | Linux（CUDA / ROCm / XPU / CPU）有官方支持。**Windows 没有官方原生支持**，官方路径是 WSL2（或 Docker Model Runner + WSL2），原生 Windows 只有社区构建。边缘 / 端侧（手机、Core ML）不是 vLLM 的领域。 |
| vllm-metal | vllm-project 下的「社区维护硬件插件」，要求 macOS 15+ 和 Apple Silicon。底层仍是 MLX，由 `mlx_lm` 提供层实现。paged attention 标为 experimental。`vllm serve` 范式是**一进程一模型**。 |
| oMLX | 仅支持 macOS Apple Silicon。能力：原生多模型、LRU 驱逐、pin、手动 load / unload、内存上限、SSD 二级 KV cache；同时提供 OpenAI 端点和 Anthropic `/v1/messages`，以及 embeddings、rerank。**已支持 `POST /v1/systemone`，并点名支持 Decision 3.0 全系**（d3 / flash / mini / nano / lite / edge）。 |
| Ollama | Win / Mac / Linux 原生。`/api/ps` 加 `keep_alive` 可以实现加载、卸载和 pin。0.35 起也提供 `/v1/systemone`，但调研只看到它支持 Nimble、Tev1 等决策模型，**能否跑 d3 未核实**。 |
| Decision 3.0 | 来自 vLLM-SR 团队，Apache-2.0，基于 Qwen3.5 系。输入是 state（文本 / JSON / 图片 / 视频）加 typed questions（`choice` / `bool` / `score`），输出选项概率而不是生成文本。HTTP 形态为 `/v1/systemone`（TypeSafe System One 线格式）。 |
| d3 规模 | edge 0.59B、lite 0.85B、nano 2.21B、mini 4.54B、flash 8.39B、d3 26.09B。edge 在新域任务上只有 12.9 分（d3 为 60.5），只适合窄而经过验证的类目。 |
| d3 在 Mac 上 | 在 M5 Pro 上，Core ML 版 d3-lite 约 53ms、d3-nano 约 69ms（JevBench 中位数）。下载体积：lite 3.7GB、nano 10.5GB。 |
| d3 风险 | **中文能力没有证据**。Engine 处于 dev channel。博文作者自测时出现过排序错误（「今天」没有排在 urgency 首位）。 |

---

## 2. 结论一：不把 oMLX 换成 vllm-metal

### 理由

1. **统一点在 iDoris 自己的契约，不在引擎。** `RuntimeAdapter` 加 OpenAI-compat 已经把接口统一了。换引擎只是换掉一个实现，接口不会因此更统一。以后要换 Ollama，也只需要再写一个 adapter。
2. **vLLM 的跨平台程度不如设想。** 它是「Linux 一等公民 + Mac 社区插件 + Windows 只能走 WSL2」。真正在三个平台都原生可用的是 Ollama 和 llama.cpp。「边缘计算」这条理由也不成立。
3. **oMLX 正好提供 Supervisor 依赖的能力**：多模型常驻、LRU、pin、SSD KV、Anthropic 端点、rerank，以及 `/v1/systemone` 加 d3。vllm-metal 一进程一模型，这些都得 iDoris 自己补；而且两者底层都是 MLX，没有证据表明有性能代差。
4. **换掉默认代价大、收益不明。** 这等于推翻 U0 的实测结论（`spike/u0/U0-LOG.md`），v0.1.3 里的 pin、内存压力和 K09/K11 竞态修复也要重做。

### 按平台分配默认后端

| 平台 | 默认 | 备选 |
|---|---|---|
| macOS Apple Silicon | **oMLX**（不变） | vllm-metal（仅作可选 runtime kind，用于 benchmark 对比）、Ollama |
| Linux + NVIDIA / AMD | **vLLM**（新增） | llama.cpp、Ollama |
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
- **R4 `ChatProxy` 支持可选 Bearer**，对应 vLLM 的 `--api-key`。
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

### 融合方式：三个单向阀插槽（代码均已预留）

1. **意图 / 复杂度提议者**
   - 新增 `SystemOneDetector`，实现 `ProfileIntentDetector`（`crates/idoris-router/src/intent/mod.rs`），locality 为 `DetectorLocality::Loopback`（oMLX 在本机回环上，`local_only` 下也允许运行；`resolve_profile` 只会跳过 `Remote` detector）。
   - 调用 oMLX 的 `/v1/systemone`，模型用 d3-lite 或 d3-nano（按评测结果选）。
   - 问题设计：`intent: choice[chat, web_search, coding, email, agent_task]`，`complexity: score[simple, medium, complex]`。
   - **先 shadow，后生效**（总体规划 M6「入口判定层」要求）：先与现行哈希 detector 并跑，只把 d3 的结论和一致率写进 Event Log，不改路由；一致率与评测达标后再切为生效。
   - 生效后置信度低于阈值：视为「未命中」，保留默认 `chat`（与现有 miss 语义一致）。模型给的阈值/置信度只能收紧，按不变量 #8 做 clamp。
   - 生效后 judge 不可用或超时：按**不变量 #8 默认拒绝**，`reason_code: judge_unavailable`。只有配置显式写 `on_judge_unavailable: fallback_hashing` 时才回落到哈希 detector，并且必须在 `X-iDoris-Degraded` 与 `profiled` 事件里标明，不允许静默回落。见决策点 D-1。
   - 配套改动：
     - 扩展 detector 的输出，让它能写 `complexity`；
     - `resolve_profile` 改为按配置选择 detector（现在硬编码 `default_detector()`）；
     - header 显式给出的值始终优先。
2. **L1 隐私收紧器**（M5 合并后）
   - 用 d3 的 bool 问题判断「是否含个人 / 敏感数据」。
   - 结果只能把 privacy 往 `local_only` 方向收紧，写入 `content_tightening` 或 INSPECTED 的有效隐私。
   - L0 规则和 L2 `decide_action` 不变。
   - REDACT 仍按 e12dd19 的决定执行：每个请求都要在本地控制台单独确认。
3. **对外 `/v1/systemone`（M6）**
   - router 增加该路由，透传到承载 `Role::Decide` 的 oMLX card（走 `ChatProxy` 模式），供 Agent24 使用。
   - judge 不可用时按不变量 #8 返回 `judge_unavailable`。

### 前置缺口

`config/routing-policy.yaml` 需要补上引用 intent / complexity 的规则（例如 `coding` → deep、`complex` → deep）。不补的话，分类器再准也没有可观测的路由效果。

**上线时机：与 d3 生效（B5）同一版本。** 如果提前上线，驱动这些规则的仍是不准的哈希识别器，误判会直接把请求送错模型。shadow 运行只记录一致率，不需要这些规则。

### 评测门槛（默认开启前必须通过）

- **数据集**：用现有 intent 话术和 conformance 用例，扩成至少 200 条中英标注样本。
- **对比对象**：HashingEmbedder、d3-lite、d3-nano。
- **指标**：准确率、p50 / p95 延迟（M1 Max 本机 oMLX）、常驻内存。
- **一致性**：选项顺序置换后结论不变（M6 要求的置换一致性测试）；按置信度分箱报告校准误差（ECE）。
- **通过线**：准确率显著高于哈希基线，p95 < 150ms，内存能与主聊天模型共存。
- **未通过**：只保留插槽，默认不开启。

---

## 4. 工作计划与决策点

拆分后的工作包与任务卡见 [`agent/plans/runtime-portability-and-decision-model.md`](agent/plans/runtime-portability-and-decision-model.md)，台账索引见 [`agent/tasks.md`](agent/tasks.md) 的「F1.2+ / F2.4+」一节。

- **WP-A 跨平台推理后端**：A1 ChatProxy Bearer → A2 vLLM runtime kind → A3 vllm-metal 对比 spike → A4 OllamaAdapter → A5 后端探测接线 → A6 跨平台发布（里程碑占位）→ A7 oMLX 依赖风险与退出路径。
- **WP-B 决策模型提议者**：B0 d3 spike → B2 评测集与评测脚本 / B3 `/v1/systemone` 契约类型 → B4 shadow detector → B5 切为生效（B1 路由规则补 intent/complexity 与它同一版本上线）→ B6 对外 `/v1/systemone` → B7 L1 隐私收紧器（M5 后）→ B8 自有决策模型（M7 占位）。

待 owner 确认的决策点：

| 编号 | 问题 | 建议 |
|---|---|---|
| D-1 | d3 生效后 judge 不可用时，意图判定是拒绝还是回落哈希？ | 默认拒绝（不变量 #8 原文），配置可显式选 `fallback_hashing`，并在响应头和事件里标明 |
| D-2 | M6 原设想「复用常驻小模型做 logit 型 System-1」，d3 是专用模型，要多占内存 | 先用 d3-lite（下载约 3.7GB）；B2 评测同时测「常驻聊天模型 + `/v1/systemone` 模拟判定」作对照，用数据决定 |
| D-3 | Windows 默认后端选 Ollama（不用 vLLM：无官方原生 Windows、WSL2 过不了「妈妈测试」、基本要求 NVIDIA） | **已确认（2026-10-15）**。代价：Ollama 用 GGUF，与 MLX 权重不通用，catalog 按后端分列模型 ID；Ollama 能否跑 d3 未核实，Windows 上的决策模型暂时空缺 |

## 附录 A · Spike 记录

（待 S1 填写：oMLX 版本、d3-lite revision、请求 / 响应样例、中英各 10 条的 p50 / p95、常驻内存。）

## 来源

- vllm-metal：[文档](https://docs.vllm.ai/projects/vllm-metal/en/latest/) · [GitHub](https://github.com/vllm-project/vllm-metal) · [Docker Model Runner 博文](https://www.docker.com/blog/docker-model-runner-vllm-metal-macos/)
- vLLM 的 Windows 现状：[fazm.ai 汇总](https://fazm.ai/t/vllm-windows-support-2026) · [dasroot 原生 Windows 指南](https://dasroot.net/posts/2026/05/run-vllm-natively-windows-without-wsl/)
- oMLX：[GitHub](https://github.com/jundot/omlx)
- Decision 3.0：[Mushroom 博文](https://blog.mushroom.cv/blog/decision30-multimodal-system-one/) · [Apple Silicon 博文](https://vllm-semantic-router.netlify.app/blog/decision-3-0-on-apple-silicon/) · [HF d3](https://huggingface.co/vllm-sr/d3) · [HF d3-edge](https://huggingface.co/vllm-sr/d3-edge) · [vllm-project/semantic-router](https://github.com/vllm-project/semantic-router) · [PingWest 报道](https://www.pingwest.com/w/318299)
- Ollama `/v1/systemone`：[runtimewire](https://runtimewire.com/article/ollama-local-decision-models-systemone)

# iDoris Rust 网关基础调研：fork / vendor / 借鉴 决策报告

> 调研时间：2026-09-27。方法：`gh api`/`gh repo view`/`gh search repos` 拉取一手 GitHub 元数据（star、`pushed_at`、commit 历史、LICENSE 原文）；对本机已有的 TensorZero 源码（`/Users/jason/Dev/auraai/Agent24/vendor/reference/tensorzero`）做全量目录级核查；对排名前三的候选（katanemo/plano、api7/aisix、llamastash/llamastash）执行 `git clone --depth 1` 到 `scratchpad/oss/` 并逐文件深读源码。调研由多个并行子任务分头核实，凡标注"未核实"的地方均为确实查不到、不是遗漏。
>
> 姊妹报告：非 Rust 候选（LiteLLM Python 部分、Ollama、Presidio、NeMo Guardrails、FastChat Controller 等）见 `scratchpad/research-ideas-nonrust.md`，本报告不重复其内容，仅在涉及"更名/语言澄清"时交叉引用。

---

## 0. iDoris 核心需求回顾（用作打分基准）

- 本地优先、OpenAI 兼容网关，个人/中小组织，macOS 为主（后续 Windows）。
- 管理异构本地/远程运行时：oMLX（HTTP :8088）、mlx_lm.server、llama.cpp server、spawn CLI 转发的订阅服务、外部 API provider。
- 路由链：隐私 → 预算 → 意图 → 容量，要求**确定性、可配置、可解释**，local_only 请求 **fail-closed**。
- 虚拟 key + 租户隔离；预算 **reserve/settle**，价格未知拒绝；审计只存元数据。
- 可选 ATIF v1.8 轨迹、feedback 端点、训练数据导出；小模型做意图判定。
- Admin API，**SQLite** 存储，个人版单二进制可跑，禁止强制 ClickHouse/Postgres/Redis。
- 许可证：Apache-2.0/MIT 等宽松协议；排除 GPL/AGPL；排除"禁止竞争性 SaaS/托管服务"条款（Elastic License、BSL 类附加条款等）。

打分说明：**思路匹配度(Fit)** 和 **工程质量(Quality)** 均为 1-5 分。部分项目在"整体"和"某单一维度"上分数差异很大（例如某项目整体路由能力弱但模型生命周期管理极强），报告中会分别注明。

---

## 1. 对比总表

### 1.1 许可证一票否决名单（确认排除，不参与后续排名）

| 项目 | 许可证核实结论 | 排除原因 |
|---|---|---|
| Helicone/ai-gateway | **GPL-3.0**（2025-11-21 从 Apache-2.0 改过来，`9649b27` commit 为证，该 commit 也是仓库至今最后一次 push） | 触碰 GPL 红线 |
| langdb/ai-gateway（已更名/迁移为 `vllora/vllora`） | **Elastic License 2.0**（`LICENSE.md` 原文含"不得作为托管/托管服务提供给第三方"条款），子包 `llm/` 单独 Apache-2.0 但主体不是 | 反竞争性 SaaS 条款 |
| doorman-dev/doorman | Apache-2.0（表面合规），但 Rust 重写未完成（当日 +59678 行迁移 PR 尚未合并），持久化强依赖 **MongoDB+Redis**，且是通用多协议网关非 LLM 专用 | 架构不符（非许可证问题，仍记录于此便于查阅） |

### 1.2 通过许可证审查的 Rust 候选（含语言核实结果）— 按"思路匹配度 × 工程质量"综合排序

| 项目 | 许可证 | Star | 最近 push | 语言实测 | 整体 Fit | 整体 Quality | 一句话定位 |
|---|---|---|---|---|---|---|---|
| **katanemo/plano**（原 archgw/Arch Gateway） | Apache-2.0（纯净） | 7067 | 2026-09-26（近乎每天） | Rust(brightstaff/hermesllm 纯async) + WASM(Rust) + Envoy(C++) + supervisord(Python) | 3.5/5 | 4.5/5 | 路由决策骨架+多provider转换质量最高，但虚拟key/预算/隐私路由/PII 全部缺失或是死代码 |
| **api7/aisix** | Apache-2.0（纯净） | 169 | 2026-09-25（每天2-5次） | Rust 单体，18 crate | 2/5（受限于无预算/无本地运行时/无隐私路由） | **5/5**（本次最高） | 功能覆盖面最完整的开源 Rust LLM 网关：虚拟key、guardrail、限流、cache、6种路由策略均已生产级实现 |
| **llamastash/llamastash** | MIT（README 另附非LICENSE的"禁军事用途"声明） | 171 | 2026-09-27（持续） | Rust，TUI+daemon | 4/5（本地运行时管理维度） | 4/5 | 本地运行时生命周期管理（进程状态机/健康探测/空闲驱逐/单飞并发）当前最佳参考实现 |
| **EricLBuehler/mistral.rs**（聚焦 server-core） | MIT（纯净） | 7715 | 2026-09-25 | Rust | 2.5/5 整体，**5/5**（模型 load/unload 维度） | 4.5/5 | `/v1/models` + unload/reload/status 生命周期 API 是生产验证过的最佳协议参考 |
| **Noveum/ai-gateway** | MIT OR Apache-2.0（纯净） | 97 | 2026-08-24 | Rust | 3.5/5 | 4/5 | 唯一显式支持 `failClosed` 字段 + in-process PII 的 Rust 网关，无外部依赖，但预算未持久化 |
| **agentgateway/agentgateway**（Linux Foundation 项目，原属 kgateway 生态） | Apache-2.0（纯净） | 5059 | 2026-09-27（当天） | **Rust 为主**(7.88MB) + Go(2.78MB，K8s控制器) | 未完全核实，估 3/5 | 未完全核实，估 4/5 | "budget and spend controls"+guardrail(regex/OpenAI moderation/Bedrock Guardrails/Model Armor)+CEL RBAC，细节机制未读源码确认 |
| **BerriAI/litellm 的 `litellm-rust` 子目录** | MIT（workspace 声明，非 enterprise/ 限制目录） | （随主仓库 star，未单独统计） | **2026-09-27（当天，几乎每天多次提交）** | Rust，40+ crate | 未完全核实（仍在搭建期），概念上潜力 4-5/5 | 早期/不稳定 | LiteLLM 官方正在把 proxy 核心重写成 Rust，含 router/auth/cache/cost/gateway 等crate，但目前经 PyO3 嵌入 Python 宿主运行，**非独立单二进制**，尚不可 vendor |
| **Northwood-Systems/millwright** | Apache-2.0（纯净） | 46 | 2026-08-10（**停滞7周**） | Rust | 2/5 整体，4/5（确定性路由维度） | 3.5/5 | 单二进制+SQLite默认+确定性成本路由(`router.rs`无`rand::`)是架构上最像iDoris的项目，但单人维护已停滞，无预算/guardrail/本地运行时 |
| **vllm-project/semantic-router** | Apache-2.0（纯净） | 5921 | 2026-09-27（当天，社区活跃） | **主体是 Go**(23.4MB)+Python(12.3MB)+Rust(3.19MB仅ML推理FFI层) | 1.5/5 整体（非Rust网关架构），**4.5/5**（仅"意图判定方法论"维度） | 5/5 | 不适合做代码基础（Envoy+K8s+Postgres/Milvus），但`pkg/classification`+`pkg/decision`的信号驱动分类+可回放决策trace是本次最成熟的方法论参考 |
| **traceloop/hub** | Apache-2.0（纯净） | 229 | 2026-07-16 | Rust | 2/5 | 3.5/5 | OTel观测原生集成较强，但Management模式强制Postgres，YAML模式功能过窄 |
| **Pushkinist/rMLX** | MIT OR Apache-2.0 | 15 | 2026-09-27 | Rust（mlx-c FFI，非套壳Python） | 2/5（单一后端，非管理器） | 4/5 | 严肃的纯Rust MLX推理引擎，可作为被iDoris管理的oMLX/mlx_lm.server替代后端，单人维护 |
| **ferrumox/fox** | MIT OR Apache-2.0 | 189 | 2026-08-22（**停滞5周，CI失败**） | Rust（llama.cpp FFI绑定，非子进程） | 2/5 | 3/5 | 分页KV cache+前缀缓存设计可参考，项目本身走下坡 |
| **truespar/paddock** | MIT OR Apache-2.0（形式合规） | 110 | 2026-09-25 | Rust | 2/5 | 2.5/5 | **来源存疑**：24天81次提交却有2646个文件、无CI，不建议采用 |
| **EricLBuehler/candle-vllm** | MIT（纯净） | 729 | 2026-09-17 | Rust | 1/5（纯推理引擎，单进程单模型） | 4/5 | 排除作为网关基础，可参考OpenAI协议struct |
| **jeremychone/rust-genai**（crate `genai`） | MIT OR Apache-2.0 | 895 | 2026-09-26 | Rust | 5/5（provider-client维度） | 4/5 | **自带 `omlx` 专用 provider adapter**（默认:8000，环境变量可指向:8088）+ 通用OpenAI兼容端点机制，出站客户端首选 |
| **64bit/async-openai**（crate `async-openai`） | MIT（纯净） | 2019 | 2026-09-09 | Rust | 4/5（provider-client维度） | 5/5 | 累计下载862万，OpenAI协议覆盖最全最稳，`Config` trait 5方法即可接自定义base_url |
| **0xPlaygrounds/rig**（crate `rig-core`） | MIT（纯净） | 8744 | 2026-09-27 | Rust | 4/5（provider-client维度） | 4/5 | 生态最热但偏Agent框架，API churn风险高（README自曝"here be dragons"） |
| **mostlygeek/llama-swap**（**Go，非Rust，仅设计借鉴**） | MIT（纯净） | 5759 | 2026-09-27 | Go | 不适用（非Rust） | 不适用 | 三层拆分(process/scheduler/swapper)+单写者事件循环设计是模型热切换的最佳架构范式 |

### 1.3 本次新核实、确认不适合的候选

| 项目 | 核实结论 |
|---|---|
| **NVIDIA-AI-Blueprints/llm-router** | `gh api repos/NVIDIA-AI-Blueprints/llm-router/languages` 实测语言构成为 **Jupyter Notebook + Python**，**没有 Rust 代码**；此前坊间"Router Controller 是 Rust"的说法**未能证实**，如实标注为未核实/疑似不准确。仓库本质是路由分类器训练教程（Router Server 用 Triton 部署分类模型，Router Controller 转发流量），Apache-2.0，351 star，2026-07-24 最后推送。不纳入 Rust 候选排名。 |
| **Envoy AI Gateway → Agent Router** | 已确认改名迁移到新仓库 **`theagentrouter/agent-router`**（README: "Manages Unified Access to Generative AI Services built on Envoy Gateway"，2147 star）。**语言实测仍是 Go**，不是 Rust。CRD化的K8s原生网关，与iDoris单机场景不符，仅供架构对照，不纳入Rust候选排名。 |
| **katanemo/archgw → katanemo/plano** | 确认为同一项目改名（仓库内 Dockerfile/crates 结构与"archgw"历史一致），已按新名 plano 记录于本报告全文。 |

---

## 2. 逐项详细说明

### 2.1 katanemo/plano（原 archgw / Arch Gateway）

- 链接：https://github.com/katanemo/plano ；配套论文 [Arch-Router (arXiv:2506.16655)](https://arxiv.org/abs/2506.16655) ；配套模型 [katanemo/Arch-Router-1.5B](https://huggingface.co/katanemo/Arch-Router-1.5B)
- **许可证**：仓库 LICENSE 逐字核对为纯净 Apache-2.0，无附加条款。**但** Arch-Router-1.5B 模型权重使用 **"Katanemo Community License Agreement"**（非 Apache/MIT）：非商业使用免费，**商用需向 DigitalOcean 申请单独商业授权**，且要求界面/文档展示"Built with DigitalOcean"字样。iDoris 若想直接商用该权重需额外授权；若只是照论文思路自训模型则不受限。
- **维护活跃度**：7067 star，484 fork，143 open issues；`pushed_at` 几乎每天更新，release 节奏规律（`0.4.35`→`0.4.36`等小版本迭代），有 Trivy CVE 扫描修复记录，是本次调研里**唯一"活人天天在维护"**的大型项目。
- **整体架构（初步调研）**：Docker 镜像内是 **Envoy(C++官方镜像) + 两个WASM插件(prompt_gateway/llm_gateway, Rust编译) + 独立常驻进程brightstaff(Rust) + supervisord(Python)统一拉起**，外壳是 Python CLI `planoai`。全仓库 code search 确认 `postgres`/`clickhouse`/`redis`/`sqlite` 零命中，不强制外部数据库，但代价是没有内建持久化状态。
- **深读后的重大修正**（详见第 4.1 节）：`brightstaff` 和 `crates/hermesllm` **均不依赖 proxy-wasm**，是自给自足的 `reqwest`/`hyper` 纯 Rust async 服务，主链路（`/v1/chat/completions` 等）**不需要经过 Envoy**；真正被 WASM ABI 强绑定、难以剥离的只有 `prompt_gateway`/`llm_gateway` 两个插件 crate。但同时发现：**PII/越狱护栏在当前主分支是死代码/demo级**（`PromptGuards`/`GuardType::Jailbreak` 全仓库无调用点，唯一 PII 实现是 44 行的 header 脱敏 + `demos/` 下一个 Python 演示微服务）；"小模型做意图判定"（`orchestrator_model_v1.rs`）实际是**对可配置大模型做一次 temperature=0.01 的 Chat Completions 调用**，JSON 解析失败时**静默 fail-open 回落到客户端原始 model**，不是传统意义上确定性分类器；**完全没有虚拟key/预算reserve-settle/Admin API/隐私路由维度**，全仓库 grep 不到相关代码。
- **功能对照**：OpenAI 兼容 ✓；多provider ✓；路由扩展点 ✓✓（域-动作偏好路由+可配置策略表，思路对但小模型分类不是确定性的）；观测/tracing ✓（OTel零代码埋点+自定义exporter范式如`posthog_exporter.rs`）；guardrail ✗（死代码）；虚拟key/预算/Admin API/隐私路由 ✗（完全没有）；本地运行时管理 ✗（未核实到）。
- **打分**：思路匹配度 3.5/5（路由/观测/多provider转换设计思路和工程质量都对胃口，但四个iDoris刚需项缺失或不可用）；工程质量 4.5/5（星标高、迭代频繁、release规范）。

### 2.2 api7/aisix

- 链接：https://github.com/api7/aisix（早期原型 `api7/aisix-archived` 已确认是同一项目 2026-03 的 etcd强依赖+React后台老版本，2026-04 在当前仓库重写）
- **许可证**：Apache-2.0，逐字核对无附加条款。
- **维护活跃度**：169 star，创建于2026-04-17，`pushed_at`2026-09-25，**5个月内每天2-5次commit**，PR编号已到#1238，10位贡献者含APISIX社区可辨识身份（moonming/membphis/juzhiyuan），提交反复引用私有仓库`api7/AISIX-Cloud`——即"开源核心+付费Cloud"商业模式，团队工程血统扎实（背后是Apache APISIX商业公司）。
- **架构**：单一静态二进制，18个crate的Cargo workspace。单实例用`resources.yaml`+`config.yaml`+SIGHUP热重载，**无SQLite**，多副本才需etcd/Redis（可选）。审计数据导出到Prometheus/OTLP/Datadog等外部系统，无内置存储。
- **功能覆盖（本次候选中最完整）**：OpenAI兼容全套✓（chat/completions、responses、embeddings、realtime）；多provider✓；6种路由策略+retry预算+cooldown✓；流式✓；虚拟key✓（SHA-256哈希存储+模型白名单+过期+禁用，**深读后确认无轮换机制**，此前判断有误）；guardrail/PII✓✓（内置11种正则检测器+Presidio/Lakera外部集成，**深读确认Presidio/Lakera不依赖付费Cloud层，自托管场景可正常工作**）；观测✓（OTLP GenAI span）。
- **深读确认的缺口（比预期更彻底）**：**预算功能完全不存在本地实现**——`crates/aisix-proxy/src/budget.rs`（826行）模块文档第一行明确写"Budget client — asks cp-api per request whether an api_key may proceed"，是纯粹的**mTLS RPC客户端**去问付费Cloud控制面，本地零额度表/零扣费逻辑，only值得借鉴的是其LRU缓存+三态fail-mode(sticky/open/closed)降级容错模式。**Admin API完全只读**：`aisix-admin/src/store.rs`注释明确写"Writes left the trait when the Admin API write path was removed"，资源变更只能改配置文件+SIGHUP或直接写etcd，iDoris若要做可写Admin API必须完全自研。**本地运行时管理完全不存在**（全仓库grep `llama.cpp`/`mlx_lm`/进程spawn相关代码，仅测试代码里有3处`std::process::Command`用于测试SIGHUP重载）。
- **打分**：思路匹配度2/5（受限于三大缺口：预算、本地运行时管理、隐私路由链均不存在）；工程质量**5/5**（本次调研最高，测试覆盖183个e2e场景496用例，crate边界清晰，代码规模约11.6MB）。

### 2.3 llamastash/llamastash

- 链接：https://github.com/llamastash/llamastash
- **许可证**：LICENSE文件本身是标准MIT。**需要注意**：README第411-414行额外写了"The Software shall be used for Good, not Evil. This software shall not be used for any military purposes including intelligence agencies."——不在LICENSE文件内，法律效力存疑，建议只借鉴设计、大改重写而非逐行拷贝代码，或联系作者确认。
- **维护活跃度**：171 star，2026-05-20建仓，2026-09-27仍有推送，deepu105一人主导1078次提交+8位外部贡献者，近两个月持续有"修bug→改文档→再修"的真实调试轨迹，外部PR经2-6轮review，已发布到crates.io/Homebrew/AUR/Scoop。README自承"Multiple AI Coding Harnesses and LLMs were heavily used"，但持续维护是真实的。
- **架构与深读发现（详见第4.3节）**：子进程spawn管理外部推理二进制（不内嵌引擎），后端目录`src/backend/`覆盖llama_cpp/vllm/sglang/lemonade/ds4，**没有MLX/mlx_lm.server/oMLX**（有一份未落地的MLX设计文档，见4.3节）。核心是`src/daemon/supervisor.rs`（1305行，`Launching→Loading→Ready|Error→Stopping→Stopped`状态机）和`src/proxy/router.rs`（1020行，多模型同时驻留+独立端口41100-41300+空闲驱逐）。YAML+CLI+环境变量三层配置，不依赖外部数据库，单二进制可跑。
- **功能对照**：OpenAI/Anthropic/Ollama三套兼容路由✓；**本地运行时生命周期管理为本次最强**（状态机+健康探测+日志轮转+空闲驱逐+单飞并发协调+端口冲突三态判定，见4.3节）；虚拟key/预算/隐私路由/guardrail 均不存在（不是网关，是运行时管理器）；崩溃后**无**自动重启（深读确认，需iDoris自建退避重试）。
- **打分**：思路匹配度4/5（专注本地运行时管理这一单一维度，几乎就是iDoris要的东西，但不支持MLX是最大缺口）；工程质量4/5（有CI、Coveralls覆盖率、46个测试文件，含"真实二进制注入异常参数"的测试方法论）。

### 2.4 EricLBuehler/mistral.rs（聚焦 mistralrs-server-core）

- 链接：https://github.com/EricLBuehler/mistral.rs ；模型管理文档：`docs/.../multiple-models.mdx`
- **许可证**：标准MIT，Copyright (c) 2024 Eric Buehler，纯净无附加条款。
- **维护活跃度**：7715 star（本次候选最高），717 fork，392 open issues，`pushed_at`2026-09-25（2天前），近5次commit高频修复+发布节奏，个人主导但社区PR活跃（编号已到2455+）。
- **架构（仅评估server/模型管理部分）**：多crate workspace（`mistralrs-core`推理核心/`mistralrs-server-core`HTTP server逻辑/`mistralrs-cli`/`mistralrs`对外SDK）。TOML多模型配置支持`[[models]]`数组，一个进程内同时加载多个独立engine模型。
- **核心亮点（本次调研模型生命周期管理最佳实践）**：`GET /v1/models` 列出已加载模型（含`status: loaded/unloaded/reloading`状态机）；`POST /v1/models/unload`、`POST /v1/models/reload`、`POST /v1/models/status`——**生产可用、有OpenAPI schema的成熟设计**，直接对应iDoris"驱逐/加载模型"需求；Rust原生`MultiModelBuilder`（`add_model`/`add_model_with_alias`/`with_default_model`）可代码层面管理多模型。Server同时兼容OpenAI Chat Completions、OpenAI Responses API、Anthropic Messages API。
- **需要如实说明的局限**：它本身是推理引擎+server，**不代理外部云provider**，无多provider转发、无虚拟key/预算/隐私路由/guardrail概念，"网关"意义上和iDoris需求方向不同，只应摘取其"模型load/unload/reload/status"这套HTTP语义去参考实现，而不是vendor整个crate（会拖入巨大的推理引擎代码，18.2MB Rust代码估算35-40万行）。
- **打分**：思路匹配度整体2.5/5，但**模型生命周期管理维度5/5**；工程质量4.5/5（OpenAPI自动生成、多语言binding、CI完善，单点维护者是风险）。

### 2.5 Noveum/ai-gateway

- 链接：https://github.com/Noveum/ai-gateway
- **许可证**：MIT OR Apache-2.0双许可（`LICENSE-MIT`+`LICENSE-APACHE`均为标准文本），纯净，是本次候选里最宽松的组合之一。
- **维护活跃度**：97 star，13 fork，`pushed_at`2026-08-24，近5次commit集中在同一天做发布收尾，节奏中等，工程规范严谨（CHANGELOG、SemVer、`deny.toml`依赖审计、`VALIDATION.md`发布检查单）。
- **架构**："One package, three deployment shapes"——native二进制/Rust library/Cloudflare Worker(wasm32)。**无强制外部存储**：凭证per-request传入不持久化；Nova Guard策略引擎默认"local policy bundle，entirely in-process，no network dependency"。BYOK模式下预算是**内存态ledger（advisory），重启即丢失，未持久化**，是与iDoris核心差距。
- **亮点**：`admission.rs`/`admission_wire.rs`/`usage.rs`/`pricing.rs`构成admission→usage两阶段结构，`cost_cap`策略类型支持`failClosed`字段（**四者/多项目里唯一显式支持"拿不到状态就拒绝"语义**）；Guardrail强项——`pii_detection`(mask/block)、`regex_match`、`token_length_cap`等9种in-process可执行策略，5种"预留"类型需外部评分服务。
- **打分**：思路匹配度3.5/5（fail-closed设计理念与iDoris高度一致，扣分在预算未持久化、无虚拟key/租户隔离、无本地运行时管理）；工程质量4/5（发布规范严谨，但仓库规模小、star低，未经大规模社区检验）。

### 2.6 agentgateway/agentgateway（Linux Foundation 项目）

- 链接：https://github.com/agentgateway/agentgateway ；文档：https://agentgateway.dev
- **许可证**：Apache-2.0，逐字核对201行标准文本，无附加条款。
- **维护活跃度**：5059 star，890 fork，287 open issues，40 subscribers，`pushed_at`2026-09-27（**当天**），是本次除semantic-router外活跃度最高的项目之一。README确认为 **Linux Foundation 项目**（"Agentgateway is a Linux Foundation project"），语言实测：**Rust 7.88MB > Go 2.78MB**（Go 部分是 K8s controller），确认 Rust 为主体。
- **功能（基于README/官方文档页初步核实，未读源码，部分为"未核实"）**：定位是"Agent/MCP流量与LLM流量统一到一个数据面"，支持OpenAI/Anthropic/Gemini/Bedrock等主流provider；明确写有"budget and spend controls"（具体是否reserve/settle两阶段、是否需要外部数据库、是否有虚拟key/租户隔离概念——**均未核实**，官方文档页只有功能列表没有实现细节）；guardrail机制含regex、OpenAI moderation、AWS Bedrock Guardrails、Google Model Armor、自定义webhook（**具体实现方式未核实**）；Auth支持JWT/API keys/OAuth，配CEL策略引擎做细粒度RBAC；非K8s部署用"flat yaml config"，K8s部署用built-in controller+Gateway API。**本地模型运行时管理**：文档页未提及，未核实到存在。
- **重要提醒**：本项目未纳入本次"Top 3 深读"名单（因为是在报告写作过程中由协调者补充要求新增的候选，时间上来不及clone深读），**其思路匹配度/工程质量评分为初步估计而非源码核实结论**，如果后续要认真评估 iDoris 的基础选型，**强烈建议对 agentgateway 补做一轮clone+源码深读**，尤其是其budget/spend controls的具体实现（是否用SQLite/是否reserve-settle）——这可能是本次调研遗漏的一个重要候选，star数和活跃度均超过plano之外的所有候选。
- **打分（初步/未完全核实）**：思路匹配度估3/5；工程质量估4/5。

### 2.7 BerriAI/litellm 的 `litellm-rust` 子目录（新发现，重要信号）

- 链接：https://github.com/BerriAI/litellm/tree/main/litellm-rust
- **许可证**：`litellm-rust/Cargo.toml`（workspace）显式声明 `license = "MIT"`，该目录不在受限的`enterprise/`目录下，判定为纯MIT。
- **活跃度**：**极高**——commit历史显示几乎每天多次提交（截至调研当天2026-09-27仍有提交：`feat(rust): add the openai_like chat config foundation`），由LiteLLM核心团队（Yujong Lee等）主导，辅以AI辅助编码（Devin AI / Claude署名合作提交）。
- **规模与架构**：**40+ 个crate**的巨型workspace，覆盖：`litellm-router`、`litellm-gateway`/`litellm-gateway-inference`/`litellm-gateway-auth`、`litellm-auth`(+aws/azure/gcp变体)、`litellm-secrets`(+aws/google/hashicorp/azure/cyberark变体)、`litellm-cache`(+azure-blob/memory/redis/s3/gcs/disk/redis-semantic/qdrant-semantic变体)、`litellm-cost`、`litellm-token-counter`(+fast/huggingface/tiktoken变体)、`litellm-framer`(SSE流式)、`litellm-http`、`litellm-llms`、`litellm-tracing`、`litellm-coroutine`/`litellm-host`（一套`Machine`/`Host` trait驱动的异步状态机效果处理器抽象，设计理念上与llama-swap的"Effects接口"模式异曲同工）。
- **关键架构判断**：`litellm-host`/`litellm-host-python`/`litellm-callbacks-legacy-python`/`litellm-python-compat`几个crate的存在，以及workspace依赖`pyo3`/`pyo3-async-runtimes`，证实这套Rust代码**当前通过PyO3嵌入到既有Python LiteLLM进程里作为扩展运行，逐crate替换Python实现的同时保持"Python parity"（AGENTS.md明确要求测试锁定与Python行为一致）**，而不是一个独立可单独部署的Rust单二进制网关。检索`crates/host/Cargo.toml`未发现`[[bin]]`目标。
- **对iDoris的意义**：这不是当前可vendor/fork的产品，而是一个**极强的方向性信号**——LiteLLM（本领域功能最全的开源网关之一，虚拟key/预算/路由设计被公认最成熟）的商业团队正在用**几乎完全相同的crate切分方式**（router/auth/cache/cost/gateway/token-counter独立成crate）把核心重写成Rust，validates iDoris"用Rust做本地优先网关"的架构方向是对的。建议：**持续观察**该目录，等它演进出独立于Python宿主的单二进制形态后重新评估是否可vendor；现阶段可以参考的是它的**crate边界划分方式**和`AGENTS.md`里的错误处理规范（每crate一个`thiserror` `Error`枚举、测试放置规则）。
- **打分**：思路匹配度概念上4-5/5，但因**未完成、非独立可部署、深度耦合PyO3宿主**，暂不计入综合排名；工程质量因高强度AI辅助+持续演进中，暂标"未完全核实"。

### 2.8 Northwood-Systems/millwright

- 链接：https://github.com/Northwood-Systems/millwright
- **许可证**：Apache-2.0，纯净。
- **维护活跃度警示**：46 star，**`pushed_at`2026-08-10，已停滞7周**；main分支仅5次提交，全部集中在2026-07-21至22的26小时内（CI记录显示更早的2026-07-08已有构建，说明git历史被squash/重写过，真实开发期约1个月）；**只有一位贡献者**（andrewliu96）；v2分支CI失败7周无人处理；唯一的用户issue至今无回复。**Bus factor=1且已停滞，可持续性存疑**。
- **架构核实（这是本次除plano外唯二"单二进制+SQLite默认"的候选）**：`millwright serve`启动，`millwright init`生成配置，**默认SQLite，Postgres可选**，policy.json+models.json配置+JSON Schema校验。CI扎实（action版本锁SHA、`clippy -D warnings`、`cargo deny`、真实Postgres容器测迁移）。
- **两个宣传词核实结论**：
  - **"deterministic"是真的**：`src/router.rs`(1178行)+`policy.rs`(245行)全文搜索确认**一次都没用`rand::`**；路由按task_type/risk分cheap/mid/frontier档，档内选估算成本最低+健康的路由；有专门测试`warm_session_lru_refresh_and_eviction_are_deterministic`。
  - **"private by default"是营销夸大但不算捏造**：全仓库搜`privacy`/`local_only`/`fail-closed`，路由层**没有一处**做隐私判断；真实含义是"自托管、凭证不外泄、无强制托管控制面"+"离线成本报告默认脱敏"两层，跟iDoris要的"隐私优先路由"不是一回事；provider只支持OpenAI兼容/Anthropic/Bedrock三种云端协议，**不支持本地运行时**。
- **功能对照**：虚拟key✓（简化版，SHA-256存储+常量时间比较，仅三种scope，无完整租户隔离）；路由轨迹✓（`millwright trace <id>`可查决策+被拒绝备选）；预算✗（main分支无，v2设计是"实际成本累计"而非"预扣结算"）；guardrail/PII✗；本地运行时管理✗。
- **打分**：思路匹配度整体2/5，**确定性路由维度4/5**；工程质量3.5/5（代码规模小，1.55MB，80个.rs文件含27个集成测试文件，测试写法值得参考）。

### 2.9 vllm-project/semantic-router（仅方法论参考，非代码基础）

- 链接：https://github.com/vllm-project/semantic-router
- **语言实测**（`gh api .../languages`）：Go 23.4MB > Python 12.3MB > TypeScript 5.98MB > **Rust仅3.19MB**，`primaryLanguage`确认为Go。Rust仅存在于`candle-binding/`（通过cgo FFI被Go调用做BERT/ModernBERT分类推理），路由决策核心逻辑全部在Go。**不适合作为"Rust网关"的代码基础**。
- **许可证**：Apache-2.0，纯净。
- **维护活跃度**：5921 star，979 fork，`pushed_at`当天，526个open issue（体量大），隶属vLLM大型社区项目，每天多次提交，是本次活跃度最高的项目之一。
- **架构**：Envoy ext_proc(gRPC External Processor)架构，Go控制面+Envoy数据面，K8s CRD原生设计，`pkg/postgres`/`pkg/milvus`/`pkg/vectorstore`引入Postgres/向量库等重型依赖，与iDoris"个人版单二进制"方向相反。
- **对iDoris最有价值的部分——意图判定方法论**：`pkg/classification`目录300+文件实现了成熟的"信号驱动分级分类"体系：`category_classifier`(领域)、`complexity_classifier`(复杂度)、`contrastive_jailbreak_classifier`(越狱检测+滑动窗口)、`classifier_pii_*`系列(含长文本分片/部分扫描)、`hallucination_detector`(NLI校验)、`authz_classifier`+`authz_fail_open.go`(**授权失败时开放/关闭语义，直接对应iDoris的fail-closed诉求**)。关键设计模式：(1)用小型专用模型(ModernBERT/BERT量级)做分类而非依赖大模型，与iDoris思路一致且经生产验证；(2)每类信号有独立"决策痕迹"(`ranking_trace.go`/`trace.go`)，路由决策可解释可回放，与iDoris"确定性路由链、非黑盒"哲学完全一致；(3)`classifier_fail_closed_test.go`把fail-closed当作一等公民测试，少见的好范式；(4)`pkg/decision/engine.go`把隐私/复杂度/PII/越狱风险等多信号合成路由决策的"signal→decision"分层设计，可直接映射到iDoris"隐私→预算→意图→容量"决策链。
- **打分**：整体思路匹配度1.5/5（语言/部署形态南辕北辙），**意图判定方法论维度4.5/5**；工程质量5/5（测试密度极高，几乎每个classifier文件配`_test.go`，论文背书，CI/发布成熟）。

### 2.10 traceloop/hub

- 链接：https://github.com/traceloop/hub
- **许可证**：Apache-2.0，纯净。
- **维护活跃度**：229 star，`pushed_at`2026-07-16（约2.5个月无更新），近5次commit显示"功能提交+立即发版"节奏，但6月到7月中有约40天空档。YC背书商业公司维护。
- **架构**：单crate，Axum+Tokio。**YAML模式**零外部依赖但功能砍到只剩静态路由（无Management API/虚拟key/预算/审计）；**Database模式**提供Management API但**强制Postgres**（Cargo.toml仅启用`sqlx`的`postgres` feature，无sqlite feature）——与iDoris硬约束冲突。
- **功能对照**：OpenAI兼容✓；多provider✓（5个：OpenAI/Anthropic/Azure/Bedrock/VertexAI，无本地运行时provider）；观测强项（OTel tracing+Prometheus metrics原生）；虚拟key/预算/guardrail/本地运行时管理均无。
- **打分**：思路匹配度2/5；工程质量3.5/5（有CI、testcontainers集成测试、Swagger自动生成，但维护节奏一般）。

### 2.11 provider-client 类 Rust crate 对比（genai / async-openai / rig-core）

| 维度 | genai (jeremychone/rust-genai) | async-openai | rig-core |
|---|---|---|---|
| 许可证 | MIT OR Apache-2.0 ✅ | MIT ✅ | MIT ✅ |
| Star / Issue | 895 / 34 | 2019 / 24 | 8744 / 112 |
| 最近发版 | 0.7.0-beta.24（2026-09-23） | 0.42.0（2026-09-09） | 0.42.0（2026-08-17） |
| 累计下载(crates.io) | 41.7万 | **862万** | 304万 |
| 原生多协议 | ✅ 27+ providers，**含专门的`omlx` adapter** | ❌ 仅OpenAI协议(含Azure) | ✅ 20+ providers,含独立`llamacpp`模块 |
| 定位 | 纯出站客户端 | 纯出站客户端 | Agent应用框架(0.42起拆出`rig-core`纯provider层) |
| 接:8088成本 | **近零**——已有`omlx` adapter（默认endpoint `http://127.0.0.1:8000/v1/`，环境变量`OMLX_ENDPOINT`可覆盖指向:8088），另有通用`genai_{n}`机制零代码接任意OpenAI兼容端点 | 低——`Config` trait仅5个方法（headers/url/query/api_base/api_key），实现一个struct即可 | 未核实具体builder API细节，结构上可行 |

- `genai`的`omlx` adapter源码实测（`src/adapter/adapters/omlx/adapter_impl.rs`）：默认endpoint `http://127.0.0.1:8000/v1/`，环境变量`OMLX_ENDPOINT`覆盖、`OMLX_API_KEY`可选鉴权(`allow_no_api_key: true`)，内部复用`OpenAIAdapter`的请求/响应/流式转换逻辑，全文不到90行——是**开箱即用地覆盖iDoris全部本地运行时**的唯一候选（`omlx`直接对应oMLX，`genai_{n}`覆盖mlx_lm.server/llama.cpp server）。
- **风险**：`genai`0.6→0.7有breaking changes，且核心维护者单一（Jeremy Chone一人主导）。`rig-core`生态最热但README自曝"⚠️ Here be dragons"（明确警告未来数月持续破坏性变更），29个子crate依赖面广，作为核心依赖长期vendor风险较高。
- **建议**：`genai`是首选出站provider客户端，`async-openai`作为"仅需OpenAI协议"场景的最稳备选，`rig-core`仅作设计参考。

### 2.12 mostlygeek/llama-swap（Go，仅设计借鉴）

- 链接：https://github.com/mostlygeek/llama-swap ；MIT License，5759 star，481 fork，`pushed_at`当天，本次调研中Go项目活跃度最高。
- **三层职责拆分（最值得借鉴）**：**Process机制**(`baseRouter`)拥有OS进程/健康检查/HTTP转发；**调度策略**(`scheduler.Scheduler`，当前仅`FIFO`实现)管理队列/in-flight计数/决策树；**驱逐策略**(`scheduler.Swapper`，纯函数)给定target+running set返回必须停掉的模型集合，有`groupSwapper`(静态分组)和`matrixSwapper`(DSL代价求解器，支持并发跑多模型组合)两种实现。
- **核心设计模式**：整个状态机跑在**单一goroutine**的for-select事件循环里，调度器本身不需要锁；耗时加载动作丢到独立goroutine异步执行，完成后以`SwapDone`事件回灌事件循环——"事件进/副作用出(`Effects`接口)"模式（与前述`litellm-host`的`Machine`/`Host`抽象理念一致，说明这是业界公认的正确模式）。设计文档记录了真实事故(issue #946)：路由器在TTL卸载过程中读到`StateStopping`快照就跳过启动导致请求永久卡死——教训是"只读快照可以，但不能用快照去gate一次变更"，变更决策必须下沉到进程自己的单写者循环。
- **排队/超时**：全局`globalConcurrencyLimit`共享信号量超限429（不排队）；模型级`concurrencyLimit`(默认10)超限也429；加载等待靠`healthCheckTimeout`(默认120s)控制轮询`checkEndpoint`(默认`/health`)超时。
- **支持后端**：不区分具体类型，靠`cmd`(任意shell命令)+`proxy`(上游URL,可省略用`${PORT}`宏自动生成)+`checkEndpoint`三元组抽象，任何暴露OpenAI/Anthropic兼容HTTP端点的进程都可接入（mlx_lm.server同理可用）。`selectors`机制的`spillover`策略（对同一"虚拟模型"按预留并发数逐个补位启动下一个实例）对iDoris"容量"路由环节有直接参考价值。
- **移植到Rust需要重新实现的部分**：整个process生命周期管理（Go版靠`internal/process`包，Rust侧要用`tokio::process`+自建状态机，无直接可复用crate）；Scheduler/Swapper/Effects三接口分离的事件循环模式（设计可照搬，Go的channel+select要换成`tokio::sync::mpsc`+单task处理循环或actor模式）；YAML配置schema+宏展开解析器；`matrixSwapper`的DSL代价求解器（最复杂部分，iDoris若只需单模型独占显存可先只做等价于`groupSwapper`的简单策略）。**不需要重新发明的是设计层面的教训**（单写者状态机、驱逐决策必须是纯函数、swap异步化不阻塞事件循环）。

### 2.13 RouteLLM（论文+参考实现，仅方法论）

- 论文：[arXiv:2406.18665](https://arxiv.org/abs/2406.18665) "RouteLLM: Learning to Route LLMs with Preference Data"；代码：https://github.com/lm-sys/RouteLLM（Apache-2.0，5546 star，**`pushed_at`2024-08-10，已两年多未更新**，纯研究代码快照，不评估工程质量）。
- **方法论**：只做"强/弱模型二选一"路由，每请求关联一个cost threshold。用Chatbot Arena人类偏好数据训练路由器预测"强模型是否会赢得这次对局"。4种路由器实现：`mf`(矩阵分解，官方推荐)、`sw_ranking`(相似度加权Elo)、`bert`(分类器微调)、`causal_llm`(因果LLM微调)。
- **关键机制——阈值校准(Threshold Calibration)**：不在训练时定死切分点，而是**部署时**用`calibrate_threshold`脚本，基于目标"强模型调用占比"在校准数据集上反推阈值——把"预算约束"转换成打分空间里的百分位数切点。
- **量化结果**：相比全量调用GPT-4，可降低85%成本同时保留95%的GPT-4级质量；路由器换用不同强弱模型对时仍保持性能（学到的是query难度这个相对通用信号）。
- **对iDoris"预算→意图"路由环节的价值**：**阈值校准的两阶段设计可直接迁移**——先训练连续打分函数，再用校准脚本把业务约束(预算)映射成打分阈值，避免把预算和模型选择耦合进同一训练目标。**局限**：只做二元路由（iDoris面对多运行时+fail-closed约束更复杂）；**没有隐私/合规维度**，local_only的fail-closed语义RouteLLM完全没有涉及，必须自研；工程上不建议vendor/fork（Python研究代码+重量级依赖LiteLLM/sglang），只取方法论用伪代码/文档形式移植。

### 2.14 其他已排除/降低优先级候选（简要记录）

| 项目 | 结论 |
|---|---|
| Pushkinist/rMLX | MIT/Apache双许可，纯Rust MLX FFI引擎（非套壳Python），可作为被iDoris管理的oMLX/mlx_lm.server**替代后端**（而非管理器本身），单人维护(344次提交)，`clippy`配置严格(deny unwrap/expect/panic)，工程质量4/5，但只是"一个运行时"不是"管理多个运行时"。 |
| truespar/paddock | 许可证形式合规，但**来源存疑**：仓库仅存在24天(2026-09-03建仓)、81次commit却有2646个文件(含906个.rs+79个.cu)、**无CI**、commit作者与贡献者账号对不上，怀疑是私有仓库整体导入或大量AI生成，不建议采用。 |
| ferrumox/fox | MIT/Apache双许可，llama.cpp的FFI绑定(非子进程)，分页KV cache+前缀缓存设计(`src/kv_cache/mod.rs`+`src/scheduler/prompt_cache.rs`)有参考价值，但项目**已停滞5周(2026-08-22最后push)+近期CI持续失败**，最新PR来自AI代理分支，走下坡趋势。 |
| EricLBuehler/candle-vllm | MIT纯净，729 star活跃，但**每个进程只能加载一个模型**(README的"multi"指多GPU/多节点非多模型)，无独立测试目录，作为网关/运行时管理器均不适用，仅OpenAI协议struct可参考。 |
| doorman-dev/doorman | Apache-2.0表面合规，但Rust重写未完成(59678行迁移PR未合并)，通用多协议网关(REST/SOAP/GraphQL/gRPC)对LLM无原生概念，持久化强依赖MongoDB+Redis，排除。 |

---

## 3. 重点分析：TensorZero

> 基于本机 `/Users/jason/Dev/auraai/Agent24/vendor/reference/tensorzero` 源码全量核查（`git log`确认为2026-06-04的`62eb8f6`提交，`gh api`确认与GitHub远端HEAD一致）。

### 3.1 许可证

- LICENSE 文件201行，逐字核对为**纯净Apache-2.0**，无任何附加条款、无NOTICE异常。`gh repo view`确认`licenseInfo.key = "apache-2.0"`。CLA.md基于Apache基金会ICLA v2.2，标准贡献者协议，不影响用户侧许可条款。

### 3.2 维护活跃度（重要提醒）

- Star **11,716**，Fork 969，Open issues 392。**`pushed_at = 2026-06-11T01:48:44Z`**，而`updated_at = 2026-09-27T09:05:31Z`（updated_at包含star等非代码事件）。即：**代码层面已约3.5个月无新推送**（对比调研当天2026-09-27），最新release是`2026.6.0`(2026-06-04)。这是一个需要在决策时重点关注的信号——不确定是团队转向商业化Autopilot产品（README提到"TensorZero Autopilot是配套付费产品"）导致开源侧降速，还是其他原因，**建议决策前向社区/官方渠道进一步确认**。

### 3.3 Gateway crate 结构

- **`gateway` crate本身仅2617行**（`main.rs`/`router.rs`/`cli.rs`/`routes/`），是一个**薄壳**：依赖`tensorzero-core`（完整业务逻辑）、`tensorzero-optimizers`、`evaluations`、`tensorzero-auth`、`autopilot-*`（付费Autopilot相关）、`tensorzero-mcp`、`durable-tools`等约10个内部crate。
- **`tensorzero-core`crate高达363,259行**，占全workspace（469,694行）近8成。workspace共**43个crate**。这意味着：想把"gateway"单独抽出来当轻量library几乎不可能——真正的重量级逻辑全部耦合在`tensorzero-core`里，直接fork/vendor需要巨大的裁剪工作量。

### 3.4 存储依赖：是否强依赖 ClickHouse

- `crates/tensorzero-core/src/db/`目录下**只有`clickhouse/`、`postgres/`、`valkey/`三个后端目录**，全仓库搜索**没有SQLite支持的任何痕迹**。
- **可以在无DB情况下降级运行**：`observability.enabled`配置项支持`null`（默认，DB不可用时打印警告继续运行）/`true`(强制要求DB可用,否则启动失败)/`false`(完全禁用观测)三态；代码里存在`ClickHouseConnectionInfo::new_disabled()`用于测试和降级场景。
- **但完整的feedback/experimentation/variant统计功能仍需要ClickHouse或Postgres**（`db/postgres/feedback.rs`确认Postgres也可作为observability backend，"如果Postgres和ClickHouse都可用，优先用ClickHouse"）。**没有SQLite适配层**，与iDoris"个人版单二进制+SQLite"的硬约束直接冲突，除非iDoris自己实现一个SQLite版`ConfigQueries`/`FeedbackQueries`等trait实现（工作量未知，需要另行评估这些trait是否足够干净可插拔）。

### 3.5 Feedback / Experimentation 模型与 iDoris 轨迹方案的契合度

- Feedback相关代码分散在`endpoints/feedback/`（含`human_feedback.rs`、`internal/count_feedback.rs`、`internal/cumulative_feedback_timeseries.rs`等）+ `db/feedback.rs`（trait） + `db/clickhouse/feedback.rs`/`db/postgres/feedback.rs`（两套后端实现）。设计上是"metric-based feedback"（对某次inference打分/标注demonstration），概念上与iDoris"feedback端点"部分契合，但数据模型深度绑定ClickHouse/Postgres的schema迁移文件（`crates/tensorzero-core/src/db/postgres/migrations/20260116210000_feedback_tables.sql`），非存储无关的抽象。
- Experimentation设计（`crates/tensorzero-core/src/experimentation/static_experimentation.rs`）：`StaticExperimentationConfig`用`candidate_variants: WeightedVariants`(BTreeMap<String,f64>权重表，可从纯数组或权重map反序列化)+`fallback_variants`做A/B流量分配，是一个干净的、纯数据结构层面的实验配置抽象（不直接耦合ClickHouse），**这部分设计思路值得参考**（尤其是"数组=等权重、map=显式权重"的双态反序列化模式），可以移植到iDoris自己的实验/灰度配置里。
- **Provider覆盖广**：`crates/tensorzero-core/src/providers/`下约20个provider文件（anthropic/aws_bedrock/aws_sagemaker/azure/deepseek/fireworks/gcp_vertex/google_ai_studio_gemini/groq/hyperbolic/mistral/openai/openrouter/sglang/tgi/together/vllm/xai + 通用`chat_completions.rs`可覆盖任意OpenAI兼容端点），provider抽象层设计成熟度高，是TensorZero相比其他候选的一个亮点。
- **`provider-proxy`crate**（1986行）只是一个用于测试的MITM缓存代理（"Heavily based on http-mitm-proxy, MIT-licensed"），非生产组件，不具备复用价值。

### 3.6 TensorZero 综合结论

**不建议fork或vendor TensorZero作为iDoris的基础**：(1) 469,694行的巨型workspace，`gateway`与`tensorzero-core`高度耦合，裁剪成本极高；(2) 存储层深度绑定ClickHouse/Postgres，无SQLite适配，与硬约束直接冲突；(3) 近3.5个月无代码推送，维护节奏存疑。**可借鉴的部分**：(a) provider抽象层的协议覆盖广度和`chat_completions.rs`通用OpenAI兼容adapter模式；(b) `WeightedVariants`双态反序列化的实验配置设计；(c) `observability.enabled: null/true/false`三态降级设计模式（DB不可用时警告继续 vs 强制要求 vs 完全禁用）——这个"降级三态"思路值得直接搬到iDoris自己的可选轨迹/审计存储配置里。

---

## 4. Top 3 深读：可直接复用的模块清单（附文件路径）

> 三个仓库均已 `git clone --depth 1` 到 `scratchpad/oss/{plano,aisix,llamastash}`。以下路径均相对各自仓库根目录。

### 4.1 katanemo/plano（`scratchpad/oss/plano`）

**深读后的重大修正**：不同于初步判断"Envoy+WASM+Python四件套强耦合、只能挑孤立算法"，深读发现 **`brightstaff` 和 `crates/hermesllm` 均不依赖 proxy-wasm**，是自给自足的纯Rust async服务（`brightstaff`的`send_upstream()`直接用`reqwest::Client`发起上游请求），真正跟Envoy/WASM ABI强耦合的只有`crates/prompt_gateway`/`crates/llm_gateway`两个插件crate——而这两个插件crate里的护栏逻辑（`PromptGuards`/`GuardType::Jailbreak`）经`grep`确认全仓库无调用点，是死代码。

| 优先级 | 文件路径 | 内容/关键类型 | 复用理由 | 改造成本 |
|---|---|---|---|---|
| 1 | `crates/hermesllm/`（整个crate，约5800+行；关键文件`transforms/request/from_openai.rs`1782行、`transforms/response/to_anthropic.rs`761行、`transforms/response/to_openai.rs`1209行） | 纯Rust库，零proxy-wasm依赖，OpenAI/Anthropic/Bedrock/Gemini/Mistral/Grok互转，含流式SSE处理、`ProviderId`/`provider_cache_capability`、token usage提取 | iDoris单二进制网关必然要做多provider格式转换，本来就无ABI耦合，可直接引入或整体搬运 | 低 |
| 2 | `crates/common/src/traces/{shapes.rs, span_builder.rs, resource_span_builder.rs}` | `SpanBuilder`/`ResourceSpanBuilder`纯struct+serde的OTel-JSON span构造器 | ATIF v1.8+OTel双格式导出的底座 | 几乎为零（纯数据结构） |
| 3 | `crates/brightstaff/src/handlers/llm/session_router.rs`（`route()`函数）+ `crates/brightstaff/src/router/orchestrator.rs`（`switch_cost_in_usd`/`estimate_switch_cost_in_usd`） | 会话缓存warmth判断+换模型美元成本计算+按`max_switch_spend_pct`闸门决策，每分支有明确`decision_label`/`reason`常量写入span（如`plano.switch.reason=same_anchor/free/within_cap/over_cap/no_pricing`），完整单测覆盖 | 本次读到的**工程质量最高、最贴合"确定性/可配置/可解释"**的决策代码，虽语义是"会话级换模型成本闸门"非"虚拟key预算"，但决策骨架(fail-open on missing pricing、reason-code化、span记录)可直接套用 | 中（需把session_cache的Memory/Redis实现换成SQLite） |
| 4 | `crates/brightstaff/src/router/orchestrator_model_v1.rs` | `generate_request()`做消息过滤/token估算裁剪(`MAX_ROUTING_TURNS=16`、`trim_middle_utf8`中间截断)，`parse_response()`解析JSON并做`fix_json_response`容错，930行单测 | "意图路由"骨架（token预算裁剪+JSON容错解析），**但注意**：这不是确定性分类器，是对大模型做一次chat completion调用，JSON失败时fail-open回落默认model——iDoris若要确定性分类需自己另起炉灶 | 中 |
| 5 | `crates/brightstaff/src/handlers/agents/pipeline.rs`（`PipelineProcessor::process_raw_filter_chain`/`execute_raw_filter`、`PipelineError`枚举） | 外部HTTP/MCP filter chain：`filters`声明为`{id,url,type}`，按id引用链式调用，任何一环4xx/5xx/网络失败即中断整链并透传错误码——**fail-closed无旁路** | guardrail/PII过滤扩展点的架构范式（外部服务化+可插拔+顺序链+失败即拒绝）与iDoris需求契合 | 中（去掉`ENVOY_API_ROUTER_ADDRESS`默认值等Envoy相关部分） |
| 6 | `crates/brightstaff/src/tracing/posthog_exporter.rs` | 实现标准`opentelemetry_sdk::trace::SpanExporter` trait：过滤span→映射PostHog专有JSON schema→批量POST，网络失败只记日志不阻塞主流程 | ATIF v1.8自定义导出器的直接模板（filter→map→batch write结构） | 低（设计模式复用，非逐行抄） |
| 7 | `crates/common/src/configuration.rs`（1465行，设计参考非代码复用） | `Routing`/`RoutingBudget`/`Listener`/`AgentFilterChain`/`SelectionPolicy`/`PromptGuards`等struct，`RoutingBudget::resolve()`的resolve-and-validate模式 | Admin API/配置文件schema设计参考 | 是思路搬运非代码搬运 |
| 8 | `crates/brightstaff/src/router/model_metrics.rs`（`ModelMetricsService::rank_models`） | 基于实时价格(DigitalOcean/models.dev)/延迟(Prometheus)按`SelectionPolicy{prefer}`排序候选模型 | 路由链"容量"环节参考 | 中 |
| 9 | `crates/brightstaff/src/handlers/routing_service.rs`（`/routing`端点） | 独立于代理路径的"纯决策"端点，只返回`{models,route,trace_id,session_id,pinned,switched}` | "decision-as-a-service"模式可直接抄 | 低 |

**deep-read发现的关键限制**（不可复用/需完全自研的部分）：PII检测/脱敏在核心Rust代码里**不存在**（唯一实现是44行的header脱敏 + `demos/filter_chains/pii_anonymizer/pii.py`演示级Python微服务，README自承"生产环境建议用Microsoft Presidio"）；**完全没有虚拟key/API key管理/预算reserve-settle账户体系**（全仓库grep不到quota/credit/reserve/settle/balance相关代码，`RoutingBudget`语义是"会话级换模型成本上限"非"虚拟key预付费"）；**完全没有Admin API**（`brightstaff`路由表里没有配置热加载/key管理CRUD/任何管理后台，配置是启动时一次性parse YAML）；**完全没有隐私路由维度**（grep不到data residency/本地优先相关逻辑）。

### 4.2 api7/aisix（`scratchpad/oss/aisix`）

Cargo依赖图核实：`aisix-core`是唯一被所有crate依赖的底座，`aisix-gateway`是第二层公共依赖，`aisix-proxy`（113,523行，占全仓库近一半）反过来依赖几乎所有其他crate，是应用层粘合代码非可剥离library——这是"不整体fork主干"的关键依据。`aisix-cache`(3,631行)、`aisix-ratelimit`(3,718行)、`aisix-guardrails`(25,378行)三者只依赖`aisix-core`+`aisix-gateway`(+可选`aisix-redis`)，**不依赖`aisix-etcd`/`aisix-admin`**，在依赖图层面确实独立可单独vendor。

| 优先级 | 文件路径 | 内容/关键类型 | 复用理由 | 改造成本 |
|---|---|---|---|---|
| 1 | `crates/aisix-proxy/src/cooldown.rs`（325行，全文件已读） | `decide_cooldown(err, cfg) -> Option<(Duration, reason)>`纯决策函数，文档明确解释"为什么单独抽出"（2023年前cooldown逻辑散落各dispatch路径曾漏接导致bug类H-1审计问题） | 依赖极简(仅`aisix_core::CooldownConfig`+`aisix_gateway::BridgeError`)，本次调研"性价比最高"的可直接抄文件之一 | 几乎可直接抄 |
| 2 | `crates/aisix-guardrails/src/pii.rs`（903行） | `BUILTIN_DETECTORS`内置11种正则检测器（email/china_mobile/china_id_card含ISO 7064校验/bank_card含Luhn校验/us_ssn/ip_address/api_key多厂商特征串/jwt/private_key PEM块），支持Mask(含仅脱敏捕获组1)或Block | 完全本地实现无外部依赖，可直接抄；`presidio.rs`/`lakera.rs`是外部HTTP服务调用（**深读确认自托管场景不依赖付费Cloud层可正常工作**，纠正了此前"可能强耦合"的顾虑） | 直接抄内置检测器；Guardrail trait需简化裁剪（原1673行lib.rs+4731行build.rs组装逻辑偏重） |
| 3 | `crates/aisix-cache`（整个crate，约2,552行不含测试） | 精确匹配+语义(embedding相似度)缓存分层，`Cache`/`CacheError`/`CacheOutcome` trait，memory默认+redis可选feature | 依赖极简(`aisix-gateway`+`aisix-core`+`aisix-obs`+可选`aisix-redis`)，无Cloud耦合痕迹，"开箱即用性"本次最高 | 整体vendor，替换`ChatResponse`类型 |
| 4 | `crates/aisix-ratelimit`（`store/mod.rs`的`RateStore` trait 133行 + `limiter.rs` 968行） | `Limiter::pre_commit()`返回`Reservation`，请求结束后`commit_tokens()`落地真实用量——RPM/RPD前置check-and-increment，TPM/TPD前置检查+响应后结算 | **这套"reserve前置估算→commit后置结算"模式结构上和预算reserve/settle几乎同一模式**，可直接照抄`RateStore` trait形状改造成预算引擎 | 中（金额化+接入`aisix-core::PricingIndex`定价表+`LocalStore`内存态换成SQLite事务表） |
| 5 | `crates/aisix-gateway/src/bridge.rs`（`Bridge` trait，第773行起） | `name()`/`wire_protocol()`/`chat()`/`chat_stream()`/`embed()`/`complete()`/`generate_image()`，`BridgeError`枚举每变体自带`http_status()`/`reached_upstream()`/`error_type()` | provider抽象层trait设计模板 | 直接抄trait设计 |
| 6 | `crates/aisix-provider-openai/src/bridge.rs`（2196行）+ `wire.rs`（1664行） | `resolve_base()`（bridge.rs:145）：只在`api_base`为空且vendor非"openai"时拒绝，否则原样透传，文档明确写"reusable by OpenAI-compatible providers...construct it with a different api_base" | **接oMLX:8088/mlx_lm.server/llama.cpp server大概率不需要写新provider crate**，配置`api_base="http://127.0.0.1:8088/v1"`+任意非空占位`api_key`即可（注意`api_key(ctx)`对空key硬性报错，本地后端也要填占位符） | 低-中（视响应字段解析严格度而定） |
| 7 | `crates/aisix-admin/src/store.rs`（`ConfigStore` trait，290行） | 9种资源×get/list全异步trait，已有`InMemoryStore`/`FileManagedStore`/`EtcdConfigStore`三种实现 | 抄trait形状自己实现`SqliteConfigStore`，工作量小 | 低（但**注意**：这只是Admin API读服务层，驱动实际路由决策的快照加载逻辑在`aisix-server/src/main.rs`是硬编码if/else，无统一`ConfigProvider` trait，接SQLite需要照`aisix-core::filesource`模块形状重新写加载器，工作量比预想稍大） |
| 8 | `crates/aisix-core/src/snapshot.rs`（`SnapshotHandle`/`AisixSnapshot`） | arc-swap版本化快照，与存储后端无关 | 直接抄这个版本化快照模式 | 低 |
| 9 | `crates/aisix-obs/src/sink/record.rs`（333行） | `SinkRecord`包装`UsageEvent`(标准元数据总是有)+可选`SinkContent`(完整prompt/response，只有导出器显式`content_mode=full`才有)，注释原文"content lives in a separate field so the default metadata-only path can never carry a prompt" | **与iDoris"元数据审计不存内容"的架构原型高度吻合** | 抄架构思路不抄具体字段（`UsageEvent`大量字段绑定cp-api wire contract需裁剪） |
| 10 | `crates/aisix-proxy/src/budget.rs`（826行，仅LRU+fail-mode降级部分约100行值得看） | LRU缓存(容量10000,TTL 5秒)+三态`FailMode::Sticky/Open/Closed`降级容错 | 只抄"外部依赖不可用时优雅降级"的模式，**预算逻辑本身完全不可用**（该模块就是纯粹的cp-api RPC客户端，零本地预算计算） | 仅降级模式部分可抄 |

**深读修正的三处重要判断**：(1) 预算缺口比想象中更彻底——不是"大概没实现"，而是代码证实`budget.rs`就是纯RPC客户端；(2) 存储抽象层是"两层"而非"一层"——读服务层(`ConfigStore`)干净可插拔，但驱动路由决策的快照加载层是硬编码if/else无统一接口；(3) Admin API完全只读、虚拟key无轮换机制——此前"功能覆盖面完整"的印象需要修正，可写的Admin能力和key轮换都是Cloud控制面专属。

### 4.3 llamastash/llamastash（`scratchpad/oss/llamastash`）

许可证提醒重申：README的"禁军事用途"条款不在LICENSE文件内，建议只借鉴设计重写而非verbatim拷贝。

| 优先级 | 文件路径 | 内容/关键类型 | 复用理由 | 改造成本 |
|---|---|---|---|---|
| 1 | `src/daemon/supervisor.rs`（1305行） | `ManagedState`枚举状态机(`Launching→Loading→Ready\|Error→Stopping→Stopped`)，`transition()`用match显式穷举合法迁移表；`tokio::process::Command`+`setsid()`独立进程组；stdout/stderr双路捕获(有界环形缓冲`RingBuffer`4096行+落盘日志10MiB轮转保留5段)；SIGTERM等5秒→SIGKILL；PID复用防护(`try_wait()`确认存活再发信号) | 后端无关设计（llama.cpp/vLLM/sglang/ds4共用同一份代码），几乎可整体照搬 | 低——只需替换`ProcessLaunchSpec`类型 |
| 2 | `src/daemon/probe.rs`（`poll_until_ready`） | 手搓HTTP/1.1(`TcpStream`发固定GET文本解析状态行)，`ProbeOptions{interval,timeout}`默认500ms/120s，`scale_for_model(weights_bytes)`按模型体积动态放宽超时(最多+2小时) | 依赖极简（不引入reqwest/hyper），健康探测轮询直接可抄 | 极低 |
| 3 | `src/proxy/coalesce.rs`（406行） | **单飞(single-flight)协调而非排队**：`HashMap<(ModelId,launch_name), Arc<SlotInner>>`注册表，第一个请求为Leader真正执行spawn，其余为Follower park在`Notify`+`Mutex<SlotState>`上等唤醒；显式处理"Leader在Follower park前就finish()"的竞态(状态先写mutex，Follower await前后各查一次)；Leader的`Drop`兜底防止panic导致Follower永久挂起 | 多个并发请求打到同一尚未启动的mlx_lm.server/oMLX模型时，不应重复spawn N次进程，这套Leader/Follower+durable slot state可直接照搬（约200行核心逻辑不含测试） | 低 |
| 4 | `src/proxy/eviction.rs`（341行） | 定时扫描(`cadence=min(30s,max(5s,ttl))`)，`decide()`纯函数四条件合取(origin!=AutoStart & state==Ready & inflight==0 & now-last_request_at>=ttl)；对"共享umbrella进程"(类似oMLX场景)有专门分支`unload_idle_umbrella_model`——**不杀进程，只调后端unload API卸载模型**，进程本身常驻 | umbrella分支对iDoris很有参考价值，oMLX/mlx_lm.server这种"已在跑的常驻服务"更接近这个模式而非process-per-model | 低 |
| 5 | `src/daemon/ports.rs`（144行）+ `src/backend/lemonade/backend.rs`的`umbrella_port_state`（约30行） | 基础分配：线性扫描41100-41300做`TcpListener::bind`探测；umbrella场景**三态冲突判定**：bind失败后补一次`connect_timeout`(250ms)——`Free`(bind成功)/`Listening`(bind失败+connect成功=真冲突)/`Remnants`(bind失败+ConnectionRefused=TIME_WAIT残留应重试) | 对iDoris做oMLX:8088健康检查/端口探测（区分"真的有oMLX在跑"vs"端口暂时占用"）直接可用 | 极低 |
| 6 | `src/gpu/metal.rs`（165行，**gpu/目录下唯一适用于macOS的文件**） | `system_profiler SPDisplaysDataType -json`探测Apple Silicon，统一内存优先读`spdisplays_vram_shared`字段，解析失败fallback到`sysinfo::System::total_memory()` | macOS场景直接可用，`gpu/{amd,nvidia,vulkan,sysfs,dxgi}.rs`均与Linux/Windows/独立显卡相关，与iDoris无关可忽略 | 极低（换`GpuDevice`结构体即可） |
| 7 | `Backend` trait 的设计思路（`src/backend/mod.rs`2092行，**不建议整体照搬**） | 两种生命周期形状划分(`Lifecycle::ProcessPerModel` vs `ManagedMultiplexer`)；`ProcessLaunchSpec`/`Readiness::HttpPoll`数据结构；必须实现方法仅6个(其余约50个有默认实现)；`ModelIdentity::Backend{backend,name}`的synthetic identity模式(`lemonade://<name>`虚拟路径scheme) | oMLX场景应被建模为**第三种更轻量的形状**（"外部已启动、只读健康检查+转发"），比现有两种都轻；synthetic identity模式可直接照抄做`omlx://<model-name>` | 中——只抄接口设计理念（分离identify/prepare_launch/start/stop且有默认实现），不建议照搬整个trait（被GGUF生态深度绑定） |
| 8 | `tests/supervisor_lifecycle_test.rs`（428行）+ fake server fixture | 编译一个真实的`fake_llama_server`二进制(behind `--features test-fixtures`)模拟被管理后端，支持CLI flag注入异常(`--health-delay-ms 5000`模拟慢加载、`--trap-sigterm`模拟不听话进程) | 测试自己supervisor实现的最佳范式：真实二进制+注入异常flag，而非纯mock | 低——写iDoris自己的假oMLX/mlx_lm.server fixture |

**重要发现（docs/plans/未落地设计文档）**：`docs/plans/2026-06-24-002-feat-mlx-backend-plan.md`（状态"active"，`TODO.md`确认至今仍是"needs a Mac"未落地开放项，**仓库里没有`src/backend/mlx/`目录**）记录了详细MLX设计推演，关键协议细节：**`mlx_lm.server`没有`/health`端点，健康检测必须改为轮询`/v1/models`返回200**（作者标注"deferred to implementation"待真机验证：`/v1/models`是否会在模型真正加载完成前就返回200）；MLX应走`ProcessPerModel`；不需要GGUF式显存admission投影。这份文档没有涉及oMLX（HTTP已在跑、需代理转发）场景，只涉及"llamastash自己spawn mlx_lm.server"场景，但其协议细节推演本身直接可用。

**两个遗留问题的确认答案**：(1) **崩溃后无自动重启**——`exit_watcher` task只做状态分类(Launching/Loading阶段挂了→`Error`；Ready/Stopping阶段挂了→`Stopped`)，全仓库grep `restart`均为daemon手动重启或注释性文字，无子进程crash后自动重新spawn的代码路径，iDoris需自建退避重试计数器（无代码可抄）。(2) **端口冲突处理**——已确认为纯OS socket语义的bind-probe-skip（进程池）+ bind+connect三态判定（长驻服务），无文件锁/外部协调服务。

**oMLX适配器改造工作量判断**：oMLX场景（已在跑的`http://127.0.0.1:8088`，无需spawn，仅需健康检测+转发）比llamastash现有任何后端都更轻量。若照搬`Backend` trait完整契约：新增一个mirror `lemonade/backend.rs`但砍掉"起进程/umbrella生命周期"部分的文件，估计**150-250行**（identify+prepare_launch+start(健康检查)+stop(no-op)+少量knobs/accelerators样板）+ `backend/mod.rs`枚举登记（约5-10处小改动）。**但更合理的选择**是不照搬整套trait，只借用"状态机+probe+eviction+coalesce"这几个通用模块，自己写一个轻量`Runtime` trait（可能仅3-4个方法：`health_check()`/`forward(req)`/`is_ready()`/`id()`），此时oMLX适配器可能**50-100行**即可完成——比llamastash任何一个后端都简单，因为它本质只是"注册固定host:port，定期探活，转发请求"。

---

## 5. 建议

### 5.1 总体路径：不整体 fork 任何一个项目，走"分层借鉴 + 少量代码级复用"

没有一个候选能同时满足iDoris的三大核心诉求（单二进制+SQLite / 隐私fail-closed确定性路由链 / 本地运行时生命周期管理），每个项目只覆盖了其中一到两个切片。**建议按功能层分别选择参考对象，自己搭建骨架**：

| iDoris 功能层 | 建议参考/依赖对象 | 方式 |
|---|---|---|
| Provider 协议转换（出站客户端） | 首选 **`genai`** crate（已有omlx adapter，可直接依赖pin版本）；备选 **`hermesllm`**（plano的crate，零WASM依赖，可整体vendor）或 **`aisix-provider-openai`**的`resolve_base`模式（多数场景可能连新provider都不用写，配置自定义`api_base`即可） | 依赖 crate 或 vendor 单个 crate |
| 本地运行时生命周期管理（load/unload/驱逐） | 骨架抄 **llamastash**的`supervisor.rs`(状态机)+`probe.rs`(健康探测)+`coalesce.rs`(单飞并发)+`eviction.rs`(空闲驱逐，含umbrella分支)+`ports.rs`(端口冲突三态判定)；oMLX/mlx_lm.server专属细节参考其**未落地MLX设计文档**（`/v1/models`轮询替代`/health`）；协议语义参考 **mistral.rs**的`/v1/models`+unload/reload/status API | 借鉴设计，大改重写（勿verbatim拷贝，因README许可条款存疑） |
| 路由/fallback/retry/cooldown | 骨架抄 **aisix**的`cooldown.rs`(纯函数)+6种路由策略选择算法；决策可解释性范式参考 **plano**的`session_router.rs`(reason-code化+span记录) | 借鉴设计+部分代码 |
| 限流→预算 reserve/settle | 架构抄 **aisix-ratelimit**的`RateStore` trait(pre_commit/commit两阶段模式)，金额化后自建SQLite事务表；fail-closed语义参考 **Noveum**的`failClosed`字段设计 | 借鉴架构，自研实现 |
| Guardrail/PII 扩展点 | 内置正则检测器直接抄 **aisix**的`pii.rs`（11种检测器+Luhn/ISO7064校验）；外部服务化filter chain架构参考 **plano**的`pipeline.rs`(fail-closed无旁路) | 部分代码复用+架构借鉴 |
| 意图判定（小模型分类） | 方法论参考 **semantic-router**的`pkg/classification`+`pkg/decision`（信号驱动分级分类+可回放决策trace+fail-closed测试范式）；避免照搬**plano**的"大模型一次性判断+fail-open"模式（不够确定性） | 仅方法论借鉴，自研实现和训练 |
| 预算路由的阈值校准 | 方法论参考 **RouteLLM**的"打分模型+部署时阈值校准"两阶段设计 | 仅方法论借鉴 |
| 观测/ATIF轨迹导出 | 模板参考 **plano**的`posthog_exporter.rs`(filter→map→batch write)；元数据/内容分离架构参考 **aisix**的`SinkRecord`/`SinkContent`分离设计；降级三态思路参考 **TensorZero**的`observability.enabled: null/true/false` | 借鉴架构 |
| 缓存层 | 整体vendor **aisix-cache** crate（精确匹配+语义缓存分层，依赖极简） | vendor crate |
| Admin API / 配置存储 | 无好的现成范本（aisix只读、plano没有、TensorZero强绑定ClickHouse/Postgres），**需要完全自研**，可参考aisix的`ConfigStore` trait形状 + plano的`configuration.rs` schema设计思路 | 完全自研，仅形状参考 |
| 事件循环/状态机架构范式 | **llama-swap**（Go）的Process/Scheduler/Swapper三层拆分+单写者事件循环+"决策必须是纯函数"的教训（对应真实事故issue #946）；`litellm-rust`的`Machine`/`Host` trait抽象是同一理念的另一Rust实现范例 | 仅设计范式借鉴 |

### 5.2 需要额外关注/后续跟进的候选

1. **agentgateway**（Linux Foundation，Rust为主，5059 star，当天仍有提交，明确带"budget and spend controls"）——本次因时间关系未列入Top 3深读，但活跃度和star数均超过plano之外的所有候选，**强烈建议后续单独立项做一轮clone+源码深读**，重点确认其预算控制的具体实现机制（是否SQLite/是否reserve-settle两阶段）。这可能是本次调研的一个重要遗漏，如果深读后发现其架构比plano更贴合"单二进制"，需要重新调整建议排序。
2. **LiteLLM的`litellm-rust`子目录**——虽然当前不可用（PyO3嵌入Python宿主，非独立部署），但其**几乎与iDoris设想完全一致的crate切分方式**（router/auth/cache/cost/gateway/token-counter独立成crate）是强烈的方向性验证信号，且开发速度极快（几乎每天提交）。建议**每季度检查一次**该目录是否演化出独立于Python宿主的部署形态，一旦成熟可能是比现在任何候选都更合适的vendor对象（同一个团队做的功能最全的虚拟key/预算/路由系统的Rust原生实现）。

### 5.3 风险提示

- **许可证风险**：llamastash的README附加"禁军事用途"条款不在LICENSE文件内，若要复用其代码（即使是重写后的衍生实现），建议要么联系作者确认该条款不构成许可证的一部分，要么彻底自己重写不参照其具体代码行，只用其思路。
- **Arch-Router模型权重的商用限制**：若考虑直接使用`katanemo/Arch-Router-1.5B`权重做意图分类器，商用需向DigitalOcean申请单独授权；更干净路径是照论文方法论自训模型。
- **plano护栏功能是空壳**：不要被plano的营销材料（暗示内置Arch-Guard分类器）误导去做架构决策——PII/越狱护栏在当前主分支代码里没有生效实现，这部分iDoris必须完全自研或从其他项目（如aisix的`pii.rs`或semantic-router的方法论）借鉴。
- **aisix的功能"看起来"最完整，但预算/本地运行时管理/隐私路由三大iDoris刚需完全缺失**——评估时不要被其"功能覆盖面最完整"的表面印象带偏，深读已确认这三块开源版里是彻底不存在（不是阉割了一部分，是压根没有本地实现）。
- **TensorZero的3.5个月无推送**、**millwright的7周停滞+单人维护**、**paddock的可信度存疑**——这三个项目在做最终决策前，如果仍在候选名单里，建议对其近况做一次时效性复核（本报告数据截至2026-09-27）。
- **迁移工作量总体评估**：按上表分层借鉴的方式，粗略估算核心网关骨架（provider转换+路由决策链+限流预算雏形+基础guardrail+Admin API只读部分）用1-2名熟悉Rust的工程师、参照本报告列出的具体文件，大约**4-8周**可以搭出可用原型；本地运行时管理层（对接oMLX/mlx_lm.server/llama.cpp三种后端+llamastash式的状态机/探针/驱逐/单飞）预计**2-3周**（llamastash的设计已经把大部分踩坑经验显式记录，包括mlx_lm.server没有`/health`端点这类协议细节）；意图判定小模型的训练/微调是独立的机器学习工作量，不计入工程周期估算。以上为**粗略估算，未做详细任务分解，仅供决策参考**。

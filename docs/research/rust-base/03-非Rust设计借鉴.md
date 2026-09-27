# iDoris 借鉴调研：非 Rust 开源模型网关 / 路由 / 模型管理项目与论文

> 调研目的：为 iDoris（本地优先、OpenAI 兼容、隐私→预算→意图→容量→降级 确定性准入路由、虚拟 key/租户隔离、reserve/settle 预算、元数据审计、ATIF 轨迹 + System-1 学习、Admin API 提议→diff→批准→版本化）寻找**设计借鉴**，不复用代码。
> 调研方式：`gh api` 拉取仓库元数据（语言/许可证/star），WebSearch 拉取架构文档、博客、论文，对关键项目（Ollama、llama-swap、LiteLLM、RouteLLM）做了 shallow clone 读源码。clone 产物在
> `/private/tmp/claude-502/-Users-jason-Dev-auraai-iDoris/a51f0c0c-e3d9-4797-9328-742c114b4f98/scratchpad/oss-ideas/`。
> 凡未能核实的地方标注「未核实」。日期：2026-09-27。

---

## 一、对比表（按「思路匹配度」排序）

匹配度定义：与 iDoris 的分层准入（隐私→预算→意图→容量→降级）、虚拟 key/租户隔离、reserve/settle 预算、Admin API 提议化、System-1 学习闭环等设计目标的重合程度。★★★=可直接抄设计（不抄代码）；★★=部分机制可借鉴；★=仅作参照/反面案例。

| 项目 | 类别 | 语言 | 许可证 | 匹配度 | 一句话定位 |
|---|---|---|---|---|---|
| [llama-swap](https://github.com/mostlygeek/llama-swap) | 模型管理 | Go | MIT | ★★★ | 单 goroutine 事件循环 + 可插拔 Swapper 驱逐策略，是本地多运行时"准入即状态机"的最佳参照 |
| [Ollama `server/sched.go`](https://github.com/ollama/ollama/blob/main/server/sched.go) | 模型管理 | Go | MIT | ★★★ | 生产级 VRAM 估算 + LRU 驱逐 + OOM 重试，踩过的坑最系统 |
| [LiteLLM](https://github.com/BerriAI/litellm) | 网关 | Python（新版本 proxy 核心已转 Rust，见下文注） | MIT（`enterprise/` 目录例外） | ★★★ | 虚拟 key / 预算 / 路由策略最全，且公开了大量真实的预算竞态 issue |
| [vLLM Semantic Router](https://github.com/vllm-project/semantic-router) | 路由+隐私 | Go 控制面 + Rust 推理核心（见下文注） | Apache-2.0 | ★★★ | 隐私(PII/越狱)→意图→路由 的分类器流水线顺序与 iDoris 的准入链高度同构 |
| [llm-d Router (原 llm-d-inference-scheduler)](https://github.com/llm-d/llm-d-router) | 路由 | Go | Apache-2.0 | ★★★ | Filter→Score→Pick 的可插拔 SchedulerProfile，是"容量"层插件化的范本 |
| [RouteLLM](https://github.com/lm-sys/RouteLLM) / [论文](https://arxiv.org/abs/2406.18665) | 路由 | Python | Apache-2.0 | ★★★ | 路由模型训练方法论（矩阵分解/BERT/因果LLM）直接对应 iDoris 的 System-1 训练目标 |
| [Microsoft Presidio](https://github.com/microsoft/presidio)（现 `data-privacy-stack/presidio`，见下文注） | 隐私 | Python | MIT | ★★★ | Analyzer/Anonymizer 分离 + 可插拔 Recognizer，是隐私准入引擎的标准形态 |
| [FastChat Controller](https://github.com/lm-sys/FastChat/blob/main/fastchat/serve/controller.py) | 模型管理/容量 | Python | Apache-2.0 | ★★★ | 心跳注册 + 队列长度感知调度，是"容量"层最小可行设计 |
| [NeMo Guardrails](https://github.com/NVIDIA-NeMo/Guardrails) | 隐私/护栏 | Python | Apache-2.0 | ★★☆ | 事件驱动 rails 流水线，可类比准入链的顺序执行与短路 |
| [Langfuse](https://github.com/langfuse/langfuse) | 审计观测 | TypeScript | MIT（`ee/` 目录例外） | ★★☆ | 批量落 S3 + 异步 worker 入 ClickHouse 的审计写入路径，元数据与原始内容分层存储的参照 |
| [Portkey Gateway](https://github.com/Portkey-AI/gateway) | 网关 | TypeScript | MIT | ★★☆ | Config 即路由规则（fallback/retry/guardrail hook）的声明式设计 |
| [Bifrost](https://github.com/maximhq/bifrost) | 网关 | Go | Apache-2.0 | ★★☆ | 插件架构（Go/WASM）+ 虚拟key/团队/客户三级预算层级 |
| [GPUStack](https://github.com/gpustack/gpustack) | 模型管理 | Python | Apache-2.0 | ★★☆ | Resource Fit Policy 的候选优先级排序，容量层可参照 |
| [Not Diamond](https://www.notdiamond.ai/)（[公开文档](https://docs.notdiamond.ai/)） | 路由 | 闭源（Python/TS 客户端 SDK 公开） | 客户端 SDK 未标注许可证（未核实） | ★★☆ | "meta-model 学习何时用哪个模型"与 iDoris System-1 目标一致，但训练管线闭源 |
| [Kong AI Gateway (ai-proxy-advanced)](https://developer.konghq.com/plugins/ai-proxy-advanced/) | 网关 | Lua（Kong 核心） | Apache-2.0 | ★★☆ | 语义负载均衡 + 6 种可切换路由算法的策略枚举方式 |
| [Apache APISIX ai-proxy(-multi)](https://apisix.apache.org/docs/apisix/plugins/ai-proxy/) | 网关 | Lua | Apache-2.0 | ★★☆ | 网关层按 token 使用量做 access log 的最小可行观测 |
| [KubeAI](https://github.com/substratusai/kubeai) | 模型管理 | Go | Apache-2.0 | ★★☆ | scale-from-zero + 前缀感知负载均衡，不依赖外部 Istio/Knative 的最小依赖哲学 |
| [OpenLLMetry](https://github.com/traceloop/openllmetry) | 审计观测 | Python | Apache-2.0 | ★★☆ | GenAI 专用 OTel 语义约定，ATIF 轨迹字段设计可参照其 span 属性命名 |
| [Helicone](https://github.com/Helicone/helicone) | 审计观测 | TypeScript | Apache-2.0 | ★★☆ | 先返回响应、后异步写 Kafka 的日志路径，保证审计不拖慢主链路 |
| [Xinference](https://github.com/xorbitsai/inference) | 模型管理 | Python | Apache-2.0 | ★★☆ | Actor 模型（Supervisor/Worker/Model Actor）做多运行时生命周期管理 |
| [LLM Guard](https://github.com/protectai/llm-guard) | 隐私/护栏 | Python | MIT | ★★☆ | input/output 双面 Scanner 各自独立可插拔，`.scan()` 统一返回 `(sanitized, is_valid, risk_score)` 契约 |
| [GPTCache](https://github.com/zilliztech/GPTCache) | 预算（缓存降本） | Python | MIT | ★★☆ | 语义缓存 + TTL/LRU 双重驱逐，预算层可作为"reserve 之前的降本前置层" |
| [LocalAI](https://github.com/mudler/LocalAI) | 模型管理 | Go | MIT | ★☆ | 按需拉取 backend + OCI 签名校验，是"确定性准入"里供应链完整性的参照 |
| [Arch-Router 论文](https://arxiv.org/abs/2506.16655) / [模型](https://huggingface.co/katanemo/Arch-Router-1.5B) | 路由 | 论文+模型权重（配套代理 [katanemo/plano](https://github.com/katanemo/plano) 是 Rust，见下文注） | Apache-2.0 | ★★☆ | 偏好对齐路由（域+动作映射）不需要重训即可加新模型，直接对应 iDoris 的意图分类需求 |
| [NVIDIA LLM Router](https://github.com/NVIDIA-AI-Blueprints/llm-router) | 路由 | Router Server 用 Triton；Router Controller 是 Rust（见下文注） | Apache-2.0 | ★★☆ | 分类器与代理分离（Router Server vs Router Controller）的两段式路由 |
| [Envoy AI Gateway → Agent Router](https://github.com/theagentrouter/agent-router)（原 `envoyproxy/ai-gateway`） | 网关 | Go | Apache-2.0 | ★☆ | CRD 化的 K8s 原生网关设计，路由策略与 iDoris 单机场景差异大，仅供参照配置模型 |
| [kgateway](https://github.com/kgateway-dev/kgateway) | 网关 | Go | Apache-2.0 | ★☆ | K8s Gateway API 控制面，供架构对照 |
| [Higress](https://github.com/higress-group/higress) | 网关 | Go | Apache-2.0 | ★☆ | Token 级限流 + per-provider token 熔断（连续异常暂停某 token） |
| [Martian (withmartian)](https://blog.withmartian.com/post/mission) | 路由 | 闭源（"Model Mapping" 无公开代码；`martianprotocol/martianrouter` 疑似同名无关项目，未核实） | 未核实 | ★☆ | "把模型当员工分派任务"的产品叙事，无法获取工程细节 |
| [OpenRouter](https://openrouter.ai/) | 网关 | 闭源 | 不适用 | ★☆ | 公开的路由策略文档（30秒无故障优先 + 价格倒数平方加权）值得参照，但无源码 |
| [LM Studio](https://lmstudio.ai/) | 模型管理 | 闭源（部分周边如 `lms` CLI、mlx-engine 开源） | 不适用 | ★☆ | JIT 加载 + Idle TTL + Auto-Evict 的产品化命名和默认值参照 |
| [vLLM production-stack](https://github.com/vllm-project/production-stack) | 路由 | Python | Apache-2.0 | ★☆ | KV-cache-aware / load-aware 路由，面向多实例集群，iDoris 单机场景暂不适用但思路可类比"容量" |
| [Ray Serve LLM](https://docs.ray.io/en/latest/serve/llm/index.html) | 路由 | Python | Apache-2.0 | ★☆ | `PrefixCacheAffinityRouter` 用近似基数树做跨路由实例的前缀亲和，集群场景 |
| [agentgateway](https://github.com/agentgateway/agentgateway) | 网关 | **Rust**（用户已列出，仅作架构参照，不计入"非 Rust"清单） | Apache-2.0 | ★☆ | Agent/MCP 流量与 LLM 流量统一到一个数据面，架构参照 |
| [exo](https://github.com/exo-explore/exo) | 模型管理 | Python | Apache-2.0 | ★☆ | P2P 无主从 + 环形内存加权分片，面向分布式推理，与 iDoris 单机多运行时目标不同赛道 |

---

## 二、逐项详细分析

### A. 网关（Gateway）

#### 1. LiteLLM
- **许可证**：MIT（`enterprise/` 目录下为独立商业许可）——[Issue #34241 讨论许可证边界](https://github.com/BerriAI/litellm/issues/34241)。
- **⚠️ 语言变化**：仓库当前 GitHub 简介自称 "Rust core with Python SDK"（`gh api repos/BerriAI/litellm` 返回该描述），说明 LiteLLM 正在把 proxy 核心迁往 Rust 以提升性能，Python 层作为 SDK/胶水层保留。列入本次调研是因为用户明确点名，且其虚拟 key/预算模块的**设计**（而非实现语言）仍是主要参照对象。
- **核心设计**：
  - Virtual Keys：spend 自动记在 `LiteLLM_VerificationTokenTable`，并按 `user_id`/`team_id` 级联记到 `UserTable`/`TeamTable`（[Virtual Keys 文档](https://docs.litellm.ai/docs/proxy/virtual_keys)）。
  - Router 支持 `provider_budget_config`（按 provider 设 `budget_limit` + `time_period`）（[Budget Routing 文档](https://docs.litellm.ai/docs/proxy/provider_budget_routing)）。
  - Tag 级预算：请求可打 tag（metadata 或 `x-litellm-tags` header），虚拟 key 也可绑定 tag 继承预算（[Tag Budgets 文档](https://docs.litellm.ai/docs/proxy/tag_budgets)）。
- **iDoris 可借鉴的具体机制**：
  - 预算维度分层（key/user/team/tag/provider）而非单一维度，对应 iDoris 的租户隔离 + 虚拟 key 设计，可直接映射到 iDoris 的 `budget` 表按 tenant/key/provider 多维聚合。
  - `provider_budget_routing` 把预算作为路由过滤条件之一（预算耗尽的 provider 直接被路由排除），与 iDoris "预算→意图→容量" 的准入顺序一致，可参照其把预算判断前置到路由候选集过滤，而不是事后拒绝。
- **踩过的坑**（对 iDoris 的 reserve/settle 预算设计极具参考价值，全部来自真实 issue）：
  - **预算重置竞态**：Redis `SET` 超时后无条件重试，重试期间另一个请求已用 `INCR` 预留了 spend，重试把这个值覆盖成 `new_spend`，预留丢失——攻击者可在 Redis 短暂故障 + 可预测的重置窗口内超支（[Issue #32614](https://github.com/BerriAI/litellm/issues/32614)，修复 [PR #32618](https://github.com/BerriAI/litellm/pull/32618)、[PR #32624](https://github.com/BerriAI/litellm/pull/32624)）。**对应 iDoris 的启示**：reserve/settle 一定要用"先读后写"的乐观锁或 Lua 原子脚本，settle 阶段绝不能用无条件覆盖式写回。
  - **内存缓存被过期 Redis 值覆盖**：Redis increment pipeline 尚未完成，同步读取就已发生，内存 spend 从 160 被覆盖回 100（同 Issue #32614 的分析，修复见 [PR #32844](https://github.com/BerriAI/litellm/pull/32844)：await Redis flush 后再读）。
  - **协调 Redis 启动期探测竞态**：`ProxyConfig._init_coordination_redis_env_fallback` 只在启动时 ping 一次 Redis（2 秒超时），失败后该 pod 终生不参与跨 pod 协调（spend counter、预算执行、鉴权缓存失效全部退化为单机内存），直到重启（[Issue #42653](https://github.com/BerriAI/litellm/issues/42653)，修复 [PR #42667](https://github.com/BerriAI/litellm/pull/42667) 改为持续重探测）。**对应 iDoris 的启示**：iDoris 若引入分布式协调（多进程/多租户共享预算状态），启动期单次探测是反面教材，需要健康检查带重试与降级标记。
- **与 iDoris 的思路差异**：LiteLLM 是"先做功能全、再打补丁修竞态"的演进路径，预算竞态是长期反复出现的问题类别（多个 PR 反复修复同一根因）；iDoris 从设计之初就把 reserve/settle 作为一等公民，且明确"未知价格拒绝"，比 LiteLLM 的"默认放行、事后修 bug"更保守，这是优势，应保持。

#### 2. Portkey Gateway
- **许可证**：MIT。**语言**：TypeScript。
- **核心设计**：Config 对象承载路由规则（fallback 策略、带自定义状态码的重试、before/after request hook）；Virtual Keys 把 provider 凭证与鉴权解耦；50+ guardrail 通过 hook 接入（[AI Gateway 文档](https://portkey.ai/docs/product/ai-gateway)、[Guardrails 文档](https://docs1.portkey.ai/docs/product/guardrails)）。
- **可借鉴机制**：声明式 Config（而非命令式代码）表达路由拓扑，与 iDoris Admin API 的"版本化配置"目标一致——可以借鉴其 Config 作为不可变版本化对象、通过 `x-portkey-config` 引用的模式，映射到 iDoris "提议→diff→批准→版本化"里的版本对象设计。
- **踩过的坑**：
  - **SSRF 安全公告**：Custom Host 存在服务端请求伪造漏洞（[GHSA-hhh5-2cvx-vmfp](https://github.com/Portkey-AI/gateway/security)，2025-12-01 发布）。对 iDoris 的启示：Admin API 若允许自定义 upstream/host 字段，必须做出站地址白名单校验。
  - **Virtual Key 与显式 header 的耦合 bug**：自托管网关在只传 `virtual_key` 时仍强制要求 `x-portkey-config` 或 `x-portkey-provider`，与虚拟 key 应"自包含全部路由配置"的预期矛盾（[Issue #1190](https://github.com/Portkey-AI/gateway/issues/1190)）。对 iDoris 的启示：虚拟 key 的语义边界（它到底代表"凭证"还是"凭证+路由策略"）要在设计阶段写清楚，否则后续会出现类似的"到底谁该生效"的歧义 bug。
- **与 iDoris 的思路差异**：Portkey 的准入顺序由 Config 里 hook 的书写顺序决定（隐式），iDoris 是硬编码的确定性顺序（隐私→预算→意图→容量→降级），后者更适合需要审计可解释性的场景。

#### 3. Bifrost（Maxim AI）
- **许可证**：Apache-2.0。**语言**：Go。
- **核心设计**：插件架构（Go 原生插件或 WASM），语义缓存、遥测、mock 请求器均以插件形式实现；治理层支持虚拟 key/团队/客户三级预算与限流层级；CEL 表达式统一描述 prompt/response/MCP 工具参数的护栏规则（[Bifrost 治理文章](https://www.getmaxim.ai/articles/deploying-ai-governance-for-enterprises-with-bifrost-edge-bifrost-gateway/)）。
- **可借鉴机制**：CEL（Common Expression Language）作为护栏规则的统一 DSL，可以给 iDoris 的 Admin API "提议"对象里的策略字段提供一种成熟、可静态校验、无副作用的表达式语言选型参照，而不是自造 mini-DSL。
- **踩过的坑**：未核实（未搜到 Bifrost 相关的公开 postmortem 或安全公告；项目相对新，2024 年后才活跃）。
- **与 iDoris 的思路差异**：Bifrost 强调"多租户企业治理"，预算层级是三级固定（key/team/customer），iDoris 的预算模型更细粒度（reserve/settle + 未知价格拒绝），后者对本地/中小组织场景的成本可控性要求更高。

#### 4. Envoy AI Gateway → Agent Router
- **许可证**：Apache-2.0。**语言**：Go。
- **⚠️ 重要变化**：2026-09-10 该项目更名为 **Agent Router**，迁移到 Agentic AI Foundation 并搬到新仓库 [`theagentrouter/agent-router`](https://github.com/theagentrouter/agent-router)，旧的 `envoyproxy/ai-gateway` 链接会重定向。CRD（`AIGatewayRoute`、`AIServiceBackend`、`BackendSecurityPolicy`）、API group（`aigateway.envoyproxy.io`）、CLI（`aigw`）、镜像与 Go module 路径保持不变（[vllm-project/semantic-router Issue #4126 讨论此次更名](https://github.com/vllm-project/semantic-router/issues/4126)）。
- **核心设计**：基于 Envoy Gateway 的 K8s CRD 扩展，`AIGatewayRoute` 描述模型级路由，`BackendSecurityPolicy` 描述凭证策略。
- **可借鉴机制**：CRD 化的"资源即配置"思路，如果 iDoris 未来需要更结构化的 Admin API schema，可以参照其 CRD 字段拆分方式（Route / Backend / SecurityPolicy 三种资源分离，而不是一个大 JSON）。
- **踩过的坑**：未核实（作为 K8s 生态项目，多数 issue 与集群编排相关，与 iDoris 单机场景关联度低，未深入挖掘）。
- **与 iDoris 的思路差异**：面向 K8s 多副本网关，路由决策依赖控制面下发配置到数据面（Envoy xDS），与 iDoris 单进程内做确定性路由的模型完全不同，仅供架构对照，不建议照搬控制面/数据面分离的复杂度。

#### 5. kgateway / agentgateway
- **kgateway**：Apache-2.0，Go，CNCF sandbox 项目（原 Gloo Edge），K8s Gateway API 控制面（[kgateway AI Gateway 文档](https://kgateway.dev/docs/envoy/latest/ai/)）。
- **agentgateway**：Apache-2.0，**Rust**（用户已在任务中列出该项目，但其数据面用 Rust 实现；本报告仍简要记录其设计，不计入"非 Rust"结论，仅供架构参照）。统一处理 Agent/MCP/LLM 流量到一个数据面，支持预算与花费控制、prompt 富化、负载均衡与故障转移（[agentgateway LLM consumption 文档](https://agentgateway.dev/docs/standalone/main/llm/)）。2025 年捐赠给 Linux Foundation，2026 年被 Agentic AI Foundation 接纳。
- **可借鉴机制**：kgateway 与 agentgateway 是"控制面/数据面分离"的范例——kgateway 管配置生命周期，agentgateway 管高性能转发。iDoris 目前是单体，但如果未来要拆分"Admin API 控制面"与"网关数据面"，这是一个现成的分层参照。
- **踩过的坑**：未核实。
- **与 iDoris 的思路差异**：面向多 Agent/MCP 协议网关，比 iDoris 的 OpenAI 兼容网关范围更广，复杂度也更高，不建议直接对标。

#### 6. Kong AI Gateway（ai-proxy-advanced 插件）
- **许可证**：Apache-2.0（Kong 核心）。**语言**：Lua。
- **核心设计**：`ai-proxy-advanced` 插件支持 6 种负载均衡算法可切换：round-robin、consistent-hashing、least-connections、lowest-latency、lowest-usage（按 token/成本）、semantic（按 prompt 与模型描述的向量相似度）、priority（分级故障转移）（[AI Proxy Advanced 文档](https://developer.konghq.com/plugins/ai-proxy-advanced/)、[语义负载均衡 how-to](https://developer.konghq.com/how-to/use-semantic-load-balancing/)）。
- **可借鉴机制**：把"路由算法"做成一个可枚举、可运行时切换的策略列表（而不是散落的 if/else），这正是 iDoris "意图→容量"路由层可以直接照搬的接口设计模式——一个 `Router` 接口 + 多个具名策略实现。
- **踩过的坑**：**AI Gateway 早期版本里，某一个 provider 触发限流会连带阻塞对其它 provider 的请求**，Kong 3.14 通过"按模型限流"重新设计解决（[Issue #3949](https://github.com/Kong/developer.konghq.com/issues/3949)）。对 iDoris 的启示：预算/限流的作用域必须精确到 (tenant, provider, model) 三元组，绝不能让一个维度的耗尽误伤其它维度——这与 iDoris "budget 按 reserve/settle 结算"的多维设计理念一致，是一个很好的反面教材佐证。
- **与 iDoris 的思路差异**：Kong 是通用 API 网关叠加 AI 插件，AI 特化程度不如 iDoris 深（例如没有 local_only fail-closed 的语义）。

#### 7. Apache APISIX ai-proxy / ai-proxy-multi
- **许可证**：Apache-2.0。**语言**：Lua。
- **核心设计**：`ai-proxy-multi` 支持按权重分流（如 80/20）+ 重试/故障转移/健康检查，并在 access log 记录 token 用量、模型、首字延迟等字段（[ai-proxy-multi 文档](https://apisix.apache.org/docs/apisix/next/plugins/ai-proxy-multi/)）。
- **可借鉴机制**：把 token 用量、延迟写入标准 access log 而不是自定义审计表，是一种"零额外基础设施"的轻量审计方式，可以作为 iDoris 审计层"元数据落盘"格式的补充参照（结构化字段 + 标准日志管道）。
- **踩过的坑**：未核实。
- **与 iDoris 的思路差异**：APISIX 的路由权重是静态配置的百分比分流，不是 iDoris 那种基于实时预算/容量状态的动态决策。

#### 8. OpenRouter（公开设计，闭源）
- **许可证**：不适用（无公开源码）。
- **核心设计**：默认路由策略——先剔除"过去 30 秒内有明显故障"的 provider，再按价格倒数平方加权，从稳定 provider 里选低价候选，其余作为 fallback（[How OpenRouter Model Routing Works](https://openrouter.ai/blog/insights/model-routing/)）。仅对成功完成的请求计费，失败/fallback 不计费（[Provider Routing 文档](https://openrouter.ai/docs/guides/routing/provider-selection)）。
- **可借鉴机制**：
  1. "近期故障窗口"作为路由候选过滤的第一步（先淘汰不健康的候选，再谈价格/质量），与 iDoris"容量"层的降级判断顺序一致。
  2. "只对成功计费"的计费口径，直接对应 iDoris 的 settle 语义——reserve 阶段预留额度，只有真正成功返回才 settle 扣费，失败要能回滚 reserve。
- **踩过的坑**：**2026 年 2 月 17 日和 19 日两次关联故障**，根因都是第三方缓存层用于 API key 查找失败，导致 20% 请求先失败、13 分钟内恶化到 80-90%，且故障表现为**误导性的 401 鉴权错误**而非真正的基础设施故障（[Requesty 对该事件的分析](https://www.requesty.ai/blog/correlated-provider-outage-september-2026)）。OpenRouter 事后声明修复方向包括熔断器和把误导性 401 改为 503。**对应 iDoris 的启示**：鉴权/缓存层故障不应该伪装成"用户凭证错误"返回给调用方，iDoris 的虚拟 key 校验失败与依赖服务（如价格表、隐私服务）不可用必须用不同的错误码区分，否则会诱导用户误诊断（这与"未知价格拒绝"的哲学一致：拒绝要说清楚为什么拒绝）。
  - 另有非 postmortem 但值得记录的坑：**中止流式请求不一定停止计费**——Bedrock、Groq、Google、Mistral 等 provider 在客户端取消流后仍会计费（[OpenRouter FAQ](https://openrouter.ai/docs/faq)）。对 iDoris 的启示：reserve/settle 在流式场景下，客户端断连不能自动等同于"零消耗"，需要向 provider 侧确认真实计费口径后再 settle 或退款，这是一个容易被忽视的边界情况。
- **与 iDoris 的思路差异**：OpenRouter 是纯 SaaS 网关，没有 local_only/隐私 fail-closed 的概念，其"路由"目标是全局最优（价格+可用性），iDoris 是本地优先，路由目标里"隐私"具有否决权，优先级完全不同。

---

### B. 路由（Routing）

#### 9. RouteLLM
- **许可证**：Apache-2.0。**语言**：Python。**论文**：[RouteLLM: Learning to Route LLMs with Preference Data](https://arxiv.org/abs/2406.18665)（Ong et al., 2024）。
- **核心设计**：4 种路由器架构对比——相似度加权排序（SW）、矩阵分解、BERT、因果 LLM（基于 Llama3-8B）；用 Chatbot Arena 的人类偏好标注 + LLM-judge 生成的成对比较标签做数据增强训练；矩阵分解路由器能用 26% 的 GPT-4 调用达到 95% 的 GPT-4 效果（约省 48% 成本）（[LMSYS 博客](https://www.lmsys.org/blog/2024-07-01-routellm/)）。
- **可借鉴的具体机制（源码级）**：仓库结构 `routellm/routers/` 下每种路由策略是独立模块（matrix factorization / similarity-weighted / BERT / causal LLM），对外暴露统一的 `route(prompt) -> model` 接口——这正是 iDoris "System-1 小路由/判定模型"应该采用的模块化训练+推理接口形态：训练脚本、模型权重、在线推理路径三者解耦，方便迭代替换路由算法而不改上层准入链。
- **踩过的坑**：论文本身指出人类偏好标签稀缺是训练路由器的主要瓶颈，因此需要用 LLM-as-judge 做数据增强；这对应 iDoris 用 ATIF 轨迹配合反馈回路训练 System-1 时，也需要考虑"真实反馈稀疏，需要用规则/强模型 judge 做半自动标注"的问题，不能假设人工标注数据量充足。
- **与 iDoris 的思路差异**：RouteLLM 只做"简单模型 vs 强模型"的二元/多元质量-成本路由，不涉及隐私、预算 reserve/settle、租户隔离，是 iDoris 意图路由层的一个子问题（质量/成本决策）而不是全链路方案。

#### 10. Arch-Router
- **许可证**：论文与模型权重 Apache-2.0（[HuggingFace 模型卡](https://huggingface.co/katanemo/Arch-Router-1.5B)）。配套代理项目已更名为 [katanemo/plano](https://github.com/katanemo/plano)（**Rust**，仅供参照，不计入非 Rust 结论）。
- **核心设计**：1.5B 小模型学习"query → 领域(domain)+动作(action) 偏好"的映射，而不是直接学习"query → 具体模型"，新增模型时只需要在偏好映射表里加一行，**不需要重新训练路由模型**（[论文](https://arxiv.org/abs/2506.16655)）。
- **可借鉴的具体机制**：这个"路由目标是偏好类别而不是具体模型 ID"的解耦设计，正好是 iDoris System-1 应该采用的思路——训练一个"识别意图类别"的小模型，类别到具体 runtime/模型的映射放在可热更新的配置表（Admin API 版本化对象）里，这样意图分类器不需要因为新增一个 mlx_lm 模型或调整降级策略而重新训练。
- **踩过的坑**：未核实（论文发布较新，暂未搜到公开生产事故报告）。
- **与 iDoris 的思路差异**：Arch-Router 只解决"选哪个模型服务质量最好"，不涉及预算/容量/隐私联合决策，且默认多模型可用，没有 iDoris 那种"降级"语义（找不到合适模型时怎么办）。

#### 11. NVIDIA LLM Router
- **许可证**：Apache-2.0。**语言**：Router Server 基于 Triton Inference Server；Router Controller 是 **Rust**（[GitHub 仓库](https://github.com/NVIDIA-AI-Blueprints/llm-router) 明确说明"uses Rust and NVIDIA Triton"）——仅作架构参照。
- **核心设计**：两段式路由——Router Controller（类代理服务）转发 OpenAI 兼容请求；Router Server（Triton）用预训练的 `prompt-task-and-complexity-classifier` 对 prompt 分类，再依据策略把请求发到不同下游 LLM（[NVIDIA 技术博客](https://developer.nvidia.com/blog/deploying-the-nvidia-ai-blueprint-for-cost-efficient-llm-routing/)）。
- **可借鉴机制**：分类模型与转发代理彻底解耦成两个独立服务（Controller 只做转发决策的执行，Server 只做分类推理），意味着分类模型可以独立升级/回滚/A-B 测试而不影响转发路径的稳定性——这与 iDoris "System-1 判定模型" 应该作为一个可独立部署、可灰度、可回滚的子服务而不是耦合进准入链主进程的设计思路一致。
- **踩过的坑**：未核实。
- **与 iDoris 的思路差异**：NVIDIA 方案默认部署在有 GPU 集群的企业环境（Triton），iDoris 面向 macOS 单机个人/小团队场景，System-1 判定模型必须足够小、能在 CPU/统一内存上低延迟推理，不能依赖 Triton 这类重型推理服务。

#### 12. Martian（withmartian）
- **许可证**：未核实（无公开源码；`martianprotocol/martianrouter` 疑似不相关的区块链项目，同名巧合，未核实是否同一团队）。
- **核心设计（仅公开叙事）**："Model Mapping" ——号称用大规模 AI 可解释性技术理解模型内部行为来预测其在具体 query 上的表现，在 OpenAI evals 上号称以更低成本达到不低于 GPT-4 的效果（[Martian 使命博客](https://blog.withmartian.com/post/mission)）。
- **可借鉴机制**：产品叙事层面的"把模型当员工看待，不同任务分派给不同专长的模型"是一个很好的向非技术用户解释路由概念的比喻，可用于 iDoris 的用户文档/Admin UI 文案，但**无法获取任何工程实现细节**。
- **踩过的坑**：未核实。
- **与 iDoris 的思路差异**：完全闭源，商业化程度高，不构成设计参照，仅列出以完成任务要求的核实动作。

#### 13. Not Diamond
- **许可证**：客户端 SDK（`notdiamond-python`、`notdiamond-node`）未标注许可证（GitHub API 返回 `license: null`），未核实是否开源；路由训练管线本身闭源。
- **核心设计**：把路由问题定义为"meta-model"训练——喂给它 prompt、候选模型的回答、评测分数，它学习"每种 query 类型该用哪个模型成本最低"；支持 `tradeoff="cost"` / `"latency"` 参数在质量优先之外做权衡（[What is Model Routing 文档](https://docs.notdiamond.ai/docs/what-is-model-routing)、[自定义路由器训练文档](https://docs.notdiamond.ai/docs/router-training-quickstart)）。
- **可借鉴机制**：`tradeoff` 参数把"路由目标函数"暴露为一个可配置的一等公民（质量 vs 成本 vs 延迟），而不是硬编码单一目标，这个接口设计可以直接映射到 iDoris Admin API 里"路由策略提议"对象的字段——允许管理员在不同租户/场景下声明不同的优化目标。
- **踩过的坑**：未核实（闭源，无法获取公开 issue/postmortem）。
- **与 iDoris 的思路差异**：Not Diamond 面向多租户 SaaS，路由训练依赖其云端评测数据集；iDoris 需要在本地用有限的 ATIF 轨迹自训练，数据量级和训练闭环的自动化程度完全不同。

#### 14. vLLM Semantic Router
- **许可证**：Apache-2.0。**语言**：Go（Envoy ExternalProcessor 控制逻辑）+ **Rust**（推理核心用 HuggingFace Candle 框架做分类器推理，用于低延迟）——用户已列出该项目，此处如实记录其混合语言事实。
- **核心设计**：基于 Envoy Proxy 的 External Processor（ext-proc）架构；8 个基于 mmBERT-32K 的神经分类器覆盖意图分类、越狱检测、PII 检测、事实核查、幻觉检测等（[系统架构文档](https://vllm-semantic-router.com/docs/overview/architecture/system-architecture/)）；支持"推理模式"开关——根据请求复杂度动态决定是否启用模型的 reasoning 参数（[Red Hat 技术博客](https://developers.redhat.com/articles/2025/09/11/vllm-semantic-router-improving-efficiency-ai-reasoning)）。
- **iDoris 可借鉴的具体机制**：
  - **分类器流水线顺序**——越狱检测 → PII 检测 → 意图/领域分类 → 路由决策，这与 iDoris "隐私 → 预算 → 意图 → 容量 → 降级" 的准入顺序高度同构：都是把"会导致直接拒绝/脱敏"的安全类判断放在最前面，把"影响选哪个模型"的语义类判断放在后面。可以直接参照其[PII Detection 文档](https://vllm-sr.ai/docs/v0.2/tutorials/content-safety/pii-detection/)和[越狱防护文档](https://vllm-sr.ai/docs/v0.1/tutorials/content-safety/jailbreak-protection/)里对"检测到即拒绝/脱敏，不进入下一环"的短路设计。
  - Domain-based routing（按领域路由，[文档](https://vllm-sr.ai/docs/v0.2/tutorials/intelligent-route/domain-routing/)）与 Arch-Router 的偏好类别思路相似，可与 #10 一起作为 System-1 意图分类的双参照。
- **踩过的坑**：
  - **PII 检测在不同实体类型上不一致**：E2E 测试显示 `US_SSN` 能正确拦截（HTTP 403），但 `EMAIL_ADDRESS` 完全没被检测到（HTTP 200，无告警 header）——同一个检测框架下不同实体类型的召回率差异巨大（[Issue #712](https://github.com/vllm-project/semantic-router/issues/712)）。**对应 iDoris 的启示**：iDoris 的隐私准入层如果依赖多个实体类型的检测器，必须对每种实体类型单独做基准测试和阈值调优，不能假设"PII 检测"是一个整体能力，一次验证覆盖所有实体类型。
  - **越狱检测把良性请求的"无匹配"错误地当成契约错误处理**（多端点越狱检测在没有命中规则时抛出错误而不是判定为安全放行）（[Issue #4271](https://github.com/vllm-project/semantic-router/issues/4271)）。对应 iDoris 的启示：安全类检测器的"未命中"和"检测失败"必须是两种不同的返回状态，未命中应默认放行（除非策略要求 fail-closed），检测失败（服务异常）才应该走 iDoris "local_only fail-closed" 那一套保守策略。
  - **工具调用结果里的 PII 未被路由感知**：功能请求 [Issue #3560](https://github.com/vllm-project/semantic-router/issues/3560) 指出当前只检测用户输入里的 PII，不检测 Agent 工具调用返回结果里可能携带的 PII，这是一个已知的覆盖盲区。对 iDoris 的启示：如果未来支持工具调用/Agent 场景，隐私准入必须覆盖"工具返回"这个注入点，不能只做入口检测。
- **与 iDoris 的思路差异**：vLLM Semantic Router 是"在网关层用多个专用小模型做流水线式内容安全+路由"，与 iDoris 的"确定性规则准入 + System-1 小模型辅助"设计接近，最大的差异是 iDoris 明确了"隐私 fail-closed"的强约束（local_only 请求宁可拒绝也不能泄露），而 vLLM Semantic Router 目前的行为更偏"检测到就拦截，检测不到就放行"，没有看到"未知情况下默认拒绝"的强约束语义。

#### 15. vLLM production-stack
- **许可证**：Apache-2.0。**语言**：Python。
- **核心设计**：三种路由模式递进——Prefix-aware routing（相同前缀固定路由到同一实例，即使缓存已被驱逐）；KV-cache-aware routing（按 KV 缓存命中率路由，避免命中率与负载的耦合问题）；Load-aware routing（在 KV 命中率之上再叠加实例负载权重，用 `loadaware-beta` 参数调节"偏向缓存命中"还是"偏向空闲实例"）（[Load Aware Routing 文档](https://docs.vllm.ai/projects/production-stack/en/latest/use_cases/loadaware-routing.html)）。
- **可借鉴机制**：Load-aware routing 明确指出了 Prefix-aware/KV-cache-aware 路由的一个真实缺陷——"热实例排队、冷实例空闲"，用一个可调 beta 参数在两个目标间做插值。这个"识别出单一优化目标的副作用，再用可调参数做加权平衡"的方法论，可以用在 iDoris 的"容量"路由层：例如在"优先复用已加载模型的 runtime"和"优先选择空闲 runtime 避免排队"之间做同样的加权。
- **踩过的坑**：文档明确描述了"纯 KV-cache-aware 路由"本身就是一个坑（会把所有匹配同一热门前缀的请求都发到同一个实例，不管它多忙），这是一个已被官方文档记录、后续版本主动修复的设计缺陷案例。
- **与 iDoris 的思路差异**：面向多实例 vLLM 集群的 KV 缓存复用，iDoris 是单机多运行时（oMLX/mlx_lm/llama.cpp），没有跨实例 KV 缓存复用的场景，但"负载 vs 亲和性"的加权思路仍可用于 iDoris 在多个本地 runtime 间做选择时的调度依据。

#### 16. llm-d Router（原 llm-d-inference-scheduler）
- **许可证**：Apache-2.0。**语言**：Go。**⚠️ 仓库名变化**：`llm-d-incubation/llm-d-inference-scheduler` 已不存在，当前实现在 [`llm-d/llm-d-router`](https://github.com/llm-d/llm-d-router)（描述为"llm-d Router: The intelligent entry point for inference requests"）。
- **核心设计**：请求调度遵循 **Filter → Score → Pick** 生命周期，由 EPP（External Processing Pod）承载，多个 `SchedulerProfile` 各自定义一组 filter+score 插件（[Request Scheduler 文档](https://llm-d.ai/docs/architecture/core/router/epp/scheduling)）。默认 profile 用 `prefix-cache-scorer`（权重更高）+ `queue-scorer` 组合打分；filter 阶段先剔除过载/资源不匹配/内存压力的 pod（[llm-d 博客：Intelligent Inference Scheduling](https://llm-d.ai/blog/intelligent-inference-scheduling-with-llm-d)）。
- **iDoris 可借鉴的具体机制**：
  - **Filter→Score→Pick 三段式** 与 iDoris 的"隐私→预算→意图→容量→降级"准入链可以映射为：隐私/预算是**硬过滤**（Filter，不满足直接排除候选），意图是**打分**（Score，影响候选优先级），容量/降级是**选择**（Pick，在过滤+打分后的候选里做最终决定，含"找不到就降级"的兜底路径）。这是一个可直接照搬的三段式抽象，能让 iDoris 的路由代码结构更清晰、可测试性更强（每个 Filter/Scorer 都是纯函数，可单元测试）。
  - `SchedulerProfile` 允许针对不同场景（chatbot vs 批处理）用不同的打分权重组合，可类比 iDoris 未来可能需要"交互式请求"和"批量任务"两种不同的容量调度策略。
- **踩过的坑**：未核实（该仓库刚完成从 `-incubation` 到主仓库的迁移，历史 issue 追溯难度较大，未深入挖掘）。
- **与 iDoris 的思路差异**：面向 K8s InferencePool 的多 pod 调度，iDoris 是单机多进程/多 runtime 调度，没有"跨节点"的维度，但插件化的 Filter/Score 抽象与规模无关，值得直接借鉴到代码结构层面。

---

### C. 模型管理（Model Management）

#### 17. llama-swap（重点，已读源码）
- **许可证**：MIT。**语言**：Go。
- **核心设计**（来自 `internal/router/design.md` 源码阅读，[design.md 全文](https://github.com/mostlygeek/llama-swap/blob/main/internal/router/design.md)）：三个关注点彻底解耦——
  1. **进程机制**（`baseRouter`，`internal/router/base.go`）：拥有 channel、run loop、进程生命周期、关闭清理；
  2. **调度策略**（`scheduler.Scheduler`，`internal/router/scheduler/`，当前唯一实现是 `FIFO`）：拥有队列、在途请求计数、"立即服务/加入现有 swap/排队/发起新 swap"的决策树；
  3. **驱逐策略**（`scheduler.Swapper`，`groupSwapper`/`matrixSwapper`）：给定目标模型和当前运行集合，返回必须停止的模型集合，是**纯函数**，不做任何 I/O 或日志。
  - **单 goroutine run loop，零锁**：`baseRouter.run()` 是唯一的状态修改入口，所有 `Scheduler` 方法只在这个 goroutine 里被调用，因此调度器状态不需要任何互斥锁。
  - **慢操作异步化**：`StartSwap` 立即返回（只是启动一个 `doSwap` goroutine），真正的停止旧进程/启动新进程/等待就绪发生在后台，完成后通过 `SwapDone` 事件重新进入 run loop，保证调度决策永远不被慢 I/O 阻塞。
  - **进程状态"只做参照读，不做决策读"的铁律**：文档明确写道——"advisory reads of process state are fine ..., but a read that gates a mutation is a bug"（决定是否启动/停止必须放到进程自己的单写者 run loop 里，而不是外部读一次快照就做决策，因为读到的状态随时可能过期）。
  - **配置层面**：`ttl`（-1 用全局值，0 禁用自动卸载，正数为秒数）+ `unloadTimeout`（优雅关闭超时，超时强杀）+ `groups`（允许多个模型并发常驻，如聊天模型和 embedding 模型同时在线）（[config.example.yaml](https://github.com/mostlygeek/llama-swap/blob/main/config.example.yaml)）。
- **iDoris 可借鉴的具体机制**：
  1. **事件流入/副作用流出（Effects 接口）模式**：`Scheduler` 只通过 `Effects`（`ModelState`/`StartSwap`/`GrantServe`/`GrantError`/`StopProcesses`）与外部交互，这让调度逻辑可以在没有真实进程、没有 goroutine 的情况下用 fake `Effects` 单元测试（参见 `internal/router/scheduler/fifo_test.go` 里的 `stubPlanner`/`fakeEffects`）。iDoris 的准入链（隐私→预算→意图→容量→降级）也应该采用同样的"决策纯函数 + 副作用通过接口注入"结构，方便对每一层单独写单测而不依赖真实的隐私服务/预算服务/runtime 进程。
  2. **`Swapper` 作为纯函数驱逐策略接口**——`EvictionFor(target, running) -> []evict` 不产生副作用、不记日志（因为它会被"投机性"地反复调用：每个新请求、每次队列 drain 都会调用一次，多数情况下答案是"不需要驱逐"），只有真正提交驱逐（`OnSwapStart`）才记一次日志。这个"查询与提交分离，日志只记提交"的原则，直接适用于 iDoris 的容量/降级判断：路由决策过程可能被反复重新评估（例如排队请求的重新调度），但审计/ATIF 轨迹只应该记录**真正发生**的准入决策，不能对"投机性重新评估"重复记账，否则审计日志会被污染，且会误导 System-1 的训练数据。
  3. **异步 swap 结果通过事件回流**，而不是同步阻塞等待，可用于 iDoris 在"降级"层需要尝试多个 runtime 逐个探测可用性时，避免主准入链被慢启动的 runtime 阻塞。
- **踩过的坑（源码 + issue 追溯到的真实生产事故）**：
  - **Issue #946 竞态导致模型永久卡死**：设计文档明确复盘——一个请求在 TTL 卸载过程中到达，读到进程状态是 `StateStopping`，于是跳过"启动"分支，然后永远等待一个"没人会启动"的进程——这直接卡死了该模型的 swap 槽位，导致该模型之后**所有**请求都挂起（[design.md 中对 Issue #946 的复盘](https://github.com/mostlygeek/llama-swap/blob/main/internal/router/design.md)）。这是本次调研中**最直接命中 iDoris 关注点**的真实案例：模型驱逐（TTL 卸载）与新请求到达之间的竞态可能导致整个准入链卡死，而不只是单次请求失败。**对应 iDoris 的启示**：iDoris 在做"容量"层判断时，绝不能用一次性读取的 runtime 状态快照来决定是否需要等待/重试/降级，必须把这个决策放到 runtime 生命周期状态机自己的单写者位置（llama-swap 用 `process.EnsureReady` 解决——不读状态分支，而是把决策塞进状态机内部）。
  - **`GrantServe` 返回值契约的隐藏坑**：调用方的 `Respond` channel 是无缓冲的，如果调用方已经断连，发送会失败，`trackedServe` 永远不会跑，也就永远不会有 `ServeDoneEvent`——所以只有 `GrantServe` 返回 `true` 才能给 `inFlight` 计数 +1，如果在返回 `false` 时也 +1，会导致该模型的在途计数永远大于零，从此再也无法被驱逐（design.md 原文明确指出这一点是"contract"级别的坑）。对应 iDoris 的启示：iDoris 的 reserve/settle 计数、以及容量层的"在途请求数"计数，都要认真核对"记账动作"与"资源真正被占用"之间的因果顺序，避免同类"计数只增不减"的资源泄漏。
- **与 iDoris 的思路差异**：llama-swap 的驱逐策略（`Swapper`）目前只回答"谁该被停"，没有 iDoris 那种"预算耗尽/隐私不合规"的多因素准入判断；但其"三个关注点分离 + 事件驱动 + 纯函数策略"的整体架构范式，是本次调研里对 iDoris 模型管理层**最值得直接复用设计（而非代码）**的项目。

#### 18. Ollama `server/sched.go`（重点，已读源码）
- **许可证**：MIT。**语言**：Go。
- **核心设计**（源码位置：[`server/sched.go`](https://github.com/ollama/ollama/blob/main/server/sched.go)，1800 行）：
  - `Scheduler` 结构体维护 `loaded map[string]*runnerRef`（已加载模型）+ `activeLoading llm.LlamaServer`（当前正在加载的模型，含由它触发的驱逐过程）——**全局同一时刻只允许一个模型处于"加载中"状态**，但已加载模型可以并行处理请求。
  - `processPending`（约 230 行起）是主调度循环：命中已加载模型且不需要重载则直接复用；未命中且已达 `OLLAMA_MAX_LOADED_MODELS` 上限则调用 `findRunnerToUnload()` 腾位置；否则按当前 GPU/系统信息决定加载策略。
  - `findRunnerToUnload()`（约 1679 行）：按 `ByDurationAndName` 排序（近似 LRU），优先找 `refCount == 0` 的空闲 runner；若没有空闲的，就等待持续时间最短的那个。
  - `waitForVRAMRecovery()`（约 1450 行）：卸载模型后，每 250ms 轮询一次 GPU 空闲显存，直到恢复到预估显存的 75% 或超时（典型收敛时间 0.5–1.5 秒），超时则退回到"用估算值"而不是死等。
  - **OOM 主动重试与全量驱逐**：`expireRunnersForRuntimeOOM`（约 1647 行）在检测到运行时 OOM 错误后，主动把所有已加载模型标记过期以腾出内存；`evictAllAndWait`（约 1605 行）在加载崩溃触发 OOM 重试时驱逐除目标外的全部模型并阻塞等待每一个的卸载信号；`oomRetryAttempted` 标志位防止无限重试循环。
- **iDoris 可借鉴的具体机制**：
  1. **"加载中"全局互斥 + "已加载"并发服务** 的两阶段并发模型——加载/驱逐是串行的关键区（`activeLoading` 只能有一个），但已加载模型间的推理请求完全并行，这个粒度划分可以直接套用到 iDoris 的多运行时管理：模型切换/驱逐是需要严格串行化的"写"操作，而对已就绪 runtime 的路由分发是可并行的"读"操作。
  2. **VRAM 恢复轮询 + 超时退化到估算值**：不是乐观地认为"发了 kill 信号内存就立刻释放"，而是主动轮询验证，同时设置合理超时避免无限等待——这对 iDoris 在 macOS 统一内存架构下管理 oMLX/mlx_lm/llama.cpp 之间的内存腾挪同样适用（统一内存的释放/GC 时机同样不确定）。
  3. **OOM 重试要有熔断（`oomRetryAttempted`）**：主动驱逐全部模型重试一次是合理的自愈手段，但必须有"只重试一次"的标志位防止在持久性故障下陷入"加载失败→驱逐全部→再加载失败→再驱逐"的抖动循环，这对 iDoris 的降级层设计是直接可用的模式：降级重试次数必须有上限，且要区分"瞬时故障重试"和"持久性故障应立即降级/拒绝"。
- **踩过的坑（真实 GitHub issue，非常契合本次调研要求的"模型驱逐导致 OOM"主题）**：
  - **调度器把系统空闲内存误当成 GPU 显存上限**：在 AMD Strix Halo（共享内存架构）上，调度器用"主机可用内存"而不是真实的 VRAM carveout 作为显存预算上限，导致第一个模型加载后调度器就认为只剩几 GiB 可用，从而对第二个模型的加载做出错误的驱逐/拒绝决策，多模型互相驱逐（[Issue #16719](https://github.com/ollama/ollama/issues/16719)）。**对应 iDoris 的启示**：iDoris 在 Apple Silicon 统一内存架构上做容量估算时，同样面临"系统可用内存"与"实际可分配给推理的内存"不是同一个数字的问题（尤其是同时有 oMLX 和 llama.cpp 混跑时），必须显式建模"统一内存 carveout"而不是直接用操作系统报告的空闲内存做预算。
  - **在系统内存充足的情况下仍错误驱逐已加载模型**（[Issue #13227](https://github.com/ollama/ollama/issues/13227)）以及**多模型加载互相驱逐**（[Issue #16719](https://github.com/ollama/ollama/issues/16719) 标题本身）：这类问题的共同根因都是"内存估算的输入信号不可靠"，而不是驱逐算法本身的逻辑错误——这提示 iDoris 在设计容量层时，"估算准确性"和"驱逐算法正确性"要分开验证，大多数生产事故出在前者。
  - **社区长期存在的功能请求**——"空闲时自动卸载模型/释放 GPU 内存"（[Issue #11085](https://github.com/ollama/ollama/issues/11085)）反映出 TTL/keep-alive 机制的默认值和可配置性一直是用户痛点，iDoris 的 TTL/降级策略应该在 Admin API 里把这些参数做成可见、可审计的一等公民配置，而不是隐藏的启发式默认值。
- **与 iDoris 的思路差异**：Ollama 的调度器目标是单一 Ollama server 内的模型生命周期管理，没有 iDoris 那种"多个不同厂商 runtime（oMLX/mlx_lm/llama.cpp/订阅 CLI/外部 API）统一调度"的异构性，且没有预算/隐私维度的准入判断——纯粹是资源约束下的 LRU 驱逐，iDoris 需要在此基础上叠加多维度准入。

#### 19. LocalAI
- **许可证**：MIT。**语言**：Go。
- **核心设计**：Backend Gallery 是一组 YAML 文件，每个定义一个后端，`uri` 指向 OCI 容器镜像，按需拉取而不是打包所有后端；OCI 镜像可要求 Sigstore 无密钥签名校验（[Backends 文档](https://localai.io/docs/backends/index.html)）。
- **可借鉴机制**：后端/模型的"按需拉取 + 签名校验"设计，对应 iDoris 的"确定性准入"里如果涉及模型/插件供应链完整性校验（例如未来允许第三方 runtime 适配器），可以直接参照其 OCI + Sigstore 签名验证流程作为最低限度的供应链安全基线。
- **踩过的坑**：未核实（搜索未命中与本次任务强相关的 postmortem，如需要可进一步挖掘 `mudler/LocalAI` 的 issue 列表）。
- **与 iDoris 的思路差异**：LocalAI 强调"广度"（支持 LLM/视觉/语音/图像/视频全品类模型），iDoris 聚焦 LLM 网关场景，供应链完整性校验思路可借鉴但整体定位不同。

#### 20. GPUStack
- **许可证**：Apache-2.0。**语言**：Python。
- **核心设计**：Scheduler 负责把模型实例分配到 worker，Controller 负责维持副本数等期望状态；Resource Fit Policy 按固定优先级排列候选放置方式：单 worker 单 GPU 全量卸载 > 单 worker 多 GPU 全量卸载 > 单 worker 部分卸载 > 跨多 worker 分布式推理（[Architecture 文档](https://docs.gpustack.ai/2.0/architecture/)）；VRAM 估算基于模型元数据的公式计算，官方文档明确承认这"通常是一个下界估算，可能不准确"（[VRAM 估算说明](https://docs.gpustack.ai/2.1/user-guide/model-deployment-management/)）。
- **可借鉴机制**：把"候选放置方案"显式排出优先级列表（而不是隐式的启发式打分），是一种更可解释、更易于人工审计和调试的调度设计，可以直接用于 iDoris 的容量层——"优先复用已加载的同 runtime 实例 > 加载空闲 runtime > 跨 runtime 借用容量 > 降级"这样的显式优先级列表。
- **踩过的坑**：
  - **异构 GPU（不同显存容量混部在同一 worker）下的 OOM**：3×3090 + 1×4070 混部，调度器选中的组合仍然 OOM（[Issue #1223](https://github.com/gpustack/gpustack.io/issues/1223) / [gpustack/gpustack#1223](https://github.com/gpustack/gpustack/issues/1223)）。
  - **声称资源充足却仍 OOM**：RPC server 场景下，声明的 6.2GB 显存需求确认"充足"，实际运行仍 OOM（[Issue #1131](https://github.com/gpustack/gpustack/issues/1131)）。
  - **启用扩展 KV Cache 后估算与实际分配不匹配**（[Issue #6289](https://github.com/gpustack/gpustack/issues/6289)）。
  - 这三个 issue 的共同模式与 Ollama 的坑高度一致：**VRAM/内存估算公式在异构硬件、特殊功能开启（如扩展 KV Cache）等边界场景下系统性失准**。对 iDoris 的启示：容量估算公式必须对"标配场景"和"边界配置（异构硬件/特殊推理特性开启）"分别验证，不能假设一个公式覆盖所有配置组合；生产环境应该在真正加载前后都做一次实测校准，而不是纯静态公式估算后就直接调度。
- **与 iDoris 的思路差异**：面向多 worker GPU 集群管理，iDoris 是单机场景，但"估算不可靠、需要分级校准"的教训是通用的。

#### 21. Xinference
- **许可证**：Apache-2.0。**语言**：Python。
- **核心设计**：基于 Xoscar Actor 框架——Supervisor Actor 管理 worker 生命周期、调度任务、监控系统状态；Worker Actor 执行具体模型计算；每个模型实例是一个独立的 Model Actor，运行在异步非阻塞的 Actor Pool 里（[internals 文档](https://inference.readthedocs.io/en/stable/development/xinference_internals.html)）。
- **可借鉴机制**：Actor 模型天然提供"每个模型实例是一个隔离的、消息驱动的单元"的抽象，与 llama-swap 的"单 goroutine run loop + 事件"殊途同归，都是为了避免共享可变状态的锁竞争。iDoris 如果用 Python/Node 实现，可以参照 Actor 模式或类似 llama-swap 的单线程事件循环，两者是等价的并发安全手段，选择哪个取决于运行时（Python 用 Actor 库如 Ray/Xoscar，Go/Node 用单 goroutine/事件循环更轻量）。
- **踩过的坑**：
  - **官方 Docker 镜像下 `RayWorkerVllm` 因 OOM 崩溃导致模型加载失败**（社区报告，未核实具体 issue 号，来源于综合搜索结果）。
  - **并发加载同一模型导致崩溃**："当两方同时调用同一个模型时，模型会崩溃并报错"（综合搜索结果描述，未核实具体 issue 号，需要进一步在 `xorbitsai/inference` issue 列表核实原文）。
  - 这两点如属实，说明 Xinference 的模型加载路径在**并发场景下缺少与 Ollama `activeLoading` 互斥锁等价的保护**。对 iDoris 的启示：这进一步印证第 18 项 Ollama 的经验——"同一模型/同一 runtime 的加载动作必须全局互斥"是一个反复被validate的必要设计，不能假设"加载"是幂等或可并发的操作。
- **与 iDoris 的思路差异**：Xinference 面向企业级分布式部署，iDoris 单机场景不需要 Supervisor/Worker 跨机分离，但并发加载保护的教训通用。

#### 22. LM Studio
- **许可证**：不适用（核心应用闭源；`lms` CLI、`mlx-engine` 等周边工具部分开源，未逐一核实各自许可证）。
- **核心设计**：JIT（Just-In-Time）加载——本地 server 可以列出所有本地模型（而不仅仅是已加载的），首次调用时才实际加载；Idle TTL（默认 60 分钟）+ Auto-Evict（加载新模型前自动卸载之前 JIT 加载的模型）两个机制配合工作（[Idle TTL and Auto-Evict 文档](https://lmstudio.ai/docs/app/api/ttl-and-auto-evict)）。
- **可借鉴机制**：JIT 加载让"模型列表 API"与"实际加载状态"解耦——OpenAI 兼容的 `/v1/models` 可以列出所有可路由的模型（不管是否已加载），这与 iDoris 的"确定性准入"里模型发现阶段的设计目标一致：客户端不需要关心某个模型当前是否驻留内存，准入链在实际路由时才决定是否需要触发加载。
- **踩过的坑**：
  - **通过非常规入口（"Locally"/"LM Link"）加载的模型绕过 Auto-Evict 与"仅保留最后一个 JIT 模型"策略**（[Issue #2051](https://github.com/lmstudio-ai/lmstudio-bug-tracker/issues/2051)）——说明多入口加载路径如果没有统一走同一个驱逐决策点，会产生驱逐策略"看不见"某些模型的盲区。对 iDoris 的启示：如果未来 iDoris 支持多种模型加载触发方式（Admin API 手动加载 + 请求触发的自动加载 + 订阅 CLI 转发），必须保证所有加载路径都汇入同一个驱逐/容量记账点，不能有"旁路"。
  - **API 触发的 JIT 加载会忽略保存的每模型设置**（[Issue #1463](https://github.com/lmstudio-ai/lmstudio-bug-tracker/issues/1463)）——配置来源不一致（UI 配置 vs API 触发）导致行为不一致，对 iDoris Admin API 的启示是：无论通过哪个入口（Admin API 提议、请求触发的自动加载）触发模型加载，都必须读取同一份"已批准版本化配置"，不能有多套配置来源。
  - **旧版本"自动卸载模型"功能故障**的历史 issue（[Issue #634](https://github.com/lmstudio-ai/lmstudio-bug-tracker/issues/634)）说明这类 TTL/驱逐功能长期是各类本地推理工具的高频 bug 来源，值得 iDoris 在设计阶段就投入更多针对性测试（例如显式的驱逐路径覆盖测试矩阵：不同触发源 × 不同并发状态）。
- **与 iDoris 的思路差异**：LM Studio 面向单用户桌面场景，没有租户隔离/预算概念；其 JIT+TTL+Auto-Evict 的产品化命名和默认值（60 分钟）可以直接作为 iDoris 面向"个人/中小组织"场景的默认参数参照起点。

#### 23. exo
- **许可证**：Apache-2.0。**语言**：Python。
- **核心设计**：无主从的 P2P 架构，设备间直接互联，UDP 广播做自动节点发现（每 2.5 秒广播一次）；默认分片策略是"环形内存加权分区"——沿环形拓扑推理，每个设备根据自身内存比例分配层数；支持拓扑感知张量并行，实时测量设备间带宽/延迟决定层放置；传输层支持 TCP、Thunderbolt 5 RDMA（1-2 微秒延迟）、实验性 CUDA IPC（[Starlog 深度解析](https://starlog.is/articles/developer-tools/exo-explore-exo/)）。
- **可借鉴机制**：exo 的场景是"多设备组成一个推理集群跑单个大模型"，与 iDoris"单机多运行时管理多个独立模型"是完全不同的问题（模型内并行 vs 模型间调度），因此直接可借鉴的机制有限；但其"根据设备实时可用内存动态计算分片比例"的思路，如果 iDoris 未来考虑同一台 Mac 上 CPU/GPU/ANE 之间的资源分配，"按实时可用资源比例动态分配工作量"的原则仍然适用。
- **踩过的坑**：未核实（未专门检索 exo 的生产事故报告；其定位是实验性/爱好者项目，相关 postmortem 较少）。
- **与 iDoris 的思路差异**：exo 解决的是"模型太大单机装不下，分布式装"的问题；iDoris 解决的是"单机上管理多个模型/多个 runtime 的准入与调度"的问题，两者是正交的问题域，仅供了解 macOS 上另一种资源管理哲学（去中心化 vs iDoris 的中心化准入链）。

---

### D. 隐私与护栏（Privacy & Guardrails）

#### 24. Microsoft Presidio
- **许可证**：MIT。**语言**：Python。**⚠️ 组织迁移**：仓库已从 `microsoft/presidio` 迁移到 [`data-privacy-stack/presidio`](https://github.com/data-privacy-stack/presidio)（`microsoft/presidio` 链接会重定向），说明该项目已从微软内部治理转向独立组织维护，需要关注后续治理连续性。
- **核心设计**：Analyzer/Anonymizer 两大组件分离——Analyzer 识别 PII 并给出置信度分数（正则+校验和算法降低误报 + spaCy/HuggingFace NER 模型理解语义上下文），Anonymizer 依据 Analyzer 的结果做掩码/加密（[Customizing Presidio Analyzer 文档](https://microsoft.github.io/presidio/samples/python/customizing_presidio_analyzer/)）。
- **可借鉴的具体机制**：
  1. **识别与处置分离**（Analyzer 只产出"发现了什么+置信度"，Anonymizer 决定"怎么处理"），这个分层可以直接映射到 iDoris 隐私准入层的接口设计——检测模块只负责打标签和置信度，处置策略（拒绝/脱敏/放行）是独立的、可按租户配置的策略层,不要耦合在检测模块内部。
  2. **可插拔 Recognizer**：每种 PII 类型（信用卡号、IBAN、SSN 等）是独立注册的 Recognizer，可以单独增删/调参，这与前面 vLLM Semantic Router 踩过的坑（不同实体类型召回率差异巨大）互相印证——独立可插拔的好处是可以针对某个类型单独修复，而不用重训整个模型。
- **踩过的坑**：
  - **FAQ 官方明确承认**："Recognizers are likely to have errors, both false-positive and false-negative"，并建议在接入前先用有代表性的数据集单独测试每个 Recognizer（[FAQ 文档](https://microsoft.github.io/presidio/faq/)）——这是 Presidio 官方对"隐私检测不是银弹"的坦诚说明，值得 iDoris 在文档里同样明确写清楚隐私准入的能力边界，避免用户误以为"iDoris 隐私准入=100% 保证不泄露"。
  - **学术研究发现的具体假阴性模式**：在漏洞归因文本等半结构化文本里，未识别的组织名（NER 消歧信息不足）、把端口号当纯整数处理（丢失实体上下文）会导致假阴性；DATETIME 和 IP_ADDRESS 识别器容易把版本号字符串误判为假阳性（综合自相关论文的分析）。对 iDoris 的启示：如果本地隐私检测依赖 NER，需要针对 iDoris 实际会遇到的文本类型（代码片段、日志、版本号等技术文本）专门做基准评测，通用 NER 的假阳性/假阴性模式在技术文本场景下会被放大。
- **与 iDoris 的思路差异**：Presidio 是一个通用 PII 检测/脱敏库，不内置"检测失败时的准入决策"（fail-open 还是 fail-closed 由调用方决定），iDoris 需要在此基础上明确加上"local_only 请求 fail-closed"这一层策略，Presidio 本身不提供这个语义。

#### 25. LLM Guard
- **许可证**：MIT。**语言**：Python。
- **核心设计**：input/output 两类 Scanner 各自独立、可插拔，`.scan()` 方法统一返回 `(sanitized_output, is_valid, risk_score)` 三元组契约（[Code output scanner 文档](https://github.com/protectai/llm-guard/blob/main/docs/output_scanners/code.md)）。
- **可借鉴机制**：统一的 `.scan()` 返回契约（脱敏结果 + 是否通过 + 风险分数）是一个很小但很实用的接口设计——三个字段分别对应"处置动作"、"准入决策"、"可审计的量化依据"，iDoris 的隐私/护栏检测模块可以直接采用同构的返回契约，方便 ATIF 轨迹记录时统一提取"风险分数"作为训练 System-1 的特征。
- **踩过的坑**：
  - **模式识别方式的注入检测对对抗样本脆弱**，**困惑度扫描（perplexity scanning）的延迟开销不适合 10ms 以内的实时路径**，**启发式的 RAG 投毒检测无法保证检出语义化的"睡眠"载荷**——这是第三方评测对 LLM Guard 已知局限性的总结（综合自搜索结果引用的评测项目）。对 iDoris 的启示：如果隐私/护栏检测要放在同步请求路径上（而不是异步旁路），必须评估检测方法本身的延迟量级是否与"确定性准入"的响应时间预算兼容，基于困惑度等统计方法的检测手段可能不适合放在同步关键路径。
  - **"Sentence" 模式下召回率更高但假阳性率也随之上升，"Full" 模式更实用均衡**；且第三方评测显示同类工具 Vigil 在两种模式下假阳性都显著低于 LLM Guard（综合自搜索结果引用的对比研究）。对 iDoris 的启示：检测粒度（逐句 vs 全文）本身就是一个需要显式暴露给管理员配置的权衡维度，不应该硬编码一种粒度。
- **与 iDoris 的思路差异**：LLM Guard 是应用层库，不内置准入链的编排顺序，iDoris 需要自己决定"隐私→预算→意图→容量→降级"的执行顺序和短路规则，LLM Guard 只提供其中"隐私/护栏"这一个环节的检测能力。

#### 26. NeMo Guardrails
- **许可证**：Apache-2.0（确认版本：[LICENSE.md](https://github.com/NVIDIA-NeMo/Guardrails/blob/develop/LICENSE.md) 声明 `SPDX-License-Identifier: Apache-2.0`；GitHub API 因组织迁移未能自动识别为 `NOASSERTION`，已手动核实）。**语言**：Python。
- **核心设计**：事件驱动运行时——用户发言产生 `UtteranceUserActionFinished` 事件，交由运行时处理并产生后续事件；Colang 是解释执行的交互建模语言，2.0 版本正在替代 1.0（[Colang 架构指南](https://docs.nvidia.com/nemo/guardrails/reference/colang-architecture-guide)）。配置由 YAML（模型/prompt/rails/tracing）+ Colang flows（对话流程/护栏逻辑）+ 自定义 Python action 三部分组成。
- **可借鉴机制**：事件驱动 + 显式声明的 "rails"（可以理解为准入链的每一段护栏）是一种比硬编码 if/else 更易于审计和可视化的编排方式，对 iDoris Admin API 的"提议→diff"设计有直接参照价值——如果准入链的每一层（隐私/预算/意图/容量/降级）都表达成类似"rail"的声明式对象，Admin API 的 diff 就可以直接对比"哪条 rail 被修改/新增/禁用了"，比对比一整段命令式代码的 diff 可读性更好。
- **踩过的坑**：Colang 1.0 的官方文档明确列出局限——主要支持文本交互、对自然语言指令（如提取用户提供的值）支持有限、不支持并发执行多个 action 或并发发起多个交互流程；Colang 2.0 截至 v0.10.0 之前也有已知限制——Guardrails Library 尚不能从 Colang 2.0 内部调用，且不支持某些生成选项（如记录已激活的 rails）（[Colang 2.0 概览](https://docs.nvidia.com/nemo/guardrails/colang_2/overview.html)）。对 iDoris 的启示：自建 DSL/声明式配置语言的演进成本很高（NeMo 用了两代 Colang 才逐步补齐并发能力），iDoris 的 Admin API 提议对象如果打算做成声明式 DSL，应优先复用成熟的表达式语言（参照 Bifrost 用 CEL 的选择，见第 3 项），而不是自造语言。
- **与 iDoris 的思路差异**：NeMo Guardrails 面向对话式护栏（防止对话偏离主题、防止越狱等），核心场景是"多轮对话流程控制"，而 iDoris 的准入链更偏"单次请求的确定性判定"，事件驱动的编排思路可借鉴，但不需要 Colang 这种完整的交互建模语言的复杂度。

---

### E. 观测与反馈（Observability & Feedback）

#### 27. Langfuse
- **许可证**：MIT（`ee/`、`web/src/ee/`、`worker/src/ee/` 目录下为独立商业许可，见 [`ee/LICENSE`](https://github.com/langfuse/langfuse/blob/main/LICENSE)）。**语言**：TypeScript。
- **核心设计**：两个应用容器（Web + Worker）+ 多种存储（PostgreSQL、ClickHouse、Redis/Valkey、S3/Blob Store）；trace 先批量写入 S3，只在 Redis 里存一个引用用于排队，Worker 再从 S3 拉取并写入 ClickHouse；API key 在 Redis 里做内存缓存，避免每次调用都打数据库；热门 prompt 走 Redis 读穿透缓存（综合自 [Langfuse 自托管文档](https://langfuse.com/self-hosting) 及相关技术博客）。
- **iDoris 可借鉴的具体机制**：
  1. **"原始内容落对象存储 + 元数据落结构化数据库/OLAP"的分层存储**，与 iDoris "审计只存元数据"的设计原则高度一致——可以直接参照 Langfuse 的分层：如果 iDoris 未来需要保留 ATIF 轨迹的原始 payload（用于训练 System-1），应该像 Langfuse 一样把大体积原始内容放对象存储、结构化的可查询元数据放数据库，而不是把两者混在一张表里；这样才能真正做到"审计只存元数据"，原始内容的留存策略可以独立于审计元数据单独配置（甚至可以选择完全不留存原始内容）。
  2. **异步写入路径（先入 Redis 队列引用，Worker 异步落库）**保证审计写入不阻塞主请求路径，这与 iDoris 的准入链应该"审计记录异步化，绝不能因为审计写入失败而拖慢或阻断实际的模型调用"的原则一致。
- **踩过的坑**：未核实（搜索未命中与预算竞态/隐私泄露强相关的 Langfuse 生产事故报告；其社区讨论区多是许可证边界的困惑，如 [Discussion #5002](https://github.com/orgs/langfuse/discussions/5002)，属于商业模式层面而非工程事故，未纳入"踩过的坑"）。
- **与 iDoris 的思路差异**：Langfuse 定位是可观测性+评测平台，不做路由/准入决策，是 iDoris 审计观测层的下游消费者角色参照，而不是准入链本身的参照。

#### 28. Helicone
- **许可证**：Apache-2.0。**语言**：TypeScript（代理运行在 Cloudflare Workers）。
- **核心设计**：五个服务组成——Web（Next.js 前端）、Worker（Cloudflare Workers 上的代理日志）、Jawn（Express+Tsoa 专用日志收集服务）、Supabase（应用数据库+鉴权）、ClickHouse（分析数据库）+ Minio（日志对象存储）。**关键设计**：日志只在**响应已经返回给客户端之后**才发布到 Kafka，raw request/response body 先存 S3，其余数据直接发 Kafka（不做处理）；ECS 消费者服务批量消费 Kafka、异步处理、单次 DB 事务批量写入（[Availability and Reliability 文档](https://docs.helicone.ai/references/availability)）。
- **可借鉴的具体机制**：**"先返回响应，后记审计"的严格时序**是本次调研里对 iDoris"审计只存元数据"要求最直接的实现参照——审计管道的任何故障（Kafka 挂了、DB 写入失败）都不应该影响用户已经拿到的响应，这个时序保证应该写进 iDoris 审计模块的核心不变量,而不仅仅是"尽量异步"的软要求。
- **踩过的坑**：未核实（Cloudflare Workers 的灰度发布策略——"整天缓慢滚动更新，每次只对一小部分流量生效"本身就是一种从"踩坑"里总结出的稳妥发布经验，但未找到对应的具体 postmortem 文章）。
- **与 iDoris 的思路差异**：Helicone 依赖 Cloudflare Workers 的边缘计算基础设施做全球分布式代理，iDoris 是单机本地网关，不需要边缘分布式架构，但"响应优先、审计异步兜底"的时序原则与部署规模无关，直接适用。

#### 29. OpenLLMetry
- **许可证**：Apache-2.0。**语言**：Python（也有其他语言 SDK，核心语义约定定义独立于语言）。
- **核心设计**：基于 OpenTelemetry 标准，扩展 GenAI 专用语义约定（span 属性）；较新版本把 prompt/output/instruction 数据整合进 `gen_ai.input.messages`、`gen_ai.output.messages`、`gen_ai.system_instructions` 等属性，替代早期实验性的 event 字段（[GenAI 语义约定文档](https://www.traceloop.com/docs/openllmetry/contributing/semantic-conventions)）。
- **可借鉴机制**：**标准化的 span 属性命名规范**可以直接作为 iDoris ATIF 轨迹字段命名的参照系——如果 iDoris 的轨迹格式在关键字段（输入/输出/系统指令/token 用量）上采用与 `gen_ai.*` 语义约定兼容的命名，未来接入标准 OTel 生态（Grafana/Jaeger 等）的成本会大幅降低，不需要自造一套字段名再做映射层。
- **踩过的坑**：**语义约定本身在快速变化**——`gen_ai.prompt`/`gen_ai.completion` 等早期属性已被官方标记为废弃（[Issue #3515](https://github.com/traceloop/openllmetry/issues/3515)），维护者原话是"Nobody designed telemetry for multi-kilobyte prompts and multi-megabyte images; we had to invent some practical extensions"。对 iDoris 的启示：ATIF 轨迹的字段 schema 从设计第一天就应该显式版本化（而不是假设字段名一成不变），因为"如何描述一次 LLM 调用的可观测数据"这个领域标准本身还在快速演进，iDoris 的 Admin API 版本化理念（提议→diff→批准→版本化）应该同样应用到 ATIF 轨迹的 schema 本身。
- **与 iDoris 的思路差异**：OpenLLMetry 是通用可观测性标准的 GenAI 扩展，不涉及路由/预算决策，纯粹是"如何记录"的参照，不是"如何决策"的参照。

---

### F. 本次新增调研的 5 个项目（均已核实为非 Rust）

#### 30. Higress（阿里巴巴，AI Gateway）
- **许可证**：Apache-2.0。**语言**：Go。CNCF 项目。
- **核心设计**：AI 原生网关，原生支持 LLM 调用、MCP、多种 AI 推理场景；支持按 token 用量做限流（可对特定模型设置如"每分钟 500 token"的限制，时间窗口可配置到秒/分/时/天）；多模型 fallback——目标服务失败（限流/访问失败）时自动切到备用模型；**per-provider token 失败熔断**——某个鉴权 token 连续异常响应超过阈值后暂停使用该 token，直到健康检查恢复（[Higress AI Gateway 文档](https://higress.ai/en/docs/ai/quick-start/)、[CNCF 加入公告](https://www.cncf.io/blog/2026/03/25/higress-joins-cncf-delivering-an-enterprise-grade-ai-gateway-and-a-seamless-path-from-nginx-ingress/)）。
- **可借鉴机制**：**per-token 健康熔断**（而不是 per-provider 整体熔断）是一个比多数网关更细粒度的设计——同一个 provider 下如果有多个凭证（对应 iDoris 可能的多虚拟 key 场景），某一个凭证异常不应该拖累其它凭证的可用性判断，这个粒度可以直接映射到 iDoris 的"降级"层：降级判断的作用域应该精确到 (tenant, virtual_key, provider, model)，与前面 Kong 的坑（第 6 项）互相印证。
- **踩过的坑**：未核实。
- **与 iDoris 的思路差异**：Higress 是通用云原生网关叠加 AI 能力，面向多租户 SaaS/企业网关场景，iDoris 是本地优先单机网关，token 级限流的实现方式（依赖网关侧统计）思路可借鉴，具体的分布式限流实现不适用于单机场景。

#### 31. KubeAI
- **许可证**：Apache-2.0。**语言**：Go。
- **核心设计**：K8s 原生的 AI 推理 Operator，按平均活跃请求数自动伸缩 Pod 数量，请求到达但无 Pod 运行时会**挂起请求、拉起 Pod、Pod 就绪后透明转发**（对客户端无感知）；代理内置前缀感知负载均衡以优化 KV 缓存利用率；**刻意不依赖 Istio/Knative（scale-from-zero）或 Prometheus 指标适配器**，保证在几乎任何 K8s 集群上开箱即用（[Autoscaling 概念文档](https://www.kubeai.org/concepts/autoscaling/)）。
- **可借鉴的具体机制**：
  1. **"请求到达时才拉起资源，期间挂起请求而不是直接拒绝"** 的 scale-from-zero 模式，与 iDoris 本地场景下"请求到来才加载 runtime"（类似 LM Studio 的 JIT 加载）是同一个思路的不同实现层级——可以进一步参照 KubeAI 对"挂起等待"设置合理超时与排队上限的做法，避免请求无限期挂起。
  2. **刻意最小化外部依赖**（不依赖 Istio/Knative/Prometheus）是一个对 iDoris 很有意义的架构哲学参照：iDoris 面向本地优先、个人/中小组织场景，同样应该在设计 Admin API、审计、预算等子系统时评估"是否真的需要引入一个重型外部依赖"，优先选择自包含的实现，降低部署门槛（呼应 PGL"妈妈测试"里"首次启动 ≤60秒、无需改配置文件"的门槛要求）。
- **踩过的坑**：未核实。
- **与 iDoris 的思路差异**：KubeAI 面向 K8s 集群的水平扩缩容，iDoris 是单机场景没有"扩容"概念，但"请求挂起等待资源就绪"与"最小化外部依赖"两条设计哲学具有跨规模的普适性。

#### 32. GPTCache
- **许可证**：MIT。**语言**：Python。
- **核心设计**：模块化的语义缓存架构——Embedding 模型、向量存储、相似度评估器、缓存存储管理器均可独立替换；Cache Manager 统一管理 Cache Store 与 Vector Store，缓存满时按替换策略（当前支持 LRU 和 FIFO）驱逐；行业最佳实践建议 TTL 驱逐是必要的默认项，因为"仅靠 LRU 而没有 TTL，可能会无限期地提供过期答案"的风险（综合自 [Zilliz GPTCache 介绍](https://zilliz.com/what-is-gptcache) 及相关实践文章）。
- **可借鉴的具体机制**：语义缓存可以作为 iDoris **预算层的前置降本手段**——在真正进入 reserve/settle 流程之前，先做一次语义相似度检索，命中则直接返回缓存结果、完全跳过 provider 调用与计费；这是与 iDoris 现有预算设计完全正交、可选叠加的一层优化，尤其适合本地/中小组织场景下常见的重复性查询（如固定的客服话术、常见代码补全模式）。
- **踩过的坑**：**"仅 LRU 无 TTL 会无限期提供过期答案"**是被业界公开讨论的已知风险点（例如第三方项目 `ai-opticore` 专门开 issue 讨论["Bounded semantic cache: prune stale/low-similarity entries"](https://github.com/MugdhaSontakke/ai-opticore/issues/5)）。对 iDoris 的启示：如果引入语义缓存作为预算层前置优化，必须同时设计 TTL 失效机制，且要考虑"模型/policy 版本变化后旧缓存是否还有效"的失效触发条件（例如 Admin API 批准了新的路由策略版本后，语义缓存里基于旧策略产生的结果应该被视为过期）。
- **与 iDoris 的思路差异**：GPTCache 是一个通用缓存中间件，不涉及隐私/租户隔离/预算记账，如果 iDoris 引入类似机制，必须确保缓存命中也要在审计轨迹里留痕（哪怕不真正调用 provider），并且要考虑跨租户缓存复用是否违反隐私隔离要求（大概率不能跨租户共享缓存）。

#### 33. FastChat Controller（LMSYS）
- **许可证**：Apache-2.0。**语言**：Python。
- **核心设计**（源码：[`fastchat/serve/controller.py`](https://github.com/lm-sys/FastChat/blob/main/fastchat/serve/controller.py)）：三组件——Web Server（用户接口）、Model Worker（承载一个或多个模型）、Controller（中央注册表+调度器）；Worker 启动时向 Controller 自注册，周期性发送心跳汇报状态；Controller 周期性清理超过心跳过期时间未上报的失联 Worker；调度支持两种派发方式——`LOTTERY`（按处理速度加权随机）和 `SHORTEST_QUEUE`（按归一化队列长度选最空闲的）（[FastChat README](https://github.com/lm-sys/FastChat/blob/main/README.md)）。
- **可借鉴的具体机制**：**心跳注册+过期清理+队列长度感知调度**是"容量"层一个极简但完整的最小可行设计——Controller 不需要理解模型内部细节，只需要 Worker 主动上报"我支持什么模型、我的处理速度、我当前队列多长"三个信号，就能做出合理调度。iDoris 如果未来要管理多个本地 runtime 进程（甚至跨机器/跨订阅 CLI 转发的情况），可以直接参照这个"心跳+自注册+队列长度"的最小接口,而不需要一开始就引入 llm-d 那种重量级的 Filter/Score 插件框架——FastChat Controller 的极简版本可以作为 iDoris 容量层的"最小可行实现"起点，llm-d 的 Filter→Score→Pick 可以作为"未来扩展方向"的参照。
- **踩过的坑**：**多 Worker 承载不同模型时，Web 界面不会正确更新可用模型列表**（[Issue #1718](https://github.com/lm-sys/FastChat/issues/1718)）——说明"Worker 动态上下线/模型集合变化"与"客户端可见的模型列表"之间的同步是一个容易被忽视的一致性问题。对 iDoris 的启示：Admin API 批准的模型/runtime 配置变化后，必须要有明确的"客户端可见的模型列表几时刷新"的一致性保证,不能让路由层内部已经识别到新配置，但对外暴露的 `/v1/models` 端点却是旧的缓存快照。
- **与 iDoris 的思路差异**：FastChat Controller 设计目标是研究/评测场景（Chatbot Arena 的后端），没有预算/隐私/多租户概念，是一个"纯容量调度"的极简参照，需要 iDoris 在此基础上叠加准入链的其余四层。

#### 34. Ray Serve LLM
- **许可证**：Apache-2.0。**语言**：Python。
- **核心设计**：`OpenAiIngress` 提供 OpenAI 兼容的 FastAPI 入口，执行自定义路由逻辑（前缀感知/会话感知）；默认用"二选一"（Power of Two Choices）负载均衡，可插拔自定义 Router；`PrefixCacheAffinityRouter` 用近似基数树（radix tree）维护前缀信息，且该结构可以在路由实例重启后依然存在、可在多个路由实例间共享（[架构总览文档](https://docs.ray.io/en/latest/serve/llm/architecture/overview.html)、[前缀感知路由文档](https://docs.ray.io/en/latest/serve/llm/user-guides/prefix-aware-routing.html)）。
- **可借鉴机制**：**"路由决策依赖的状态（前缀树）独立于路由进程本身的生命周期"**是一个值得注意的设计——路由实例可以重启、扩容、缩容，但路由决策依赖的历史信息（哪些前缀被哪些实例处理过）通过外部化的数据结构得以保留。这对 iDoris 未来如果需要"记住哪类请求之前被路由到哪个 runtime/降级路径"（用于加速重复模式的判定）有参照价值——决策所需的历史状态应该设计成可以独立于进程重启而存活。
- **踩过的坑**：未核实。
- **与 iDoris 的思路差异**：Ray Serve LLM 面向多副本集群自动扩缩容场景（`num_replicas="auto"`），iDoris 单机场景没有水平扩缩容需求，"二选一负载均衡"和前缀树共享机制主要在多实例场景下才有意义。

---

## 三、最佳实践清单（按 iDoris 分层汇总）

### 接入层（Ingress / OpenAI 兼容）
- **模型发现与实际加载状态解耦**：`/v1/models` 应能列出所有"可路由"的模型，不管其是否已实际加载，加载决策留到真正路由时才做——出处：[LM Studio JIT 加载](https://lmstudio.ai/docs/app/api/ttl-and-auto-evict)。
- **鉴权失败与依赖服务故障要返回不同的错误语义，不能互相伪装**：不要让缓存层/依赖服务故障表现为"用户凭证错误"——出处：[OpenRouter 2026-02 故障复盘](https://www.requesty.ai/blog/correlated-provider-outage-september-2026)。

### 身份 / 虚拟 Key / 租户隔离层
- **虚拟 key 的语义边界要在设计阶段写清楚**（它到底是纯凭证，还是"凭证+路由策略"的自包含对象），避免后续出现"到底哪个配置该生效"的歧义 bug——出处：[Portkey Issue #1190](https://github.com/Portkey-AI/gateway/issues/1190)。
- **预算/限流的作用域要精确到 (tenant, virtual_key, provider, model) 四元组**，不能让一个维度耗尽误伤其它维度——出处：[Kong AI Gateway Issue #3949](https://github.com/Kong/developer.konghq.com/issues/3949)、[Higress per-token 熔断](https://higress.ai/en/docs/ai/quick-start/)。

### 模型管理层（多运行时 / 加载 / 驱逐）
- **"加载中"全局互斥，"已加载"并发服务**——同一时刻只允许一个模型处于加载/驱逐的关键区，已就绪模型间的请求处理完全并行——出处：[Ollama `sched.go` 的 `activeLoading` 设计](https://github.com/ollama/ollama/blob/main/server/sched.go)。
- **三个关注点彻底解耦**：进程机制 / 调度策略（队列+决策树）/ 驱逐策略（纯函数）分离成独立可替换的接口，调度器只通过 `Effects` 接口产生副作用，方便无侧效应单测——出处：[llama-swap `internal/router/design.md`](https://github.com/mostlygeek/llama-swap/blob/main/internal/router/design.md)。
- **驱逐决策必须放进资源自身的单写者状态机，而不能靠外部读一次快照就分支决策**——这是 llama-swap Issue #946（模型永久卡死）的根因教训，也是 Ollama/GPUStack/Xinference 多个"驱逐/加载竞态"issue 的共同模式——出处：[llama-swap design.md 对 Issue #946 的复盘](https://github.com/mostlygeek/llama-swap/blob/main/internal/router/design.md)。
- **驱逐/卸载后要主动轮询验证资源已释放，设置合理超时后退化为估算值，不要乐观假设或无限等待**——出处：[Ollama `waitForVRAMRecovery`](https://github.com/ollama/ollama/blob/main/server/sched.go)。
- **容量估算公式要按"标配场景"和"边界配置"（异构硬件、特殊功能开启）分别验证，不能假设一个公式覆盖所有组合**——出处：[GPUStack Issue #1223](https://github.com/gpustack/gpustack/issues/1223)、[Issue #6289](https://github.com/gpustack/gpustack/issues/6289)、[Ollama Issue #16719](https://github.com/ollama/ollama/issues/16719)。
- **OOM 自愈重试必须有熔断（只重试一次），避免"加载失败→驱逐重试→再失败"的抖动循环**——出处：[Ollama `oomRetryAttempted` 标志位](https://github.com/ollama/ollama/blob/main/server/sched.go)。
- **所有模型加载触发路径（手动/自动/API/CLI转发）必须汇入同一个驱逐与记账决策点，不能有旁路**——出处：[LM Studio Issue #2051](https://github.com/lmstudio-ai/lmstudio-bug-tracker/issues/2051)。
- **"记账动作"与"资源真正被占用/释放"之间的因果顺序要仔细核对，避免计数只增不减的资源泄漏**——出处：[llama-swap `GrantServe` 契约](https://github.com/mostlygeek/llama-swap/blob/main/internal/router/design.md)。

### 路由 / 意图层
- **路由决策按 Filter（硬性淘汰）→ Score（打分排序）→ Pick（最终选择+兜底降级）三段式组织，每段都是可独立测试的纯函数**——出处：[llm-d Router 调度器文档](https://llm-d.ai/docs/architecture/core/router/epp/scheduling)。
- **路由目标是"识别意图类别"，类别到具体模型/runtime 的映射放在可热更新的配置表里，新增模型不需要重训路由模型**——出处：[Arch-Router 论文](https://arxiv.org/abs/2506.16655)。
- **路由训练与在线推理路径解耦成独立模块，方便迭代替换算法**——出处：[RouteLLM `routellm/routers/` 结构](https://github.com/lm-sys/RouteLLM)。
- **优化目标（成本/质量/延迟）做成可配置的一等公民，不同租户/场景可声明不同目标**——出处：[Not Diamond `tradeoff` 参数](https://docs.notdiamond.ai/docs/what-is-model-routing)。
- **识别出单一路由信号的副作用后，用可调权重在多个信号间插值，而不是硬编码单一目标**——出处：[vLLM production-stack `loadaware-beta`](https://docs.vllm.ai/projects/production-stack/en/latest/use_cases/loadaware-routing.html)。
- **容量层的最小可行实现只需要"心跳自注册 + 队列长度上报"两个信号即可支撑合理调度，不必一开始就上重量级插件框架**——出处：[FastChat Controller](https://github.com/lm-sys/FastChat/blob/main/fastchat/serve/controller.py)。

### 隐私层
- **检测（识别+打分）与处置（拒绝/脱敏/放行）严格分离成两个独立模块**——出处：[Presidio Analyzer/Anonymizer 分离](https://microsoft.github.io/presidio/samples/python/customizing_presidio_analyzer/)。
- **每种敏感信息类型独立可插拔、独立验证，不能假设"隐私检测"是整体能力**：不同实体类型的召回率可能天差地别（如 EMAIL_ADDRESS 完全未被检出而 SSN 正常拦截）——出处：[vLLM Semantic Router Issue #712](https://github.com/vllm-project/semantic-router/issues/712)。
- **"未命中"和"检测失败"必须是两种不同状态**：未命中默认放行（除非策略要求 fail-closed），检测失败（服务异常）才应触发 iDoris 的 fail-closed 保守路径——出处：[vLLM Semantic Router Issue #4271](https://github.com/vllm-project/semantic-router/issues/4271)。
- **隐私检测覆盖面要包含工具调用返回结果，不能只检测用户输入这一个注入点**——出处：[vLLM Semantic Router Issue #3560](https://github.com/vllm-project/semantic-router/issues/3560)。
- **官方文档应坦诚说明检测能力的边界（存在假阳性/假阴性），避免用户误以为隐私准入=绝对保证**——出处：[Presidio FAQ](https://microsoft.github.io/presidio/faq/)。
- **检测粒度（逐句 vs 全文）应作为显式可配置维度暴露给管理员**，因为粒度与召回率/误报率强相关——出处：[LLM Guard 评测对比](https://www.getmaxim.ai/articles/top-5-ai-gateways-for-ai-governance-a-comprehensive-guide/) 相关综合评测。
- **若检测方法本身有不可忽视的延迟（如困惑度扫描），需评估是否兼容同步准入路径的响应时间预算**——出处：LLM Guard 第三方评测总结（综合搜索结果）。

### 预算层
- **reserve/settle 必须用原子操作（Lua 脚本/乐观锁），不能用"先超时重试、再无条件覆盖"的模式，否则预留会被覆盖丢失**——出处：[LiteLLM Issue #32614](https://github.com/BerriAI/litellm/issues/32614)。
- **跨进程/跨 pod 的预算协调依赖（如 Redis）如果启动期探测失败，不能永久退化为单机内存模式，必须持续重探测**——出处：[LiteLLM Issue #42653](https://github.com/BerriAI/litellm/issues/42653)。
- **只对真正成功完成的调用 settle 扣费，失败/fallback 不计费；流式请求要向 provider 确认真实计费口径，客户端断连不能自动等同于零消耗**——出处：[OpenRouter 计费策略](https://openrouter.ai/docs/faq) 与 [Provider Routing 文档](https://openrouter.ai/docs/guides/routing/provider-selection)。
- **语义缓存可作为 reserve/settle 之前的可选前置降本层，但必须有 TTL 失效机制，且要考虑策略版本变化后缓存是否应失效、以及跨租户复用是否违反隔离要求**——出处：[GPTCache 架构](https://zilliz.com/what-is-gptcache) 与 [相关 stale cache 讨论](https://github.com/MugdhaSontakke/ai-opticore/issues/5)。

### 审计观测层
- **原始内容（可能很大、可能含敏感信息）落对象存储，结构化可查询元数据落数据库/OLAP，两者物理分层**，真正贯彻"审计只存元数据"——出处：[Langfuse 存储架构](https://langfuse.com/self-hosting)。
- **审计写入必须在响应已返回给客户端之后才发生，任何审计管道故障都不能影响用户已拿到的响应**——出处：[Helicone 异步日志架构](https://docs.helicone.ai/references/availability)。
- **可观测性字段 schema 要显式版本化**，不要假设字段命名一成不变；优先采用行业语义约定（如 `gen_ai.*`）而不是自造字段名，降低未来接入标准生态的成本——出处：[OpenLLMetry 语义约定变迁](https://github.com/traceloop/openllmetry/issues/3515)。

### 学习层（ATIF / System-1）
- **训练数据稀疏是常态**，需要设计"用规则/强模型 judge 做半自动标注"的数据增强路径，不能假设真实人工反馈量级充足——出处：[RouteLLM 论文的数据增强方法](https://arxiv.org/abs/2406.18665)。
- **投机性重新评估（例如排队请求的多次重新调度）不应重复写入训练用的轨迹记录**，只有真正提交执行的决策才应计入 ATIF 轨迹，否则会污染 System-1 的训练数据分布——出处：[llama-swap `Swapper.EvictionFor` vs `OnSwapStart` 的日志原则](https://github.com/mostlygeek/llama-swap/blob/main/internal/router/design.md)。
- **路由模型应作为独立可灰度/可回滚的子服务，与转发执行路径解耦**，方便独立升级 System-1 而不影响准入链主链路的稳定性——出处：[NVIDIA LLM Router 的 Controller/Server 分离](https://developer.nvidia.com/blog/deploying-the-nvidia-ai-blueprint-for-cost-efficient-llm-routing/)。

### 管理面（Admin API / 提议 → diff → 批准 → 版本化）
- **声明式配置对象（而非命令式代码）承载路由/护栏规则，diff 才能对比"哪条规则被改了"而不是对比一整段逻辑代码**——出处：[Portkey Config 设计](https://portkey.ai/docs/product/ai-gateway)、[NeMo Guardrails 的声明式 rails](https://docs.nvidia.com/nemo/guardrails/reference/colang-architecture-guide)。
- **策略表达式优先复用成熟的表达式语言（如 CEL），不要自造 DSL**——自建语言的演进成本很高（NeMo 用了两代 Colang 才补齐基础能力）——出处：[Bifrost 用 CEL 描述护栏规则](https://www.getmaxim.ai/articles/deploying-ai-governance-for-enterprises-with-bifrost-edge-bifrost-gateway/)、[Colang 2.0 已知限制](https://docs.nvidia.com/nemo/guardrails/colang_2/overview.html)。
- **无论通过哪个入口触发（Admin API/自动加载/CLI 转发），都必须读取同一份已批准的版本化配置**，不能有多套配置来源——出处：[LM Studio Issue #1463](https://github.com/lmstudio-ai/lmstudio-bug-tracker/issues/1463)。
- **配置变更后，对外暴露的模型/能力列表要有明确的一致性刷新保证**，不能让路由内部已生效新配置、但外部可见列表仍是旧快照——出处：[FastChat Issue #1718](https://github.com/lm-sys/FastChat/issues/1718)。
- **尽量减少对重型外部依赖的强绑定**（如不强制要求 Istio/Knative/Prometheus），保持自包含、低部署门槛，呼应 PGL"妈妈测试"对首次启动时间和配置复杂度的硬门槛——出处：[KubeAI 架构哲学](https://www.kubeai.org/concepts/autoscaling/)。
- **出站地址类字段（自定义 host/upstream）必须做白名单校验，防止 SSRF**——出处：[Portkey SSRF 安全公告 GHSA-hhh5-2cvx-vmfp](https://github.com/Portkey-AI/gateway/security)。

---

## 附：本次调研中发现的仓库/组织迁移（写报告时的事实核实记录）

- `envoyproxy/ai-gateway` → 已更名为 **Agent Router**，仓库迁移到 [`theagentrouter/agent-router`](https://github.com/theagentrouter/agent-router)（2026-09-10，[相关 PR 讨论](https://github.com/vllm-project/semantic-router/pull/4278)）。
- `microsoft/presidio` → 迁移到 [`data-privacy-stack/presidio`](https://github.com/data-privacy-stack/presidio)（GitHub API 自动重定向确认，未找到官方迁移公告，未核实具体原因）。
- `NVIDIA/NeMo-Guardrails` → 迁移到 [`NVIDIA-NeMo/Guardrails`](https://github.com/NVIDIA-NeMo/Guardrails)（NVIDIA 内部组织重组的一部分，未核实具体原因）。
- `katanemo/archgw` → 更名为 [`katanemo/plano`](https://github.com/katanemo/plano)（同时语言标注为 Rust）。
- `llm-d-incubation/llm-d-inference-scheduler` 已不存在，当前实现在 [`llm-d/llm-d-router`](https://github.com/llm-d/llm-d-router)。
- **LiteLLM** 的 GitHub 仓库简介已自称 "Rust core with Python SDK"，说明其代理核心正在向 Rust 迁移，需要在后续跟踪中确认这一变化对其虚拟 key/预算模块可参照性的影响（本报告分析的仍是其公开文档描述的行为语义，与具体实现语言无关）。
- **NVIDIA LLM Router** 的 Router Controller、**vLLM Semantic Router** 的推理核心、**Arch-Router** 配套的 `katanemo/plano` 代理、**agentgateway** 均为 Rust 实现——这几项均为用户在任务中明确点名的项目，本报告如实记录其语言事实，但仅将其作为架构/协议设计参照，不构成"非 Rust"结论的一部分；本次调研主动新增的 5 个项目（Higress、KubeAI、GPTCache、FastChat、Ray Serve LLM）均已通过 `gh api` 核实为非 Rust。

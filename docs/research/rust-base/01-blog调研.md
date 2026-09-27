# 调研：iDoris 用 Rust 重写模型网关——以哪个开源项目为基础最快

> 数据源：`mushroom-blog` MCP（`search_posts` / `get_post`）。
> 结论先说：blog 库里**没有任何一篇直接拆解 TensorZero / Helicone AI Gateway / Arch(archgw)+Plano(katanemo) / LangDB / Traceloop Hub / Noveum / mistral.rs / candle / vLLM semantic-router / llama-swap / LiteLLM / Portkey** 本身（全部 0 命中或纯噪声命中）。但库里有大量**同类问题（Rust 网关工程、本地路由决策、模型管理、审计/护栏、fail-closed 网关架构）的一手拆解**，其中好几篇直接点名了 RouteLLM、Arch-Router、vLLM Semantic Router、katanemo/plano，可以作为二手判断依据；另外有多篇 Rust 网关/harness 项目的深度测评，工程细节可直接复用到 iDoris 的选型判断里。

---

## 1. 检索日志表

方法：按任务要求拆出关键词，中英文都覆盖候选项目名、技术栈词、路由/网关主题词，每个关键词实际调用一次 `search_posts`（`limit` 默认 20）。共执行 **55 个关键词**，超过任务要求的 40 个下限。

| # | 关键词 | 命中数 | 选读/备注 |
|---:|---|---:|---|
| 1 | TensorZero | 0 | 无 |
| 2 | Helicone | 2（噪声，AgentSight/n8n 客服拆解，与 Helicone 项目无关）| 未选读 |
| 3 | Arch | 20（全部因子串 "arch"/"architecture" 命中，与 Arch/archgw 项目无关）| 未选读 |
| 4 | archgw | 0 | 无 |
| 5 | Plano | 0（但 katanemo/plano 作为附带信息出现在「semantic-router」一文中）| 见 §2-1 |
| 6 | katanemo | 0 独立命中 | 同上，附带出现 |
| 7 | Arch-Router | 0 独立命中 | 同上，附带出现 |
| 8 | RouteLLM | 1 | 「semantic-router-ai-agent-entry-point-guide」 |
| 9 | semantic router | 1 | 同上，精读 |
| 10 | vLLM router | 1 | 同上（文中同时评了 vLLM Semantic Router） |
| 11 | llama-swap | 0 | 无 |
| 12 | LiteLLM | 1（Hindsight 简介带过，非拆解）| 未选读 |
| 13 | Portkey | 1（jev-chat-jarvis 噪声）| 未选读 |
| 14 | OpenRouter | 多条噪声 + treg（"OpenRouter for Agent Tools"）| 未精读 |
| 15 | mistral.rs | 0 独立命中（adk-rust 一文提到支持 mistral.rs 做本地推理）| 见 §2-8 |
| 16 | candle | 0 | 无 |
| 17 | Ollama | 20+（本地推理综述类文章）| 未逐篇精读 |
| 18 | LocalAI | 多条（LocalAGI、depth-anything.cpp）| 未精读 |
| 19 | oMLX | 少量（M1 Max 指南等）| 未精读 |
| 20 | MLX | 20+ | 未逐篇精读 |
| 21 | llama.cpp | 20+ | 未逐篇精读 |
| 22 | envoy | 0 | 无 |
| 23 | Rust | 20+（Rust 生态综述类）| 精读多篇（见下） |
| 24 | Rust 重写 | 若干（authentik、Codex CLI、Obscura） | 精读 authentik、Codex CLI |
| 25 | LLM 网关 | 少量噪声（webclaw）| 未精读 |
| 26 | AI 网关 | 噪声 | 未精读 |
| 27 | 模型网关 | lobehub、WeKnora（噪声）| 未精读 |
| 28 | 推理网关 | 0 相关 | 无 |
| 29 | 模型路由 | 噪声为主 | 未精读 |
| 30 | 路由器 | 噪声为主 | 未精读 |
| 31 | axum | 0 直接命中 | 无 |
| 32 | tokio | 0 直接命中（噪声）| 无 |
| 33 | 意图路由 | 1（aether 小模型运维 Agent，转载）| 未精读 |
| 34 | 模型切换 | 噪声 | 未精读 |
| 35 | 模型热加载 | 0 | 无 |
| 36 | 模型管理 | llm-dock、zinc、voicestudio | 未精读（内容偏本地推理面板） |
| 37 | 多模型 | 多条（exxperts、lobehub 等）| 未精读 |
| 38 | 本地模型 | 20+ | 未逐篇精读 |
| 39 | guardrail | 0 直接（Archestra 通过其他关键词命中）| 见 §2-4 |
| 40 | 成本路由 | 0 | 无 |
| 41 | OpenTelemetry | 0 直接（Archestra/Trinity 通过其他关键词命中）| 见 §2-4 |
| 42 | bandit | 少量噪声 | 未精读 |
| 43 | 预算 | 噪声 | 未精读 |
| 44 | sidecar | opendisplay、hauhaucs、tare、berd、cloudflare-mesh | 精读 tare |
| 45 | ext_proc | 0 | 无 |
| 46 | fork | 噪声 | 未精读 |
| 47 | vendor | 噪声 | 未精读 |
| 48 | 开源选型 | 0 | 无 |
| 49 | gateway | wemux、kairic-edge、archestra、opensquilla、ccteam 等 | 精读 archestra、opensquilla、ccteam提及 |
| 50 | inference gateway | 0 | 无 |
| 51 | model router | 0 | 无 |
| 52 | 网关性能 | 0 | 无 |
| 53 | 反馈 | 噪声（jev 系列）| 未精读 |
| 54 | 实验 | 噪声 | 未精读 |
| 55 | A/B | 噪声 | 未精读 |

**精读文章清单（get_post 全文，12 篇）：**

1. [AI Agent 入口路由怎么选：5 类开源方案实测对比](https://blog.mushroom.cv/blog/semantic-router-ai-agent-entry-point-guide/)（含 RouteLLM / Arch-Router / vLLM Semantic Router / semantic-router / Octopus-v2 横评）
2. [OneCLI：让 AI Agent 永远看不到真实密钥的开源凭证网关](https://blog.mushroom.cv/blog/onecli-ai-agent-credential-gateway-secret-vault-rust/)（Rust MITM 网关）
3. [多 Agent 无人值守跑 4 天做了什么：OpenClaw 网关架构拆解](https://blog.mushroom.cv/blog/openclaw-gateway-unattended-multi-agent-orchestration/)（fail-closed 网关设计）
4. [Archestra：企业级一体化 AI 平台](https://blog.mushroom.cv/blog/archestra-enterprise-ai-platform-mcp-gateway-guardrails-llm-proxy-k8s/)（LLM 网关 + 护栏 + 可观测性功能全景）
5. [OmniRoute：290 个 AI 提供商、一个端点](https://blog.mushroom.cv/blog/omniroute-free-ai-gateway-290-providers-never-stop-coding/)（fallback 级联 + 19 种路由策略）
6. [ClawRouter：专为 AI Agent 设计的 LLM 路由框架](https://blog.mushroom.cv/blog/clawrouter-llm-router-agent-native-multi-model-cost-optimization/)（本地 15 维评分，<1ms 路由）
7. [OpenSquilla 0.5.2：本地 Agent 路由器](https://blog.mushroom.cv/blog/opensquilla-token-efficient-ai-agent-squilla-router/)（LightGBM+ONNX 本地分类路由）
8. [ADK-Rust：43 个 crate 拆开来用](https://blog.mushroom.cv/blog/adk-rust-zavora-ai-rust-agent-framework-43-crates-modular/)（Rust Agent 运行时架构范式）
9. [tare：无损上下文压缩](https://blog.mushroom.cv/blog/tare-lossless-context-compression-cache-correct-output-aware/)（Rust proxy/CLI/MCP 四态部署工程范式）
10. [Ferrum：Rust 单二进制本地推理](https://blog.mushroom.cv/blog/ferrum-infer-rs-rust-single-binary-local-llm-metal-cuda/)（Metal+CUDA 共用 runtime）
11. [arle：Rust 本地 LLM 运行时 + 在线蒸馏](https://blog.mushroom.cv/blog/arle-local-llm-distillation-guide/)
12. [orion-core 实测：Rust Agent Harness](https://blog.mushroom.cv/blog/orion-core-rust-agent-harness-local-llm-teardown/)
13. [SIE：Superlinked 开源统一推理引擎](https://blog.mushroom.cv/blog/superlinked-sie-inference-engine-agent/)（模型按需加载 + LRU 淘汰）
14. [三足鼎立：Codex Harness / DeepSeek Harness / AgentScope 2.0 横评](https://blog.mushroom.cv/blog/deepseek-harness-everything-plugin-cordis-compare-claude-code-codex/)

---

## 2. 逐篇笔记

### 2-1. AI Agent 入口路由怎么选（5 类开源方案实测对比）
**URL**: https://blog.mushroom.cv/blog/semantic-router-ai-agent-entry-point-guide/

**核心观点**：把"路由该走哪条路径"（agent/搜索/邮件/模型/链路）和"该选哪个模型"这两件事分开看。前者叫**路径路由**，后者才是 RouteLLM 解决的问题。作者横评五类方案：
- **类 A 嵌入/语义路由**（aurelio-labs/semantic-router，3760★，4天前有 push；vLLM semantic-router，5078★，耦合 vLLM serving）——**~0.1B、零 LLM 调用、库形态可换嵌入 provider**，作者结论是"路径路由"首选。
- **类 B 微型专用路由模型**（Supra-Router-51M，51M 参数，训练集仅 992 行，尚早期）。
- **类 C 偏好/复杂度路由**（**RouteLLM**，5275★，但 `pushedAt` 停在 2024-08，近两年无实质更新，作者判定"它解决的是模型选择，不是路径路由，硬套会文不对题"；**Arch-Router-1.5B**，1.5B 生成式，1471 下载/270 赞，模型固定不可换底座；其代理框架 **katanemo/plano** 反而很活跃，6910★，昨天还有 push）。
- **类 D 端侧函数调用模型**（Octopus-v2、Salesforce xLAM-1b-fc-r、MadeAgents/Hammer——Hammer 实测仅 121★，2025-06 后无更新，"比预想中冷门得多"）。
- **类 E 路由框架/网关**（katanemo/plano、ClawRouter、openziti/llm-gateway 等，定位是承载路由逻辑的基础设施，不与前四类同台比较）。

**与 iDoris 选型的关系**：iDoris 要做"隐私→预算→意图→容量"四级确定性路由，这正是**路径路由**问题，不是模型选择问题——文章的核心区分对 iDoris 路由层设计直接适用：**意图判定这一步应该走本地小模型/嵌入相似度，零 LLM 调用**，符合 iDoris 已定的"用小模型/System-1 做意图判定"思路。RouteLLM 的教训（近两年无更新、定位不匹配）说明它不该作为 iDoris 路由层的参照对象。

**可直接借鉴的设计**：
- 路由器要做成**可换后端的库**，而不是绑死某个嵌入/生成模型（作者点名 semantic-router 的"库不是固定模型"是它相对 Arch-Router 的关键优势）——iDoris 的意图判定模型应做成可插拔接口，而不是硬编码某个 System-1 模型。
- **两阶段架构**：第一阶段路径路由（走哪条业务路径），第二阶段才是"调哪个工具/参数"。iDoris 的隐私→预算→意图→容量四级路由本质上也应该是分阶段确定性判断，而不是一次性丢给一个大模型做端到端决策。

---

### 2-2. OneCLI：让 AI Agent 永远看不到真实密钥的开源凭证网关
**URL**: https://blog.mushroom.cv/blog/onecli-ai-agent-credential-gateway-secret-vault-rust/

**核心观点**：onecli/onecli（2746★，Apache-2.0，**Rust + TypeScript**）在 Agent 和 API 之间插一个 MITM 网关：Agent 只持有占位符 key，网关按 host/path 规则匹配后**实时解密注入真实凭证**（AES-256-GCM 静态加密，运行时不落地明文）。架构是 **Rust 网关（性能敏感路径，微秒级延迟）+ Next.js 控制面（管理 Agent/密钥/权限）+ PostgreSQL**。Bitwarden 主动来做 SDK 集成，说明密码管理器行业正视 Agent 场景的凭证风险。

**与 iDoris 选型的关系**：这正是 iDoris 网关要处理的"审计只存元数据、不留敏感明文"和"本地优先"的具体落地案例——凭证永远不出网关边界，模型/工具侧永远拿不到真实密钥。

**可直接借鉴的设计**：
- **"性能敏感路径用 Rust，控制面用别的语言"**的分层选择本身就是一个可复用的架构判断：iDoris 的请求路径（隐私过滤 + 预算检查 + 路由决策）应该是纯 Rust 热路径，管理 API/仪表盘可以用别的技术栈实现而不拖累延迟。
- **MITM 代理注入 + 零信任占位符**模式可以直接套用在 iDoris 对接外部 API（订阅 CLI 转发、外部 API）时的凭证管理上：外部 API key 不下发给上层调用者，全部由网关按路由决策注入。

---

### 2-3. 多 Agent 无人值守跑 4 天：OpenClaw 网关架构拆解
**URL**: https://blog.mushroom.cv/blog/openclaw-gateway-unattended-multi-agent-orchestration/

**核心观点**：OpenClaw（38.8万★）的架构核心是一句话——"**可信网关，不可信执行，确定性策略**"。具体机制：
- 网关和执行环境**物理分离**：凭证、策略判断、状态留在网关侧，执行沙箱拿不到网关权限；
- **"拒绝是结构性的，不是请模型自觉"**——工具要么存在要么不存在，审批链路走不通默认结果是**拒绝**（fail-closed），不是放行（fail-open）；
- 凭证**按次铸造、10 分钟 TTL**，落盘哈希存储；
- **审批门槛绑定内容哈希**（canonical command + cwd + env hash + 文件内容哈希）——防止"用一次批准的操作偷换成另一次危险操作"；
- 一个容易被忽略的关键限制：**沙箱和审批默认是关闭的**，"跑无人值守多天"必须显式打开这层加固，否则默认配置下并不安全。

**与 iDoris 选型的关系**：这是"local_only 请求 fail-closed"这条设计原则在真实生产系统里的具体实现范例——iDoris 的 local_only 请求 fail-closed，不应该是一个上层业务逻辑判断，而应该是**网关结构性拒绝**（工具/路由不存在，而不是"模型答应不这么做"）。

**可直接借鉴的设计/教训**：
- fail-closed 要做成**架构级不可协商**，而不是靠 prompt 或模型自觉；
- **默认关闭高开销的安全机制、显式打开**是合理的工程折中，但必须在文档和 CLI 里显著提示，否则用户会误以为默认配置已经安全（这是本文明确指出的一个"容易被忽略的关键限制"，iDoris 做管理 API 时要避免这个陷阱）。

---

### 2-4. Archestra：企业级一体化 AI 平台
**URL**: https://blog.mushroom.cv/blog/archestra-enterprise-ai-platform-mcp-gateway-guardrails-llm-proxy-k8s/

**核心观点**：archestra-ai/archestra（4201★，TypeScript，AGPL 3.0）一个 URL + 一个 token 覆盖 LLM 网关（虚拟 API Key、费用限额、动态模型路由）、MCP 网关（OAuth + On-Behalf-Of）、双 LLM 护栏（一个模型生成、另一个独立验证）+ Lethal Trifecta 防护（高权限+不可逆操作+外部影响三重触发即拦截）、OpenTelemetry traces + Prometheus metrics、SSO/RBAC。3 个 Fortune-50 部署，p95 延迟 31ms，已加入 CNCF。

**与 iDoris 选型的关系**：这是"审计只存元数据 + 可选记录轨迹 + 管理 API"这组需求在企业级产品里的**功能全景参照**——不是选型对象（TypeScript，非 Rust），但它列出的功能清单（虚拟 API Key、费用限额、双 LLM 护栏、OTel）可以直接当作 iDoris 网关的功能对照 checklist。

**可直接借鉴的设计**：
- **双 LLM 护栏（一个生成、一个独立验证）+ Lethal Trifecta（多维度同时触发才拦截）**是比单一护栏更精细的风险控制思路，可用于 iDoris 的隐私过滤/预算控制之外再加一层"高风险组合动作"探测；
- OpenTelemetry + Prometheus **开箱即用而非事后打补丁**——iDoris 从项目一开始就该把可观测性接口设计进 Rust 网关核心，而不是留到后期外挂。

---

### 2-5. OmniRoute：290 个 AI 提供商、一个端点
**URL**: https://blog.mushroom.cv/blog/omniroute-free-ai-gateway-290-providers-never-stop-coding/

**核心观点**：diegosouzapw/OmniRoute（27824★，MIT，TypeScript，5 个月做到 2.7万★）把订阅账号/API key/免费 tier 组合成一个端点，**4 层自动 fallback 级联**（订阅→API Key→廉价→免费）+ **19 种路由策略**（`auto/coding`、`auto/cheap`、`context-relay`跨 provider 传递上下文、`cache-optimized`锁定 prompt 前缀最大化缓存命中、`lkgp`粘性到上次成功路径、`fusion`扇出多模型+judge 合成、`pipeline`链式执行）。**三层弹性**：Provider 断路器（408/5xx 触发）、Connection 冷却（429 遵守 Retry-After）、Model 锁定（单模型故障不影响同 provider 其他模型）。

**与 iDoris 选型的关系**：iDoris 要管理异构模型运行时并做"容量"维度路由，OmniRoute 的**三层弹性架构**（provider 级断路器 / connection 级冷却 / model 级锁定）是"容量感知路由"和"多个异构运行时统一调度"的一个可直接复用的分层故障隔离模型。

**可直接借鉴的设计**：
- `cache-optimized` 策略（把同一个可复用 prompt 前缀锁定在同一个后端账号/运行时，最大化 prompt cache 命中）——iDoris 在多运行时之间路由时也要考虑 KV cache/prompt cache 的粘性，不能纯按负载均衡轮询打散；
- 三层故障隔离（provider/connection/model）比单一熔断器更精细，可以直接套用到"多个异构模型运行时"的健康检查设计上。

---

### 2-6. ClawRouter：专为 AI Agent 设计的 LLM 路由框架
**URL**: https://blog.mushroom.cv/blog/clawrouter-llm-router-agent-native-multi-model-cost-optimization/

**核心观点**：BlockRunAI/ClawRouter（6673★，MIT，TypeScript）路由逻辑**完全在本地跑，<1ms**：15 维度本地评分（不调用任何外部 API）→ 四级复杂度（SIMPLE/MEDIUM/COMPLEX/REASONING）× 三种策略（ECO/AUTO/PREMIUM）→ 选出该层最便宜可用模型。作者明确对比："之前'智能路由'的意思是你在仪表板配规则，平台帮你调 API，路由决策在对方服务器上跑；ClawRouter 的路由完全本地，没有中间服务器知道你在用什么模型"。

**与 iDoris 选型的关系**：这是"本地优先 + 路由决策不出机器"的直接对照——iDoris 的确定性路由（隐私→预算→意图→容量）本质上也应该是**纯本地、亚毫秒级、无网络依赖**的判断链路，ClawRouter 的架构验证了这个方向的可行性和收益（92% 成本节省）。

**可直接借鉴的设计/教训**：
- 15 维度评分是**规则驱动而非端到端训练模型**，作者自己承认"不如端到端训练的路由模型精准"——iDoris 如果用小模型做意图判定，要清楚这是在用更贵的方案换取更高精度，需要有数据支撑这个权衡是值得的；
- 路由延迟必须控制在个位数毫秒级，否则在多步 Agent 循环里会累积成秒级延迟——这是给 iDoris Rust 网关设一个明确的非功能性指标（NFR）。

---

### 2-7. OpenSquilla 0.5.2：本地 Agent 路由器
**URL**: https://blog.mushroom.cv/blog/opensquilla-token-efficient-ai-agent-squilla-router/

**核心观点**：SquillaRouter 是一个**在设备上运行的 LightGBM + ONNX Runtime 分类器**，评估每个 turn 的复杂度（长度/语言/是否含代码/关键词/语义 embedding），路由到 C0→C3 四级模型，**分类零 token、prompt 不出机器**。PinchBench 实测：分数持平（0.9251 vs 0.9255），成本从 $6.23 降到 $0.69（降 89%），token 消耗降 44%。技术报告主张"harness-native 路由器会把日常 agent 流量转化为自我改进的数据飞轮"。

**与 iDoris 选型的关系**：这是"意图判定用小模型/System-1"这条设计的一个**已验证的工业级实现范式**——用轻量 ML 分类器（而非 LLM）做路由决策评分，训练/推理成本极低，且效果与"全部用最贵模型"几乎打平。

**可直接借鉴的设计**：
- **自适应推理**（只对判定为复杂的 turn 才启用扩展推理/CoT）+ **自适应 system prompt**（简单任务用轻量指令，避免 prompt cache 被浪费）——这两条可以直接映射到 iDoris 的"意图判定→按复杂度选模型"链路上；
- 用 **LightGBM+ONNX** 这类轻量传统 ML 模型做路由分类，而不是上一个小 LLM，是成本/延迟上更优的选择，值得 iDoris 在"小模型意图判定"之外，额外评估纯规则/传统 ML 分类器路径。

---

### 2-8. ADK-Rust：43 个 crate 拆开来用
**URL**: https://blog.mushroom.cv/blog/adk-rust-zavora-ai-rust-agent-framework-43-crates-modular/

**核心观点**：zavora-ai 社区维护的 ADK-Rust（社区项目，非 Google 官方，667★，Apache-2.0）把 Agent 运行时拆成 **43 个可独立发布的 crate**，按 tier（minimal/standard/enterprise/full）通过 Cargo feature flag 组合。冷启动 109ms（比 Python SDK 快 4.6x），内存 ~15MB（比 LangGraph 低 6x），但**单次 agent loop 开销（568μs）反而比 Python SDK（253μs）高**——Rust 的收益主要在冷启动和内存，不在循环吞吐。支持通过统一 trait 换 provider（Gemini/OpenAI/Anthropic/DeepSeek/Ollama/Bedrock/mistral.rs 本地推理），`adk-graph` 有 SQLite checkpoint 的 durable resume（进程崩溃重启可从断点恢复）。

**与 iDoris 选型的关系**：这是 iDoris 用 Rust 重写时"要不要模块化到 crate 级"这个架构决策的一个直接参照系——43-crate 的拆分方式（core/server/auth/graph/eval/realtime/browser/RAG/sandbox/skill 各自独立 crate）示范了如何让"网关最小可用核心"和"可选高级能力"解耦，避免把整个框架强制打包给轻量部署场景。

**可直接借鉴的设计/教训**：
- **按 tier 划分 feature flag**（minimal → standard → enterprise → full）是让同一套代码库既能跑最小网关又能承载企业级能力的成熟做法，iDoris 的 Rust 网关也应该从第一天就按这个思路拆 crate，而不是先写成单体再拆；
- **性能数据要诚实报告局限**：文章明确指出"Rust 的优势不在循环开销，在冷启动和内存"——这提醒 iDoris 不要迷信"用 Rust 就一定更快"，要针对具体瓶颈（是冷启动、内存，还是热路径吞吐）做针对性验证；
- **`adk-skill` 用词法匹配（非嵌入模型）解析 SKILL.md 并注入 prompt**，是一个"轻量、零推理成本的匹配层"范例，可以类比到 iDoris 意图判定里"先词法/规则粗筛，再上小模型精判"的分层设计。

---

### 2-9. tare：无损上下文压缩（Rust 工程范式）
**URL**: https://blog.mushroom.cv/blog/tare-lossless-context-compression-cache-correct-output-aware/

**核心观点**：Rust 编写、MIT，**同一套代码库同时提供 proxy / CLI / MCP server / 库四种部署形态**（`tare-proxy` 反代 + base_url 零改动接入；`tare wrap claude` 包一层 CLI；`tare-mcp` 本地 stdio server 不需要 API key）。三条工程约束：无损默认（有损手段显式 opt-in）、**cache-correct**（先探测 provider 前缀缓存断点，只压缩断点之后的动态后缀，避免一个改动的字节作废整段 10 倍缓存折扣）、**output-aware**（监测输出 token 尖峰，自动回退压缩力度，避免"压狠了模型用啰嗦输出补偿、总账反涨"）。9 个 crates、228 个测试、`fmt`/`clippy -D warnings`/`cargo deny` 门禁每个提交。

**与 iDoris 选型的关系**：这篇的价值不在于压缩本身，而在于**"一套 Rust 代码库同时支持 proxy/CLI/MCP/库四态部署"**这个架构范式——iDoris 的网关也需要同时支持"作为 OpenAI 兼容 HTTP 服务被上层调用"和"作为库嵌入其它 Rust/Tauri 应用"这两种形态，tare 提供了一个可复用的 workspace 组织方式（多 crate + 统一核心逻辑）。

**可直接借鉴的设计**：
- **cache-correct** 原则对 iDoris 尤其重要：iDoris 的路由决策（隐私→预算→意图→容量）如果需要改写 prompt/上下文，必须先确定 provider 端的前缀缓存边界，不能因为路由层的改写把缓存折扣全部作废；
- **代理转发客户端送来的任意凭证**（包括订阅 OAuth token，不仅是 API key）——这与 iDoris 要接"订阅 CLI 转发"的需求直接对应，tare 的凭证透传设计值得参考；
- **响应头汇报每轮决策数据**（如 `x-tare-input-tokens`、`x-tare-aggression`）是一个轻量、无侵入的可观测性模式，可用于 iDoris 网关向上层暴露路由决策依据（走了哪条路、为什么），而不需要额外的 side-channel。

---

### 2-10. Ferrum：Rust 单二进制本地推理，Metal 和 CUDA 共用同一套 runtime
**URL**: https://blog.mushroom.cv/blog/ferrum-infer-rs-rust-single-binary-local-llm-metal-cuda/

**核心观点**：sizzlecar/ferrum-infer-rs（14★，MIT，早期项目）**一个 Rust 二进制、无 Python 运行时**，Metal 和 CUDA 走同一套 runtime（通过 feature flag + 预编译产物区分，量化格式分工：Metal 走 GGUF，CUDA 走 GPTQ/safetensors）。服务端能力不是玩具级：连续批处理、分页 KV cache、前缀缓存、带类型的准入控制，OpenAI 兼容 Chat Completions + 无状态 Responses API。性能表**带 95% 置信区间和测试条件**（M1 Max 32GB 上 Qwen3.5 4B 并发16时 61.9±0.1 tok/s）。`ferrum doctor` 命令**只解析、不下载不启动**，体现"先告诉我要做什么，再决定要不要下载"的克制设计。

**与 iDoris 选型的关系**：这不是网关项目，而是"本地推理运行时"项目，但它示范了 iDoris 要管理的"oMLX、mlx_lm、llama.cpp 等多个异构运行时"里，**单个运行时本身**可以做到什么程度的工程质量（诚实的性能数据、后端统一 API、按需下载）——可以作为 iDoris 评估/接入某个具体本地推理后端时的质量基准，而不是网关本身的参照对象。

**可直接借鉴的设计**：
- **`doctor` 命令模式**（先解析配置、打印将要执行的动作，不做任何有副作用的操作）——iDoris 管理 API 应该有类似的"预演"能力，尤其是涉及下载模型、切换运行时这类重操作前；
- 性能数据**带置信区间和测试条件**是可信度的最低门槛，iDoris 后续做路由决策的成本/延迟建模时也应遵循这个标准，而不是单点数字。

---

### 2-11. arle：Rust 本地 LLM 运行时 + 在线蒸馏（OPD）
**URL**: https://blog.mushroom.cv/blog/arle-local-llm-distillation-guide/

**核心观点**：cklxx/arle（14★，MIT，纯 Rust，2026-03 发布）**一个二进制三件事**：`arle serve`（OpenAI 兼容 HTTP 服务）、`arle`（本地 Agent/REPL）、`arle train opd`（在线蒸馏，**教师就是正在 serve 的生产模型**，不需要额外部署）。35B MoE 在 M4 Pro 上跑出 85 tok/s（每 token 仅激活约 3B）。4B 学生蒸馏后 MATH-500 从 0.518 升到 0.792（+27pp），逼近 35B 教师的 0.82。默认开启投机解码（+47%，输出与贪心解码逐位相同）、跨轮 KV 缓存复用、KV Recall（超长上下文只保留 sink+最近+top-k 相关块）。

**与 iDoris 选型的关系**：这是"一个二进制同时承担推理服务 + Agent + 训练"思路的直接示范，虽然定位是本地推理引擎而非网关，但它验证了 Rust 生态里"轻量本地服务 + 在线学习闭环"是可行的工程路线——如果 iDoris 未来要在网关层做"用生产流量持续蒸馏/微调 System-1 决策模型"，这是一个可参考的实现范式（教师=正在 serve 的模型，学生=待优化的小模型）。

**可直接借鉴的设计**：与 iDoris 主线（模型网关）关系较间接，主要价值是"在线蒸馏"范式对 iDoris 未来做"System-1 决策模型持续优化"时的参考，而非网关架构本身的借鉴。

---

### 2-12. orion-core：Rust Agent Harness 深度实测（重要的负面案例）
**URL**: https://blog.mushroom.cv/blog/orion-core-rust-agent-harness-local-llm-teardown/

**核心观点**：anistark/orion-core（3★，MIT，从桌面应用拆出的 Rust 库）提供**对话循环层**（工具调用解析/执行/回填、按整轮裁剪的 token 预算、10 种聊天模板、15 种流式事件、审批钩子），但**不带工具、沙箱、记忆、MCP**，README 说"支持 llama.cpp/MLX/云 API"，但**现成的后端只有一个 OpenAI 兼容 HTTP 客户端**——MLX/llama.cpp 都要接服务中转或自己写 trait 实现。作者实测发现两个真实工程坑：① **宽松的工具调用解析会把任何带 `name` 字段的 JSON 块当成调用**（有安全隐患，必须挂审批钩子）；② **HTTP 后端按"字符数÷4"估算 token，中文实测少估 2.0–2.4 倍**，导致上下文预算形同虚设（6 轮对话真实 5801 token，估算只有 2523）。

**与 iDoris 选型的关系**：这是本次调研里**最重要的负面教训**——iDoris 面向中文场景（意图判定、本地小模型、System-1），如果网关或路由层用"字符数/4"这类通用估算做 token 计费和上下文预算，**在中文场景下会系统性失准 2 倍以上**，直接影响预算路由这条主线的可靠性。

**可直接借鉴的教训**：
- **token 估算必须接真实分词器**，不能用字符数近似，尤其是 iDoris 明确要做"预算"维度的路由决策，估算偏差会直接导致预算失控或不必要的降级；
- **工具调用解析的宽松匹配是安全隐患**——任何"看起来像调用"的输出都可能被误执行，必须配合审批钩子；这与 iDoris 的"local_only fail-closed"设计原则是同一类问题：宽松匹配 = 隐性的 fail-open，需要显式收紧；
- **"README 说支持 X，但现成实现只是通过 HTTP 转接"**——这是评估任何候选开源项目时都要验证的一点：iDoris 选型时要亲自读代码确认"支持"的真实含义，而不是只看 README 声明（这也是这篇文章本身示范的调研方法）。

---

### 2-13. SIE：Superlinked 开源统一推理引擎
**URL**: https://blog.mushroom.cv/blog/superlinked-sie-inference-engine-agent/

**核心观点**：superlinked/sie（2286★，Python，Apache 2.0）把 Agent 需要的 5 类任务（搜索/文档转 Markdown/结构化提取/内容安全/Agent 循环）统一进**一套 OpenAI 兼容 API + 一个集群**，**100+ 模型按需加载 + LRU 淘汰**（显存不足时淘汰最久未用模型）。本地 `pip install` 到 GKE/EKS/AKS 生产集群（Helm+Terraform）用**同一套 SDK 代码**，只改 `base_url`。

**与 iDoris 选型的关系**：这是"管理多个异构模型运行时"这条需求在**统一推理引擎**（而非网关）层面的一个已验证范式——虽然是 Python 而非 Rust，但"**按需加载 + LRU 淘汰**"这个模型生命周期管理策略，正是 iDoris 要解决的"oMLX/mlx_lm/llama.cpp 多运行时按需调度"问题的通用解法，可以直接移植到 Rust 网关的模型管理模块设计中。

**可直接借鉴的设计**：
- **按需加载 + LRU 淘汰**是 iDoris 模型管理层（不只是网关路由层）应该采用的核心策略，适配 iDoris 本地优先 + 资源受限的场景；
- **本地开发和生产集群用同一套代码，只换 base_url**——这条工程原则同样适用于 iDoris：网关的路由/审计/护栏逻辑应该与"跑在哪"（本地 Mac / 云端）解耦。

---

### 2-14. 三足鼎立：Codex Harness / DeepSeek Harness / AgentScope 2.0 横评
**URL**: https://blog.mushroom.cv/blog/deepseek-harness-everything-plugin-cordis-compare-claude-code-codex/

**核心观点**：这不是网关项目横评，而是 **Agent Harness**（智能体运行时底座）横评，但其中 **Codex Harness** 的架构对 iDoris 直接有参考价值：OpenAI 开源，Apache-2.0，**80+ Rust 子模块，9600+ 次提交**，百万级用户生产验证，核心组件包括 `codex exec`（CI 流水线执行器）、SDK（TypeScript/Python）、`app-server`（JSON-RPC，把 Agent 嵌进业务系统：持久化会话、流式事件、任务中断、自定义工具、人工审批）。DeepSeek Harness（MIT，**Cordis 微内核，一切皆插件**）四种运行模式，模型无关，可调度 Claude Code/Codex 作为子 Agent。

**与 iDoris 选型的关系**：Codex Harness 的"**80+ Rust 子模块**"证明了大规模 Rust 项目按微内核+插件方式拆分子模块、并做到百万用户生产级验证是可行的工程路径——这是除 ADK-Rust（43 crate）之外，第二个"Rust 大项目模块化拆分"的一手证据，两者互相印证"模块化拆 crate/子模块"是 Rust 网关类项目的通用最佳实践，而不是个别项目的偶然选择。DeepSeek Harness 的 **Cordis 微内核、一切皆插件**架构，也呼应了"多个异构模型运行时"需要一个统一的可插拔运行时抽象。

**可直接借鉴的设计**：
- **微内核 + 插件**（Cordis 模式）和**分层子模块**（Codex 的 80+ Rust 子模块）是两种验证过的大型 Rust 系统组织方式，iDoris 网关在"管理多个异构模型运行时"时，应优先考虑把每个运行时适配器（oMLX adapter / mlx_lm adapter / llama.cpp adapter / 订阅 CLI adapter / 外部 API adapter）做成独立可插拔模块，而不是网关核心里硬编码分支逻辑；
- 文中给出的选型口诀（"数据出境无所谓 + 用 OpenAI → Codex；不出境 + 用阿里云 → AgentScope；什么都想自己控 → DeepSeek Harness"）提示：**开源基础的选择要先问清楚约束条件**（数据主权、模型绑定意愿、工程定制能力），这个方法论同样适用于 iDoris 选 Rust 网关基础项目——先明确 iDoris 自己的约束（本地优先/多运行时/隐私强约束/不绑定单一模型厂商），再倒推该 fork 哪个项目或只借鉴设计。

---

## 3. blog 中关于各候选项目的评价汇总

| 候选/参照项目 | blog 是否直接拆解过 | 结论 |
|---|---|---|
| **TensorZero** | 否（0 命中）| blog 未覆盖，无法给出二手判断 |
| **Helicone AI Gateway** | 否（0 相关命中）| 同上 |
| **Arch / archgw（katanemo）** | 否，但 **Arch-Router-1.5B** 和其框架 **katanemo/plano** 在 §2-1 里被提及：Arch-Router 是"模型固定、不可换底座"的 1.5B 生成式路由模型（1471 下载/270 赞），其外围代理框架 plano "很活跃，6910★，昨天有 push" | plano 本身值得关注但 blog 未专门拆解；Arch-Router 定位是"第二阶段"路由（选模型），不是"路径路由" |
| **Plano（katanemo）** | 同上，仅附带提及 | 无独立评价 |
| **LangDB ai-gateway** | 否（0 命中）| 无 |
| **Traceloop Hub** | 否（0 命中）| 无 |
| **Noveum** | 否（0 命中）| 无 |
| **mistral.rs** | 否独立拆解，仅在 ADK-Rust 一文中作为"可选本地推理后端之一"被提及（支持 Gemma 4、Qwen 3.5）| 未见工程质量评价 |
| **vLLM semantic-router** | 是（§2-1）：5078★，昨天有 push，"更生产级，代价是耦合 vLLM serving" | 若 iDoris 已用/计划用 vLLM 做后端可考虑，否则耦合成本高 |
| **llama-swap** | 否（0 命中）| 无 |
| **LiteLLM** | 否独立拆解（仅 1 处一带而过提及）| 无实质评价 |
| **Portkey** | 否（0 相关命中）| 无 |
| **RouteLLM** | 是（§2-1）：5275★，但 **`pushedAt` 停在 2024-08，近两年无实质更新**，"它解决的是模型选择问题，不是路径路由，硬套到入口场景会文不对题" | 明确的负面结论：不推荐作为 iDoris 路由层参照 |
| **Arch-Router 论文/模型** | 是（§2-1）：模型固定、1.5B、不可换底座 | 定位是"选模型"这一层，非"路径路由"，且灵活性不足 |

**小结**：mushroom-blog 对本题点名的 8 个 Rust 候选（TensorZero/Helicone/Arch/Plano/LangDB/Traceloop Hub/Noveum/mistral.rs server/vLLM semantic-router）**只覆盖了 vLLM semantic-router 一个**，且是作为"路径路由"参照物顺带提及，并非专门拆解。这意味着 **iDoris 团队需要通过 blog 之外的渠道（GitHub 一手调研、代码阅读）来评估 TensorZero / Helicone AI Gateway / Arch(archgw) 这几个最直接的候选**，本次 blog 调研只能提供"通用工程范式"层面的间接支持，无法替代对这几个项目本身的直接评审。

---

## 4. 初步建议

**声明前提**：由于 blog 未直接拆解 TensorZero / Helicone AI Gateway / Arch(archgw) / LangDB / mistral.rs server 这几个最直接的候选，以下建议主要基于"通用 Rust 网关工程范式"和"路由/模型管理设计模式"的间接证据，**不能替代对这几个候选项目本身的直接代码审查**。建议团队在做最终决策前，另行对 TensorZero、Helicone AI Gateway、Arch(archgw) 三者做一次直接的代码/文档评审（这是本次调研发现的最大信息缺口）。

1. **架构组织方式：优先"微内核 + 可插拔运行时适配器"，而非单体**。
   证据：ADK-Rust 的 43-crate 拆分（§2-8）、Codex Harness 的 80+ Rust 子模块（§2-14）、DeepSeek Harness 的 Cordis 微内核插件架构（§2-14）三个独立案例都指向同一个结论——大型 Rust Agent/网关类系统普遍采用模块化拆分，把每个模型运行时（oMLX/mlx_lm/llama.cpp/订阅 CLI/外部 API）做成独立 crate/插件，网关核心只负责路由决策 + 审计 + 护栏。**风险**：过度拆分会增加维护和版本管理复杂度（ADK-Rust 文中也提到 0.x 阶段跨 crate 破坏性变更的问题），需要为 iDoris 设定清晰的 crate 边界和版本策略。

2. **路由层设计：路径路由（隐私→预算→意图→容量）应该是纯本地、亚毫秒级、规则/轻量 ML 驱动，不应该是端到端 LLM 调用**。
   证据：ClawRouter 的 15 维本地评分 <1ms（§2-6）、OpenSquilla 的 LightGBM+ONNX 本地分类器（§2-7）、semantic-router 的嵌入相似度路由（§2-1）三者共同验证了"路由决策应零/低 LLM 调用、完全本地"这个方向，且都报告了显著的成本/延迟收益。**建议**：iDoris 的"意图判定用小模型/System-1"这条既有设计方向是对的，但应评估是否可以先用**规则 + 轻量传统 ML 分类器**（如 OpenSquilla 的 LightGBM 路线）承担"隐私/预算"这两级更确定性的判断，把小模型/System-1 留给"意图"这一级更需要语义理解的判断，分层降低整体延迟和复杂度。出处：§2-1、§2-6、§2-7。

3. **Fail-closed 必须是网关结构性行为，不能靠模型自觉或宽松匹配**。
   证据：OpenClaw 的"拒绝是结构性的，不是请模型自觉"（§2-3）是正面范例；orion-core 的"宽松工具调用解析会把任意 JSON 误判为调用"（§2-12）是**必须避免的负面模式**。**建议**：iDoris 的 local_only fail-closed 逻辑要在路由决策的最底层强制执行（工具/路由不存在，而非返回错误码让上层决定是否重试云端），并且要像 OpenClaw 一样明确文档化"哪些安全机制默认开、哪些默认关"，避免默认配置给用户"已经安全"的错觉。出处：§2-3、§2-12。

4. **Token 计费/预算路由必须用真实分词器，不能用字符数估算——这是中文场景的一个已验证的真实陷阱**。
   证据：orion-core 实测中文 token 被少估 2.0–2.4 倍，6 轮对话后真实 token 已超预算但估算值只有一半（§2-12）。**建议**：iDoris 的预算路由维度必须内置真实分词器（至少覆盖 Qwen 系等中文场景常用分词器），不能沿用"字符数/4"这类英文语境下的通用估算，否则预算路由这条主线在中文场景下会系统性失效。出处：§2-12。

5. **凭证管理：网关内 MITM 注入 + 零信任占位符，凭证从不下发给上层调用者**。
   证据：OneCLI 的 Rust MITM 网关架构（§2-2），性能敏感路径（凭证注入）用 Rust、控制面用别的语言的分层选择被验证可行（微秒级延迟）。**建议**：iDoris 对接"订阅 CLI 转发"和"外部 API"两类运行时时，可直接借鉴这个模式——外部凭证只存在于网关内部，路由决策产生后由网关注入，模型调用方（包括本地推理引擎和上层业务）永远拿不到真实凭证。出处：§2-2。

6. **模型运行时管理：按需加载 + LRU 淘汰是通用最优策略**。
   证据：SIE 的 100+ 模型按需加载 + LRU 淘汰（§2-13），在资源受限、多模型运行时场景下已被验证。**建议**：iDoris 管理 oMLX/mlx_lm/llama.cpp 多个本地运行时时，应采用相同策略而非常驻全部运行时，尤其是在本地优先、单机资源有限的部署场景下。出处：§2-13。

7. **信息缺口与后续行动**：本次调研未能在 blog 库中找到对 TensorZero、Helicone AI Gateway、Arch(archgw)、LangDB、mistral.rs server 的直接一手评价，这是"以哪个项目为基础最快"这个核心问题目前最大的证据空白。**建议**：
   - 对 TensorZero（Apache-2.0，Rust 原生，专门做 LLM 网关+可观测性+实验框架）、Arch(archgw)（Rust/Envoy 数据面 + katanemo/plano 生态）、Helicone AI Gateway（Rust 重写版）三者做一轮直接的 GitHub 代码结构 + issue/PR 活跃度审查，重点看：是否已有"隐私路由/预算路由"这类确定性路由的可扩展点、模型运行时适配器是否易于新增（尤其是 oMLX/mlx_lm 这类小众本地后端）、审计/轨迹记录是否支持"只存元数据 + 可选完整轨迹"这种分级模式；
   - 在此之前，**不建议贸然 fork 任何一个候选项目**——本次调研提供的是"通用工程范式"层面的方向性证据（模块化架构、本地路由、fail-closed、真分词器、按需加载），这些原则在决定"fork 谁"之后同样适用于指导具体实现，但不能替代对候选项目本身适配度的直接评审。

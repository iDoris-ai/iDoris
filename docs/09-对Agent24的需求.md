# iDoris 对 Agent24 的需求（Requirements，非实现）

> 文档类型：跨仓库需求（iDoris → Agent24）
> 日期：2026-07-30 ｜ 维护者：iDoris.ai / @jhfnetboy
>
> **边界声明**：本文只**提需求**，**不从 iDoris 角度修改 Agent24 内核**。ModelRouter 的具体实现、优先级、排期由 **Agent24 自己决定**（Agent24 有自己的 TASKS.md / P4 门）。这里给"iDoris 需要 Agent24 提供什么 + 为什么 + 验收怎样"，实现方式由 Agent24 定。

> 背景：iDoris = 对外一个 OpenAI-compat 本地 URL 的"个人 AI 网关"（三能力，见 [01](./01-统一模型服务-架构设计.md)）。Agent24 内核经 `ModelRouter` 消费它。

---

## R1｜把 iDoris 统一 URL 接为一个 provider（加法，零回归）
- **需求**：`ModelRouter::from_env()` 支持可选 `IDORIS_URL`（+ `IDORIS_API_KEY`），把 iDoris 统一服务当作一个 OpenAI-compat provider 接入；未配置时行为不变。
- **为什么**：iDoris 是 Agent24 之外的独立服务进程；两者靠 REST 进程边界通信（不 import）。
- **建议(非强制)**：locality 判定复用现有 `env_local_tier`（loopback/Tailscale 私网→可视 Local）。
- **验收**：配 `IDORIS_URL` 后 chat/runs 走 iDoris；不配则与今日完全一致；`pnpm gen:api` 无漂移。

## R2｜任务画像经控制面 header 传递（不塞 prompt）
- **需求**：调用 iDoris provider 时，把 `TaskProfile{privacy, complexity, intent, capabilities, fallback}` 映射为 `X-iDoris-*` header（见 [06 §10.5](./06-组件接口契约与互换标准.md)）。
- **为什么**：OpenAI `/v1/chat/completions` 载不动意图/隐私/能力语义；否则消费方会把策略塞进 prompt/model 名，路由变脆（codex-rescue S4）。
- **验收**：iDoris 端能从 header 读到 privacy=local_only 等并据此路由；无 header 时有安全默认（fail_closed）。

## R3｜LocalOnly 隐私语义贯穿 iDoris provider（fail-closed）
- **需求**：`Privacy::LocalOnly` 任务经 iDoris provider 时，必须保证只落到本地后端；iDoris 无本地可用时**报错而非外泄**。
- **为什么**：隐私是契约不是约定（codex-rescue G4）。Agent24 的隐私路由已存在，需覆盖 iDoris provider 这条路径。
- **验收**：LocalOnly + iDoris 无本地 → Unavailable 报错，绝不走 external。

## R4｜硬件感知的模型推荐（动态，可放 ModelRouter 或独立模块）
- **需求**：按当前机器（RAM/芯片/OS）+ 模型目录，自动推荐"常驻核心 + 临时"组合与量化档（算法与标准见 [07](./07-模型量化内存评估与动态推荐.md)）。
- **为什么**：Agent24 分发到 Win/Linux/Mac，硬件差异大；不能写死某模型/某后端（oMLX 仅 Apple）。
- **建议(非强制)**：作为 `ModelRouter` 的 `HardwareProfile + catalog + recommend()` 扩展；或独立 util，ModelRouter 消费其结果。**归属由 Agent24 定**。
- **验收**：24GB profile → 推荐 9B 常驻;64GB → 可上更大;Win/Linux → 选 vLLM/llama.cpp 后端。

## R5｜跨平台后端选择（不绑 oMLX）
- **需求**：capability③ 后端按 OS/硬件探测选：macOS→oMLX、Win/Linux→vLLM/llama.cpp/Ollama；都在 OpenAI-compat + LoadPolicy 契约之后。
- **为什么**：oMLX 属 MLX 生态仅 Apple Silicon；Agent24 要跨平台分发。
- **验收**：同一 Agent24 二进制在三平台都能起本地模型层（后端不同、契约一致）。

## R6｜危险动作照走现有审批门（复用，不新建）
- **需求**：经 iDoris 触发的工具/副作用，仍走 Agent24 既有 C4 审批 + D3 Guardian + 审计链。
- **为什么**：安全性白拿，不为 iDoris 另开绕过路径（同 E4 MCP server 原则）。
- **验收**：iDoris 路径下的危险动作在 daemon 侧照常弹审批。

---

## 非需求（明确不要 Agent24 做）
- 不要求 Agent24 内置 oMLX/任何具体引擎——那是 iDoris/平台后端的事。
- 不要求 Agent24 承载联邦训练——训练是离线进程（Flower+mlx-lm），iDoris/独立进程负责。
- 不要求 Agent24 为 iDoris 破坏性改协议——一切加法迁移（同 H1 经验）。

## 交接方式
本文是 iDoris 侧的需求登记。落到 Agent24，由 Agent24 团队评审后进其 TASKS.md（"iDoris 主 AI 接入"属 Agent24 P4 门后项，需 Agent24 侧用户拍板排期）。iDoris 侧只提供：统一 URL 服务 + 契约 + 动态推荐算法/标准供复用。

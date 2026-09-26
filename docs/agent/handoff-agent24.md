# iDoris → Agent24 交接：R1–R6 接口约定与联调

> 来源：docs/09-对Agent24的需求.md（iDoris 只提需求，不改 Agent24 内核）。
> 本文件是交接件：逐条给出「接口约定 / iDoris 侧现状 / 联调命令 / 验收」。
> 归属与排期由 Agent24 决定（属其 P4 门后「iDoris 主 AI 接入」）。

## 通用边界
- 通信只走进程边界 REST（OpenAI-compat），跨仓不 import。
- 一切加法迁移：未配置 IDORIS_URL 时 Agent24 行为与今日完全一致。

## R1 · 把 iDoris 统一 URL 接为一个 provider（加法，零回归）
- 接口约定：ModelRouter 增加可选 provider 槽 IDORIS_URL（可选 IDORIS_API_KEY）；base URL 即 iDoris 的 OpenAI-compat 根（暴露 /v1/chat/completions、/v1/models）。
- iDoris 侧现状：Router 监听 127.0.0.1:PORT；/v1/models 与 /v1/chat/completions 已实现（T1.3.1/T1.3.4）。
- Agent24 侧：from_env() 加一个 OpenAI-compat 槽；未配置时行为不变；gen:api 无漂移。
- 联调命令：curl -s $IDORIS_URL/v1/models | jq -e '.data|length>0'

## R2 · 任务画像经控制面 header 传递（不塞 prompt）
- 接口约定：X-iDoris-Privacy/Intent/Complexity/Capabilities/Fallback（可选 X-iDoris-Tenant）。缺省 privacy = local_only（保守默认）。
- iDoris 侧现状：parseProfile() 已实现（T1.3.2），非法值 400。
- Agent24 侧：把 TaskProfile 六字段映射到 header；不要把策略塞进 prompt/model 名。
- 联调命令：curl -s $IDORIS_URL/v1/chat/completions -H 'x-idoris-privacy: local_only' -H 'x-idoris-intent: coding' ...

## R3 · LocalOnly 隐私语义贯穿 iDoris provider（fail-closed）
- 接口约定：privacy=local_only 的请求，iDoris 只落到 locality=loopback 且 allowed_egress ⊆ {none,loopback} 的 provider；无候选 → 503 local_only_unavailable，绝不 external。
- iDoris 侧现状：dispatch() + isLocalCapable()（T1.3.3）；pnpm test:privacy 断言 20 条 local_only 无本地 → 20x503 且出站计数为 0。
- Agent24 侧：LocalOnly 任务经 iDoris provider 时不得因「URL 是 loopback」就假定落点；消费 iDoris 的落点语义（设计文档 §4）。
- 联调命令：pnpm test:privacy

## R4 · 硬件感知的模型推荐
- 接口约定：iDoris 提供推荐算法/标准（本仓 docs/07、docs/13、packages/recommender），Agent24 可独立实现或消费。
- iDoris 侧现状：@idoris/recommender（T2.1.1–3）：内存公式、常驻/临时选择、IDORIS_CORE_MODEL override、tradeoff 输出。
- Agent24 侧：归属由 Agent24 定；建议 HardwareProfile + catalog + recommend()。
- 联调命令：pnpm --filter @idoris/recommender test

## R5 · 跨平台后端选择（不绑 oMLX）
- 接口约定：capability3 后端按 OS/硬件探测：macOS→oMLX；Win/Linux+NVIDIA→vLLM；否则 llama.cpp/Ollama。都在 OpenAI-compat + LoadPolicy 契约之后。
- iDoris 侧现状：detectBackend()（T1.2.3）；引擎名只出现在 adapters。
- Agent24 侧：不在内核里写死任何引擎名。
- 联调命令：pnpm --filter @idoris/adapters test

## R6 · 危险动作照走现有审批门（复用，不新建）
- 接口约定：经 iDoris 触发的工具/副作用，仍走 Agent24 既有 C4 审批 + D3 Guardian + 审计链。
- iDoris 侧现状：能力①订阅中转被沙箱化且 fail-closed（T1.4.1）；审计只存元数据、内容黑名单拒绝写入（T2.2.3）。
- Agent24 侧：不为 iDoris 另开绕过路径。
- 联调命令：pnpm test:audit

## 非需求（明确不要 Agent24 做）
不内置任何具体引擎；不承载联邦训练；不为 iDoris 破坏性改协议。

## 联调总命令
- iDoris 侧起服务并发一次本地调用：pnpm smoke:agent24
- 外部槽（有 key 才真跑）：pnpm smoke:external

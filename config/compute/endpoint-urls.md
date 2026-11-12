# 端点 URL 对照

许可表 `endpoint-permit-table.yaml` 里只有 `provider`、`model`、`endpoint` 三个标签。URL 不进那张表。

S4 的供应商配置按 **EndpointKey**（`provider` + `model` + `endpoint`）对应到下表的 URL。不要用 URL 字符串当路由键。

查询日期：2026-11-12。URL 都来自 `docs/design/compute-service-endpoint-table.draft.yaml` 同一天的记录。草案没给出完整 URL、或写明未核对的，下表写「未核实」，不补一个猜出来的地址。

| EndpointKey | URL | 说明 |
|---|---|---|
| `deepinfra` / `deepseek-ai/DeepSeek-V3.2` / `openai-chat-completions` | `POST https://api.deepinfra.com/v1/openai/chat/completions` | 草案原文 |
| `deepinfra` / `deepseek-ai/DeepSeek-V4-Flash` / `openai-chat-completions` | `POST https://api.deepinfra.com/v1/openai/chat/completions` | 草案原文 |
| `deepinfra` / `Qwen/Qwen3.6-35B-A3B` / `openai-chat-completions` | `POST https://api.deepinfra.com/v1/openai/chat/completions` | 草案原文 |
| `openrouter` / `route-policy` / `zdr-only-exclude-safety-retention` | `POST https://openrouter.ai/api/v1/chat/completions` | 草案中间段是账户或请求级策略（ZDR only，且排除 Safety Retention），不是某一个模型名。模型 id 用 `route-policy`。具体模型当前下游未核实 |
| `deepseek-official` / `deepseek-flash` / `chat-completions` | `POST https://api.deepseek.com/chat/completions` | 草案原文。文档版本名 DeepSeek-V4.1-Flash-1017 写在许可表 notes，不另作一个键 |
| `deepseek-official` / `deepseek-v4-pro` / `chat-completions` | `POST https://api.deepseek.com/chat/completions` | 草案原文。文档版本名 DeepSeek-V4-Pro-0813 |
| `together` / `serverless-default` / `prompt-storage-on` | 未核实 | 草案写：OpenAI 兼容基址见供应商文档，本次未核对完整 URL。本行是 Prompt storage 开着的默认，不是 ZDR |
| `fireworks` / `open-models` / `serverless-chat` | 主机名 `api.fireworks.ai`；美国限制时为 `us.api.fireworks.ai`。完整路径未核实 | 草案没有给出路径 |
| `groq` / `unspecified` / `openai-chat-completions` | `POST https://api.groq.com/openai/v1/chat/completions` | 草案把生产与预览型号放在同一行，没有单列模型 id |
| `alibaba-model-studio-intl` / `workspace-default` / `singapore-workspace` | `https://{WorkspaceId}.ap-southeast-1.maas.aliyuncs.com` | 草案给出的主机名。角色模型文档里的 compatible-mode 路径是否覆盖全部聊天型号：未核实 |
| `alibaba-model-studio-intl` / `zdr-adapted` / `singapore-workspace` | 同上，草案没有另给 URL | 只对应留存政策第 4.1 节已适配型号，且工作空间已开通 ZDR。本项目没有这种工作空间 |
| `google-vertex` / `unspecified` / `regional-sg` | 未核实 | 草案只标明区域端点 `asia-southeast1`，没有调用 URL。许可表的区域标签是 `sg`，不是厂商区域名。全球端点不在此键 |
| `siliconflow` / `unspecified` / `inference-api` | 未核实 | 英文站与中文站哪一个适用于国际调用：未核实 |
| `opentyphoon` / `typhoon-v2.5-30b-a3b-instruct` / `chat-completions` | `POST https://api.opentyphoon.ai/v1/chat/completions` | 草案原文。2026-11-12 是否仍提供该模型 ID：未核实 |
| `together` / `scb10x/Typhoon-2-8B-instruct` / `serverless` | 无可用的无服务器 URL | 型号页写该模型不在 Serverless API 上。草案中间段含空格，键里的模型 id 把空格写成 `/`。页面：`https://www.together.ai/models/typhoon-2-8b-instruct` |
| `openrouter` / `scb10x/llama3.1-typhoon2-70b-instruct` / `model-page` | `https://openrouter.ai/scb10x/llama3.1-typhoon2-70b-instruct` | 这是模型页，不是已确认的调用地址。抓取只得到站点外壳 |
| `moonshot` / `unspecified` / `official-api` | 未核实 | 草案写型号与 URL 都未核实 |

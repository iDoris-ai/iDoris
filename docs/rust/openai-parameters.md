# Rust chat 参数支持范围（FU-202）

遵循接口规范 G-6、§3.11：参数必须兑现，否则返回 `400 unsupported_field`。
校验在隐私与角色选择之后、预算预留与加载之前；选定后端的错误带
`X-iDoris-Served-Locality`，所有响应带 `X-iDoris-Record-Id`。

| 参数 | Supervisor（on_demand / evict_to_load） | Resident HTTP 代理 |
| --- | --- | --- |
| `model` | 解析角色或核对具体模型；见 openai-model-identity.md | 原样转发，上游负责支持性校验 |
| `messages` | 转发字符串 `role`、`content`；其他字段或非文本内容显式 400 | 原样转发 |
| `stream` | 仅缺省或 `false`；见 openai-streaming.md | 既有 SSE 路径 |
| `max_tokens`, `max_completion_tokens` | 400 | 原样转发 |
| `temperature`, `top_p`, `stop`, `seed` | 400 | 原样转发 |
| `presence_penalty`, `frequency_penalty`, `logit_bias` | 400 | 原样转发 |
| `n`, `logprobs`, `top_logprobs`, `response_format` | 400 | 原样转发 |
| `tools`, `tool_choice`, `parallel_tool_calls`, `functions`, `function_call` | 400 | 原样转发 |
| `stream_options`, `user`, `store`, `metadata`, `service_tier` | 400 | 原样转发 |
| `reasoning_effort`, `modalities`, `audio`, `prediction` 及其他未列字段 | 400 | 原样转发 |

Supervisor 的 `ChatRequest` 当前仅承载模型和文本消息，所以本次明确拒绝所有采样参数，
包括 `max_tokens: 40`，不再制造限制已生效的假象。即使值为 `null` 或通常的默认值也拒绝。
未来支持参数时须同时扩展 adapter 传输、预算估算和实际上游请求断言，不能只放宽校验。
这份清单描述 Rust 实现；Resident 上游实际支持哪些参数取决于该端点。

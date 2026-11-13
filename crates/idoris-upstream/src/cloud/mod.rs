//! idoris.ai/api 计算服务 **S4：OpenAI 兼容的云上游适配器**。
//!
//! 设计依据：`docs/design/compute-service-design.md` §6（请求状态机与失败切换）、§8（端点级许可表）。
//! 本模块只管**协议与传输**：怎么构造请求、怎么读响应与流、怎么解析 usage、怎么分类失败。
//! 路由、准入、尝试记录、结算由路由层（`idoris-router::cloud`）用 S0 契约类型驱动，这里不依赖契约 crate，
//! 也不知道账本、状态机、许可表。
//!
//! | 模块 | 内容 |
//! |---|---|
//! | [`wire`] | 请求体构造（**白名单**转发客户端字段，`tools` / `tool_choice` / 结构化输出透传，`max_tokens` 与 `model` 强制覆盖，流式固定 `stream_options.include_usage`）与非流式响应解析 |
//! | [`sse`] | 增量 SSE 解析（按事件，限长，文件结束时没完成的事件不算数） |
//! | [`chunk`] | 流里一个事件的含义：`[DONE]`、注释、带 `id` / `usage` / `error` 的 JSON chunk |
//! | [`usage`] | 上游声称的 usage 的判定：结构不对、前后矛盾的一律「无效」，由调用方按「usage 未知」处理 |
//! | [`client`] | HTTP 传输：`prepare`（没有字节离开本机）/ `send`（字节可能已经出去）两步；超时、大小上限、不跟随重定向、密钥不外泄 |
//! | [`failure`] | 失败的稳定代码 |
//! | [`credentials`] | 从文件读密钥（环境变量那种沿用 `remote::EnvCredentialSource`） |
//!
//! # 与既有 `remote::RemoteClient` 的关系
//!
//! 不复用、不修改。`RemoteClient` 建在 `genai` 之上，请求与响应只有 `role` / `content` 文本：没有工具调用，
//! 拿不到 usage，也拿不到供应商请求号，更不能在「连接」与「写字节」之间留出落库的时机。计算服务的结算依赖
//! 这些，所以云上游走这个直接基于 `reqwest` 的适配器。既有路径（本机 oMLX、`RemoteClient`、订阅中转）
//! 一行没改。
//!
//! # 真实供应商
//!
//! 本模块的所有测试都对本地假 HTTP 服务进行，**不连接任何真实的外部推理供应商**。

pub mod chunk;
pub mod client;
pub mod credentials;
pub mod failure;
pub mod sse;
pub mod usage;
pub mod wire;

pub use chunk::{ChunkInfo, ChunkKind, classify};
pub use client::{
    CloudEndpointClient, CloudEndpointSettings, CloudTimeouts, ConfigError, PrepareError,
    PreparedCall, ResponseHead, build_http_client, validate_base_url,
};
pub use credentials::FileCredentialSource;
pub use failure::CloudFailure;
pub use sse::{SseError, SseEvent, SseParser};
pub use usage::{RawUsage, UsageParse, parse_usage};
pub use wire::{
    BuildError, ChatCall, CompletionError, ParsedCompletion, RESERVED_EXTRA_BODY_FIELDS,
    build_chat_body, parse_completion, reserved_extra_body_field,
};

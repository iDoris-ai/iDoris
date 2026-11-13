//! 云上游的 HTTP 传输：一个端点一个 [`CloudEndpointClient`]。
//!
//! # 为什么把一次调用拆成 `prepare` / `send` 两步
//!
//! 契约要求「**先记 `WireWriteStarted` 再写字节**」（`idoris_compute_contracts::attempt`）：执行器在
//! `prepare`（构造请求、取密钥，**还没有任何字节离开本机**，失败是安全的，对应 `NeverLeftHost`）之后、
//! `send` 之前把「可能已经执行」的状态落库，落库成功才调用 `send`。`reqwest` 的 `send` 把连接、写请求、
//! 等响应头合在一个 future 里，没法在「连接成功」和「开始写」之间插入一步，所以这里的保守做法是：
//! **`send` 一开始就按「字节可能已经出去」算**。连接阶段失败（拒绝连接、DNS、TLS）在分类里仍然是
//! [`CloudFailure::ConnectFailed`]，但状态机里不会因此回到 `not_sent`——成本保留待核销，对账时会查无此单后释放
//! （见设计稿 §6、ops §7）。这是有意的保守，不是遗漏。
//!
//! # 安全立场
//!
//! - **不跟随重定向**（`Policy::none()`）：重定向可能把 `Authorization` 带到别的主机；3xx 一律是失败；
//! - **不走代理**（`no_proxy()`），**明确要求不压缩**（`Accept-Encoding: identity`；工作区里别的依赖可能打开了
//!   `reqwest` 的解压特性，不显式写这个头 `reqwest` 会自己加 `gzip, br` 一类，流式事件会被压缩缓冲）；
//! - 只发 `Authorization`、`Content-Type`、`Accept`、`Accept-Encoding`、`User-Agent` 五个头，**不转发客户端的任何头**，
//!   也不带租户、请求 id 一类能关联到用户的信息；
//! - 密钥只在 `prepare` 里取出、只进 `Authorization` 头（标记为 sensitive），不写日志、不进任何错误；
//! - 基址必须是 `https`（仅回环地址允许 `http`，给本机测试用），不能带用户名密码、查询串、片段。
//!
//! 取消：丢弃 [`ResponseHead`]（或 `send` 的 future）就会关闭到上游的连接，上游看到的是客户端断开。

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HeaderName, USER_AGENT,
};
use serde_json::{Map, Value};

use super::failure::CloudFailure;
use super::wire::{BuildError, ChatCall, build_chat_body};
use crate::remote::CredentialSource;

/// 响应体（非流式）默认上限：4 MiB。
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// 单个 SSE 事件默认上限：1 MiB。
pub const DEFAULT_MAX_EVENT_BYTES: usize = 1024 * 1024;

/// 一次调用的期限。执行器按「配置上限」与「整个请求剩下的时间」取较小值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloudTimeouts {
    /// 从开始发送到收到响应头（含连接）。
    pub response_head: Duration,
    /// 非流式：读完整个响应体。
    pub body_total: Duration,
    /// 流式：两个数据块之间的最长静默。
    pub stream_idle: Duration,
}

/// 配置错误（构造客户端时）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("base_url is not a valid URL")]
    InvalidUrl,
    #[error("base_url must use https (plain http is only allowed for loopback hosts)")]
    InsecureScheme,
    #[error("base_url must not carry credentials, a query string or a fragment")]
    UrlHasExtras,
    #[error("invalid supplier request id header name")]
    InvalidHeaderName,
    #[error("failed to build the HTTP client")]
    HttpClient,
}

/// 一个端点的传输配置。
#[derive(Debug, Clone)]
pub struct CloudEndpointSettings {
    /// 例如 `https://api.example.invalid/v1`（不含 `/chat/completions`）。
    pub base_url: String,
    /// 传给 [`CredentialSource::api_key`] 的标签。
    pub provider_label: String,
    /// 从哪个响应头读供应商请求号（小写头名）。
    pub supplier_request_id_header: String,
    /// 端点级固定附加字段（见 `wire` 模块文档）。
    pub extra_body: Map<String, Value>,
    pub max_response_bytes: usize,
}

impl CloudEndpointSettings {
    pub fn new(base_url: impl Into<String>, provider_label: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            provider_label: provider_label.into(),
            supplier_request_id_header: "x-request-id".to_string(),
            extra_body: Map::new(),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }
}

/// 构造云上游共用的 `reqwest::Client`：不跟随重定向、不走代理、给定连接超时。
pub fn build_http_client(connect_timeout: Duration) -> Result<reqwest::Client, ConfigError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(connect_timeout)
        // 空闲连接比常见的网关空闲超时（60s 左右）早关：避免复用已经被对端关掉的连接，
        // 否则一次本可成功的调用会因为「连接在写请求时被重置」而白白失败、触发切换。
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| ConfigError::HttpClient)
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// 校验并规范化基址：返回不带结尾 `/` 的字符串形式。
pub fn validate_base_url(base_url: &str) -> Result<reqwest::Url, ConfigError> {
    let url = reqwest::Url::parse(base_url).map_err(|_| ConfigError::InvalidUrl)?;
    let host = url.host_str().ok_or(ConfigError::InvalidUrl)?;
    match url.scheme() {
        "https" => {}
        "http" if is_loopback_host(host) => {}
        _ => return Err(ConfigError::InsecureScheme),
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::UrlHasExtras);
    }
    Ok(url)
}

/// 一个端点的客户端。
pub struct CloudEndpointClient {
    http: reqwest::Client,
    endpoint_url: reqwest::Url,
    credentials: Arc<dyn CredentialSource>,
    settings: CloudEndpointSettings,
    request_id_header: HeaderName,
}

impl std::fmt::Debug for CloudEndpointClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不含密钥：URL 校验过没有用户信息，凭证来源只在 prepare 里取
        f.debug_struct("CloudEndpointClient")
            .field("provider", &self.settings.provider_label)
            .field("url", &self.endpoint_url.as_str())
            .finish_non_exhaustive()
    }
}

/// `prepare` 的失败：**没有任何字节离开本机**。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PrepareError {
    /// 取不到密钥（来源不可用、文件不存在、格式不对）。
    #[error("upstream credential unavailable")]
    Credential,
    /// 请求体不合法（客户端输入问题）。
    #[error(transparent)]
    Body(#[from] BuildError),
    /// 构造 HTTP 请求失败。
    #[error("could not build the upstream request")]
    Build,
}

impl PrepareError {
    pub fn code(&self) -> &'static str {
        match self {
            PrepareError::Credential => "credential_unavailable",
            PrepareError::Body(_) => "invalid_request_body",
            PrepareError::Build => "request_build_failed",
        }
    }
}

impl CloudEndpointClient {
    pub fn new(
        settings: CloudEndpointSettings,
        credentials: Arc<dyn CredentialSource>,
        http: reqwest::Client,
    ) -> Result<Self, ConfigError> {
        let base = validate_base_url(&settings.base_url)?;
        let mut endpoint_url = base;
        let path = format!(
            "{}/chat/completions",
            endpoint_url.path().trim_end_matches('/')
        );
        endpoint_url.set_path(&path);
        let request_id_header =
            HeaderName::from_bytes(settings.supplier_request_id_header.as_bytes())
                .map_err(|_| ConfigError::InvalidHeaderName)?;
        Ok(Self {
            http,
            endpoint_url,
            credentials,
            settings,
            request_id_header,
        })
    }

    pub fn settings(&self) -> &CloudEndpointSettings {
        &self.settings
    }

    /// 构造请求并取密钥。**失败时没有任何字节离开本机。** 成功后调用方先把状态落库，再 [`PreparedCall::send`]。
    pub async fn prepare(
        &self,
        call: &ChatCall<'_>,
        timeouts: CloudTimeouts,
    ) -> Result<PreparedCall, PrepareError> {
        let body = build_chat_body(&ChatCall {
            extra_body: &self.settings.extra_body,
            ..*call
        })?;
        let key = self
            .credentials
            .api_key(&self.settings.provider_label)
            .await
            .map_err(|_| PrepareError::Credential)?;
        let request = self
            .http
            .post(self.endpoint_url.clone())
            .bearer_auth(key)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT_ENCODING, "identity")
            .header(
                ACCEPT,
                if call.stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .header(
                USER_AGENT,
                concat!("idoris-cloud/", env!("CARGO_PKG_VERSION")),
            )
            .body(body)
            .build()
            .map_err(|_| PrepareError::Build)?;
        Ok(PreparedCall {
            http: self.http.clone(),
            request,
            timeouts,
            request_id_header: self.request_id_header.clone(),
            max_response_bytes: self.settings.max_response_bytes,
        })
    }
}

/// 已经构造好、还没有发送的调用。
pub struct PreparedCall {
    http: reqwest::Client,
    request: reqwest::Request,
    timeouts: CloudTimeouts,
    request_id_header: HeaderName,
    max_response_bytes: usize,
}

impl std::fmt::Debug for PreparedCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 请求里有 Authorization：不打印请求本身
        f.debug_struct("PreparedCall")
            .field("url", &self.request.url().as_str())
            .finish_non_exhaustive()
    }
}

fn classify_send_error(error: &reqwest::Error) -> CloudFailure {
    if error.is_connect() {
        if error.is_timeout() {
            CloudFailure::ConnectTimeout
        } else {
            CloudFailure::ConnectFailed
        }
    } else if error.is_timeout() {
        CloudFailure::ResponseHeadTimeout
    } else {
        CloudFailure::SendFailed
    }
}

impl PreparedCall {
    /// 发送并等响应头。**从这一刻起字节可能已经离开本机。** 任何状态码都返回 `Ok`（非 2xx 由调用方按
    /// [`ResponseHead::status`] 处理）；传输层失败返回 [`CloudFailure`]。
    pub async fn send(self) -> Result<ResponseHead, CloudFailure> {
        let PreparedCall {
            http,
            request,
            timeouts,
            request_id_header,
            max_response_bytes,
        } = self;
        match tokio::time::timeout(timeouts.response_head, http.execute(request)).await {
            Err(_) => Err(CloudFailure::ResponseHeadTimeout),
            Ok(Err(error)) => Err(classify_send_error(&error)),
            Ok(Ok(response)) => {
                let header_text = |name: &HeaderName| {
                    response
                        .headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .filter(|v| !v.is_empty() && v.len() <= 256)
                        .map(str::to_owned)
                };
                Ok(ResponseHead {
                    status: response.status().as_u16(),
                    content_type: response
                        .headers()
                        .get(CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned),
                    supplier_request_id_header: header_text(&request_id_header),
                    declared_length: response
                        .headers()
                        .get(CONTENT_LENGTH)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok()),
                    response,
                    timeouts,
                    max_response_bytes,
                })
            }
        }
    }
}

/// 响应头（以及还没读的响应体）。丢弃它就是取消：连接关闭，上游看到客户端断开。
pub struct ResponseHead {
    pub status: u16,
    pub content_type: Option<String>,
    /// 配置的头里的供应商请求号（原文，已限长）。
    pub supplier_request_id_header: Option<String>,
    declared_length: Option<u64>,
    response: reqwest::Response,
    timeouts: CloudTimeouts,
    max_response_bytes: usize,
}

impl std::fmt::Debug for ResponseHead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseHead")
            .field("status", &self.status)
            .field("content_type", &self.content_type)
            .finish_non_exhaustive()
    }
}

impl ResponseHead {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 内容类型是 `text/event-stream`。
    pub fn is_event_stream(&self) -> bool {
        self.content_type.as_deref().is_some_and(|ct| {
            ct.trim_start()
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
        })
    }

    /// 读完整个响应体（非流式）：有总时限，有大小上限。
    ///
    /// 时限是 [`CloudTimeouts::body_total`]，从**调用这个方法时**起算（响应头已经到了）。如果调用方还有一个更大范围的
    /// 总期限（一个请求含所有切换的总期限），用 [`Self::read_body_until`]：否则「响应头时间 + 响应体时间」会超过总期限。
    pub async fn read_body(self) -> Result<Vec<u8>, CloudFailure> {
        self.read_body_until(None).await
    }

    /// 同 [`Self::read_body`]，另外受一个**绝对期限**约束：读正文的时限是 `min(body_total, 距 deadline 还剩多少)`，
    /// 用**读正文这一刻**的剩余时间算（不是发送之前算好的）。
    /// 一个请求的总期限 120 秒，响应头第 29 秒才回来，正文最多还能读 91 秒，不是重新开始的 120 秒。
    pub async fn read_body_until(
        mut self,
        deadline: Option<std::time::Instant>,
    ) -> Result<Vec<u8>, CloudFailure> {
        if self
            .declared_length
            .is_some_and(|len| len > self.max_response_bytes as u64)
        {
            return Err(CloudFailure::BodyTooLarge);
        }
        let limit = match deadline {
            Some(deadline) => self
                .timeouts
                .body_total
                .min(deadline.saturating_duration_since(std::time::Instant::now())),
            None => self.timeouts.body_total,
        };
        let max = self.max_response_bytes;
        let read = async {
            let mut body = Vec::new();
            loop {
                match self.response.chunk().await {
                    Ok(Some(chunk)) => {
                        if body.len() + chunk.len() > max {
                            return Err(CloudFailure::BodyTooLarge);
                        }
                        body.extend_from_slice(&chunk);
                    }
                    Ok(None) => return Ok(body),
                    Err(_) => return Err(CloudFailure::BodyReadFailed),
                }
            }
        };
        match tokio::time::timeout(limit, read).await {
            Ok(result) => result,
            Err(_) => Err(CloudFailure::BodyTimeout),
        }
    }

    /// 流式：读下一个数据块。`Ok(None)` 是流结束（**不等于完成**：有没有 `[DONE]` 由调用方判断）。
    /// 两块之间静默超过 `stream_idle` 返回 [`CloudFailure::StreamIdleTimeout`]。
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, CloudFailure> {
        match tokio::time::timeout(self.timeouts.stream_idle, self.response.chunk()).await {
            Err(_) => Err(CloudFailure::StreamIdleTimeout),
            Ok(Err(_)) => Err(CloudFailure::StreamTruncated),
            Ok(Ok(chunk)) => Ok(chunk.map(|c| c.to_vec())),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn base_url_rules() {
        for ok in [
            "https://api.example.invalid/v1",
            "https://api.example.invalid/v1/",
            "https://api.example.invalid",
            "http://127.0.0.1:8088/v1",
            "http://localhost:9/v1",
            "http://[::1]:9/v1",
        ] {
            assert!(validate_base_url(ok).is_ok(), "{ok}");
        }
        for (bad, why) in [
            ("not a url", ConfigError::InvalidUrl),
            ("api.example.invalid/v1", ConfigError::InvalidUrl),
            ("http://api.example.invalid/v1", ConfigError::InsecureScheme),
            (
                "http://127.0.0.1.example.invalid/v1",
                ConfigError::InsecureScheme,
            ),
            ("ftp://api.example.invalid/v1", ConfigError::InsecureScheme),
            (
                "https://user:pass@api.example.invalid/v1",
                ConfigError::UrlHasExtras,
            ),
            (
                "https://user@api.example.invalid/v1",
                ConfigError::UrlHasExtras,
            ),
            (
                "https://api.example.invalid/v1?key=1",
                ConfigError::UrlHasExtras,
            ),
            (
                "https://api.example.invalid/v1#frag",
                ConfigError::UrlHasExtras,
            ),
        ] {
            assert_eq!(validate_base_url(bad).unwrap_err(), why, "{bad}");
        }
    }

    struct FixedKey;

    #[async_trait::async_trait]
    impl CredentialSource for FixedKey {
        async fn api_key(&self, _provider: &str) -> Result<String, crate::UpstreamError> {
            Ok("sk-sentinel-never-print".to_string())
        }
    }

    fn client(base: &str) -> CloudEndpointClient {
        CloudEndpointClient::new(
            CloudEndpointSettings::new(base, "example-provider"),
            Arc::new(FixedKey),
            build_http_client(Duration::from_secs(1)).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn the_endpoint_url_is_the_base_plus_chat_completions_whatever_the_trailing_slash() {
        for base in [
            "https://h.example.invalid/v1",
            "https://h.example.invalid/v1/",
        ] {
            assert_eq!(
                client(base).endpoint_url.as_str(),
                "https://h.example.invalid/v1/chat/completions"
            );
        }
        assert_eq!(
            client("https://h.example.invalid").endpoint_url.as_str(),
            "https://h.example.invalid/chat/completions"
        );
    }

    #[test]
    fn a_bad_header_name_is_a_config_error() {
        let mut settings = CloudEndpointSettings::new("https://h.example.invalid/v1", "p");
        settings.supplier_request_id_header = "bad header".into();
        let err = CloudEndpointClient::new(
            settings,
            Arc::new(FixedKey),
            build_http_client(Duration::from_secs(1)).unwrap(),
        )
        .unwrap_err();
        assert_eq!(err, ConfigError::InvalidHeaderName);
    }

    #[tokio::test]
    async fn debug_output_never_contains_the_key_or_the_request() {
        let client = client("https://h.example.invalid/v1");
        let body = serde_json::json!({"messages": [{"role": "user", "content": "SECRET PROMPT"}]});
        let extra = Map::new();
        let prepared = client
            .prepare(
                &ChatCall {
                    client_body: body.as_object().unwrap(),
                    upstream_model: "m",
                    max_tokens: 8,
                    stream: false,
                    extra_body: &extra,
                },
                CloudTimeouts {
                    response_head: Duration::from_secs(1),
                    body_total: Duration::from_secs(1),
                    stream_idle: Duration::from_secs(1),
                },
            )
            .await
            .unwrap();
        for text in [format!("{client:?}"), format!("{prepared:?}")] {
            assert!(!text.contains("sk-sentinel"), "{text}");
            assert!(!text.contains("SECRET PROMPT"), "{text}");
        }
    }

    #[tokio::test]
    async fn a_credential_failure_is_a_prepare_error_with_no_detail() {
        struct NoKey;
        #[async_trait::async_trait]
        impl CredentialSource for NoKey {
            async fn api_key(&self, _provider: &str) -> Result<String, crate::UpstreamError> {
                Err(crate::UpstreamError::internal("secret path /etc/keys/prod"))
            }
        }
        let client = CloudEndpointClient::new(
            CloudEndpointSettings::new("https://h.example.invalid/v1", "p"),
            Arc::new(NoKey),
            build_http_client(Duration::from_secs(1)).unwrap(),
        )
        .unwrap();
        let body = serde_json::json!({"messages": [{"role": "user", "content": "x"}]});
        let extra = Map::new();
        let err = client
            .prepare(
                &ChatCall {
                    client_body: body.as_object().unwrap(),
                    upstream_model: "m",
                    max_tokens: 8,
                    stream: false,
                    extra_body: &extra,
                },
                CloudTimeouts {
                    response_head: Duration::from_secs(1),
                    body_total: Duration::from_secs(1),
                    stream_idle: Duration::from_secs(1),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err, PrepareError::Credential);
        assert!(!err.to_string().contains("/etc/keys"));
        assert_eq!(err.code(), "credential_unavailable");
    }
}

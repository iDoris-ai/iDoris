//! 从请求和上游响应里取出**要检查的文本**，并为 edu 重建发给客户端的响应体。
//!
//! # 输入：检查的就是「真正会送到模型的所有文本」
//!
//! 上游请求体由 `idoris_upstream::cloud::wire::build_chat_body` 用**白名单**构造（[`FORWARDED_FIELDS`]）。
//! 这里**用同一个函数构造一份同样的请求**，再把里面客户端写的每一个字符串——**值和键名**，任何层级——都取出来
//! （[`forwarded_view`]）：检查的和转发的是同一份东西，不靠「我们认为哪些字段会被转发」的另一张表。
//! 将来有人给 `build_chat_body` 加字段、改形状，这里自动跟着变。性质测试
//! `every_string_in_the_forwarded_request_is_in_the_checked_text` 用随机的请求体证明：转发请求里的每一个字符串都在检查内容里。
//!
//! 取文本时还做三件事（r3 的 High#2 / High#4）：
//!
//! - **键名也是文本**：工具 schema 的属性名、消息对象里的任意额外键，模型都读得到；
//! - **看起来是 JSON 的字符串解码之后再取一遍**（`{"text":"kill yourself"}` 里的转义）；
//! - **`arguments` 必须是合法的 JSON**，解析不了就是 [`ExtractError::Malformed`]（失败即关闭），历史消息里的工具调用参数同样。
//!
//! # 来源：客户端自己挣不到豁免
//!
//! 每条消息按**来源**分类（[`Origin`]），规则检查器按来源用不同的规则集：
//!
//! | 来源 | 谁 | 为什么分开 |
//! |---|---|---|
//! | `User` | `user`、**`system`、`developer`**、未知角色、`tools` / `tool_choice` / `response_format` / `stop` 和其它一切客户端写的文本 | 孩子写的话，最严。**客户端填的角色不证明这段话来自运营方**（入口只认证了凭证，成员凭证也能发 `system`） |
//! | `Tool` | `tool`、`function` | 工具返回的引用内容（可能是网页摘录），不可信，最严 |
//! | `Assistant` | `assistant` | 对话历史里模型说过的话，按**输出**规则查（拒绝语里会提到有害词，不能把拒绝语当成有害请求） |
//! | `Operator` | **只有服务端配置里登记的运营方提示**（[`TrustedOperatorText`]） | 运营方的系统提示会复述要拒绝的话题（「如果孩子要求自伤……就拒绝」），只查越狱类规则 |
//!
//! 运营方内容只能来自服务端受信配置：`system` / `developer` 消息的内容**以**某条登记的提示**开头**时，这一段开头算运营方的，
//! 后面跟着的（比如 `ช่วงชั้นของผู้เรียน: 初中`）仍然按孩子写的话检查；不以登记的提示开头的，整条都按孩子写的话。
//! 没有登记任何提示（默认）时，客户端写的内容没有任何一个字节能得到豁免。
//!
//! 非文本内容（图片、音频、文件……）规则检查器无法检查：置 [`CheckedInput::has_uncheckable_content`]，
//! 由 gate 按失败即关闭拒绝。语音转写是 P1，不在这里。
//!
//! # 输出：重建，而不是原样转发
//!
//! 非 edu 的响应体原样交给客户端（S4 的行为，一字不变）。edu 的**不是**：上游的响应里有我们没有检查的文本字段
//! （`reasoning_content`、`annotations`、`provider`……），原样转发就等于把没检查的字节交给孩子。
//! [`rebuild_response`] 只取 `choices[0].message` 的 `content`、`refusal`、`tool_calls[].function.{name,arguments}`
//! ——这些全部会被检查——再加上只含数字的 `usage` 和词表里的 `finish_reason`，重新序列化。
//! **响应 `id`、`model`、工具调用 `id` 都不取上游的**（r3 Medium#1）：上游能把孩子写的电话号码回显成一个合法的「短标识」，
//! 它们不在被检查的文本里，会绕过 PII 输出检查。`id` 由服务端生成（请求号），`model` 是已选路由的模型，工具调用 `id` 从请求号派生。
//! **客户端收到的每一个字符串，要么是被检查过的文本，要么是服务端自己写的。**

use std::borrow::Cow;
use std::sync::Arc;

use idoris_upstream::cloud::wire::{ChatCall, FORWARDED_FIELDS, build_chat_body};
use serde_json::{Map, Value, json};

use super::text::Sensitive;

/// 一段文本的来源（决定规则检查器用哪套规则）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    User,
    Tool,
    Operator,
    Assistant,
}

/// 一段要检查的文本。
#[derive(Debug, Clone)]
pub struct Segment {
    pub origin: Origin,
    pub text: Sensitive,
}

/// 一个请求里所有要检查的文本。
#[derive(Debug, Clone, Default)]
pub struct CheckedInput {
    pub segments: Vec<Segment>,
    /// 请求里有无法检查的非文本内容（图片、音频、文件……）。
    pub has_uncheckable_content: bool,
    /// 最后一条孩子说的话（`user` 或未知角色的消息）在 `segments` 里的位置。`system` 消息、`tools`、`stop` 现在也是
    /// `Origin::User`（客户端写的内容一律按孩子写的话检查），所以「最后一个 `Origin::User` 的片段」不再等于孩子最后一句话。
    pub last_user_segment: Option<usize>,
}

impl CheckedInput {
    pub fn total_bytes(&self) -> usize {
        self.segments.iter().map(|s| s.text.len()).sum()
    }

    /// 孩子和工具写的文本（最严的那两种来源）。
    pub fn untrusted_texts(&self) -> impl Iterator<Item = &str> {
        self.segments
            .iter()
            .filter(|s| matches!(s.origin, Origin::User | Origin::Tool))
            .map(|s| s.text.expose())
    }

    /// 最后一条用户消息（给 LLM 检查器当输出检查的上下文）。
    pub fn last_user_text(&self) -> Option<&str> {
        self.last_user_segment
            .and_then(|index| self.segments.get(index))
            .map(|s| s.text.expose())
    }
}

/// 一次工具调用里会被检查的文本。
#[derive(Debug, Clone)]
pub struct ToolCallText {
    pub name: Sensitive,
    /// 原样转发给客户端的 `arguments`（JSON 文本）。检查的是它解码之后的键和值，见 [`ToolCallText::decoded_arguments`]。
    pub arguments: Sensitive,
}

impl ToolCallText {
    /// `arguments` 解析成 JSON 之后的每一个键和字符串值（任何层级；字符串值本身又是 JSON 的，再解一层）。
    /// 规则只看得到 JSON 文本的字面（`k`），客户端解析之后看到的是解码之后的字。解析不了（或者套得太深）返回空：
    /// 收到上游响应时已经按失败即关闭拒绝过这种参数，见 [`rebuild_response`]。
    pub fn decoded_arguments(&self) -> Vec<String> {
        decoded_pieces(self.arguments.expose()).unwrap_or_default()
    }

    /// 给 LLM 检查器看的参数文本：解码之后的键和值各占一行；解码不出东西就用原文。
    pub fn judge_text(&self) -> String {
        let pieces = self.decoded_arguments();
        if pieces.is_empty() {
            self.arguments.expose().to_string()
        } else {
            pieces.join("\n")
        }
    }
}

/// 模型回答里所有要检查的文本。
#[derive(Debug, Clone, Default)]
pub struct CheckedOutput {
    pub content: Option<Sensitive>,
    pub refusal: Option<Sensitive>,
    pub tool_calls: Vec<ToolCallText>,
}

impl CheckedOutput {
    /// 回答、拒绝语、工具名和参数的原文。
    pub fn texts(&self) -> Vec<&str> {
        let mut texts = Vec::new();
        if let Some(content) = &self.content {
            texts.push(content.expose());
        }
        if let Some(refusal) = &self.refusal {
            texts.push(refusal.expose());
        }
        for call in &self.tool_calls {
            texts.push(call.name.expose());
            texts.push(call.arguments.expose());
        }
        texts
    }

    /// 全部要检查的文本：[`texts`](Self::texts) 加上工具参数解码之后的每个键和值。规则检查器看这一份。
    pub fn all_texts(&self) -> Vec<Cow<'_, str>> {
        let mut texts: Vec<Cow<'_, str>> = Vec::new();
        if let Some(content) = &self.content {
            texts.push(Cow::Borrowed(content.expose()));
        }
        if let Some(refusal) = &self.refusal {
            texts.push(Cow::Borrowed(refusal.expose()));
        }
        for call in &self.tool_calls {
            texts.push(Cow::Borrowed(call.name.expose()));
            texts.push(Cow::Borrowed(call.arguments.expose()));
            texts.extend(call.decoded_arguments().into_iter().map(Cow::Owned));
        }
        texts
    }

    pub fn total_bytes(&self) -> usize {
        self.texts().iter().map(|t| t.len()).sum()
    }
}

/// 取文本失败：请求或响应的结构不符合约定，没法可靠地取出要检查的内容。失败即关闭。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractError {
    Malformed,
    TooLarge,
}

/// 递归取串的最大深度。
const MAX_DEPTH: usize = 48;

/// 一个字符串本身又是 JSON（`arguments`、被双重编码的内容）时，解开它要多花多少「深度」：
/// 一共最多解 `MAX_DEPTH / NESTED_JSON_COST` = 4 层（再多的不是正常的内容，按 `Malformed` 拒绝）。
const NESTED_JSON_COST: usize = 12;

/// 消息里不是客户端文本的固定结构：角色名（词表是封闭的）。它们装不下任何内容，不当文本取。
const KNOWN_ROLES: &[&str] = &[
    "system",
    "developer",
    "user",
    "assistant",
    "tool",
    "function",
];

/// `build_chat_body` 自己写进去的、不是客户端的字段（模型、上限、流式开关由服务端决定）。
const SERVER_SET_FIELDS: &[&str] = &["model", "max_tokens", "stream", "stream_options"];

// ---------------------------------------------------------------------------
// 服务端受信的运营方提示
// ---------------------------------------------------------------------------

/// 登记的运营方提示至少多少个字符：太短的片段（「你是」）相当于通配符，任何消息都能以它开头。
pub const MIN_TRUSTED_PROMPT_CHARS: usize = 32;
/// 单条登记的运营方提示最多多少字节。
pub const MAX_TRUSTED_PROMPT_BYTES: usize = 16 * 1024;
/// 最多登记多少条。
pub const MAX_TRUSTED_PROMPTS: usize = 64;

/// 服务端配置里登记的、受信的运营方提示（`safety.trusted_operator_prompts`）。
///
/// 只有这里登记过的文本能得到运营方的待遇（只查越狱类规则）；客户端填的 `role`、凭证的种类、请求头都不能。
/// 匹配是**逐字、只认开头**：一条 `system` / `developer` 消息的内容以某条登记的提示开头，这一段开头是运营方的，
/// 后面的部分仍然是孩子写的话。只认开头（不认中间、结尾）是为了不让人拿登记的提示当分隔符把有害的话拆开。
#[derive(Clone, Default)]
pub struct TrustedOperatorText {
    /// 按长度从长到短排好：开头匹配取最长的那条。
    prompts: Arc<Vec<String>>,
}

impl std::fmt::Debug for TrustedOperatorText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustedOperatorText")
            .field("prompts", &self.prompts.len())
            .finish()
    }
}

impl TrustedOperatorText {
    /// 没有登记任何提示：客户端写的内容没有任何一个字节能得到豁免。
    pub fn none() -> Self {
        Self::default()
    }

    /// 校验并登记。每条至少 [`MIN_TRUSTED_PROMPT_CHARS`] 个字符、至多 [`MAX_TRUSTED_PROMPT_BYTES`] 字节，不重复，
    /// 最多 [`MAX_TRUSTED_PROMPTS`] 条。错误信息里没有提示的内容。
    pub fn new(prompts: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut list: Vec<String> = Vec::new();
        for (index, prompt) in prompts.into_iter().enumerate() {
            let position = index + 1;
            if prompt.chars().count() < MIN_TRUSTED_PROMPT_CHARS {
                return Err(format!(
                    "第 {position} 条受信的运营方提示太短（至少 {MIN_TRUSTED_PROMPT_CHARS} 个字符）：太短的片段任何消息都能以它开头，等于没有检查"
                ));
            }
            if prompt.len() > MAX_TRUSTED_PROMPT_BYTES {
                return Err(format!(
                    "第 {position} 条受信的运营方提示太长（最多 {MAX_TRUSTED_PROMPT_BYTES} 字节）"
                ));
            }
            if list.contains(&prompt) {
                return Err(format!("第 {position} 条受信的运营方提示重复了"));
            }
            list.push(prompt);
        }
        if list.len() > MAX_TRUSTED_PROMPTS {
            return Err(format!(
                "最多登记 {MAX_TRUSTED_PROMPTS} 条受信的运营方提示，现在有 {}",
                list.len()
            ));
        }
        list.sort_by_key(|prompt| std::cmp::Reverse(prompt.len()));
        Ok(Self {
            prompts: Arc::new(list),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.prompts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.prompts.len()
    }

    /// 登记的提示（长的在前）。
    pub fn prompts(&self) -> impl Iterator<Item = &str> {
        self.prompts.iter().map(String::as_str)
    }

    /// `text` 以某条登记的提示开头：返回 `(那条提示, 后面剩下的)`。
    fn strip_prefix<'a>(&self, text: &'a str) -> Option<(&'a str, &'a str)> {
        self.prompts
            .iter()
            .find(|prompt| text.starts_with(prompt.as_str()))
            .map(|prompt| text.split_at(prompt.len()))
    }
}

// ---------------------------------------------------------------------------
// 输入
// ---------------------------------------------------------------------------

/// 一个字符串本身看起来是 JSON 对象或数组时，把它解码之后的键和值也取出来。解析不了就只是文本。
fn decode_nested_json(text: &str, out: &mut Vec<String>, depth: usize) -> Result<(), ExtractError> {
    let trimmed = text.trim_start();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return Ok(());
    }
    let parsed = match serde_json::from_str::<Value>(text) {
        Ok(parsed) => parsed,
        // 嵌套深到 serde_json 自己都不肯解析（超过 128 层）：我们看不到里面的东西，客户端 / 模型未必也看不到，失败即关闭
        Err(error) if error.to_string().contains("recursion limit exceeded") => {
            return Err(ExtractError::Malformed);
        }
        // 只是恰好以 `{` / `[` 开头的普通文本
        Err(_) => return Ok(()),
    };
    collect_value(&parsed, out, depth.saturating_add(NESTED_JSON_COST))
}

/// `arguments`：必须是合法的 JSON 文本（空白算「没有参数」）。原文先取，再取解码之后的键和值。解析不了是 `Malformed`。
fn collect_arguments(text: &str, out: &mut Vec<String>, depth: usize) -> Result<(), ExtractError> {
    out.push(text.to_string());
    if text.trim().is_empty() {
        return Ok(());
    }
    let parsed: Value = serde_json::from_str(text).map_err(|_| ExtractError::Malformed)?;
    collect_value(&parsed, out, depth.saturating_add(NESTED_JSON_COST))
}

/// 递归取出一个 JSON 值里的每一个字符串：值，**还有键名**；`arguments` 要能解析，看起来是 JSON 的字符串再解一层。
fn collect_value(value: &Value, out: &mut Vec<String>, depth: usize) -> Result<(), ExtractError> {
    if depth > MAX_DEPTH {
        return Err(ExtractError::Malformed);
    }
    match value {
        Value::String(text) => {
            out.push(text.clone());
            decode_nested_json(text, out, depth)?;
        }
        Value::Array(items) => {
            for item in items {
                collect_value(item, out, depth + 1)?;
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                out.push(key.clone());
                match item {
                    Value::String(text) if key == "arguments" => {
                        collect_arguments(text, out, depth + 1)?;
                    }
                    _ => collect_value(item, out, depth + 1)?,
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

/// 一段 JSON 文本解析之后的每一个键和字符串值（不含原文）。
fn decoded_pieces(raw: &str) -> Result<Vec<String>, ExtractError> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let parsed: Value = serde_json::from_str(raw).map_err(|_| ExtractError::Malformed)?;
    let mut out = Vec::new();
    collect_value(&parsed, &mut out, NESTED_JSON_COST)?;
    Ok(out)
}

fn origin_of(role: &str) -> Origin {
    match role {
        "assistant" => Origin::Assistant,
        "tool" | "function" => Origin::Tool,
        // `user`、**`system` 和 `developer`**、任何未知角色：最严。角色是客户端填的，不证明这段话来自运营方。
        _ => Origin::User,
    }
}

/// 一段内容文本：开头是登记过的运营方提示的话，这一段开头归运营方，后面的按孩子写的话；其余全是孩子写的话。
fn push_text(
    text: &str,
    trusted: Option<&TrustedOperatorText>,
    pieces: &mut Vec<String>,
    operator: &mut Vec<String>,
) -> Result<(), ExtractError> {
    let rest = match trusted.and_then(|trusted| trusted.strip_prefix(text)) {
        Some((prompt, rest)) => {
            operator.push(prompt.to_string());
            rest
        }
        None => text,
    };
    if rest.is_empty() {
        return Ok(());
    }
    pieces.push(rest.to_string());
    decode_nested_json(rest, pieces, 0)
}

/// `content`：字符串，或分段数组（只有 `text` 分段能检查）。分段里的其它键和值同样会被转发，所以同样取出来。
fn collect_content(
    content: &Value,
    trusted: Option<&TrustedOperatorText>,
    pieces: &mut Vec<String>,
    operator: &mut Vec<String>,
    uncheckable: &mut bool,
) -> Result<(), ExtractError> {
    match content {
        Value::Null => Ok(()),
        Value::String(text) => push_text(text, trusted, pieces, operator),
        Value::Array(parts) => {
            for part in parts {
                match part {
                    Value::String(text) => push_text(text, trusted, pieces, operator)?,
                    Value::Object(object) => match object.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            match object.get("text") {
                                Some(Value::String(text)) => {
                                    push_text(text, trusted, pieces, operator)?;
                                }
                                _ => return Err(ExtractError::Malformed),
                            }
                            for (key, value) in object {
                                if key != "type" && key != "text" {
                                    pieces.push(key.clone());
                                    collect_value(value, pieces, 1)?;
                                }
                            }
                        }
                        _ => *uncheckable = true,
                    },
                    _ => return Err(ExtractError::Malformed),
                }
            }
            Ok(())
        }
        _ => Err(ExtractError::Malformed),
    }
}

/// 客户端请求体里**会被转发**的那一部分：用真正构造上游请求的 `build_chat_body` 构造一份（模型、上限换成占位值，
/// 端点级固定字段不放进来：那是运营方写的，不是客户端的），再去掉服务端自己写的字段。
///
/// 构造不出来（字段类型不对）说明这个请求不会被转发，执行器会在发出任何字节之前拒绝它；
/// 这时仍然按白名单把客户端写的字段都过一遍，让检查先于那个拒绝，结论和以前一样。
pub fn forwarded_view(client: &Map<String, Value>) -> Map<String, Value> {
    let no_extra = Map::new();
    let probe = ChatCall {
        client_body: client,
        upstream_model: "probe",
        max_tokens: 1,
        stream: false,
        extra_body: &no_extra,
    };
    if let Ok(bytes) = build_chat_body(&probe)
        && let Ok(Value::Object(mut map)) = serde_json::from_slice::<Value>(&bytes)
    {
        for key in SERVER_SET_FIELDS {
            map.remove(*key);
        }
        return map;
    }
    FORWARDED_FIELDS
        .iter()
        .filter_map(|field| {
            client
                .get(*field)
                .filter(|value| !value.is_null())
                .map(|value| ((*field).to_string(), value.clone()))
        })
        .collect()
}

/// 从客户端请求体里取出所有要检查的文本。`max_bytes` 是文本总量上限（超过返回 `TooLarge`）。
/// 没有登记任何受信的运营方提示：客户端写的内容一律按孩子写的话检查。
pub fn extract_input(
    body: &Map<String, Value>,
    max_bytes: usize,
) -> Result<CheckedInput, ExtractError> {
    extract_input_with(body, max_bytes, &TrustedOperatorText::none())
}

/// 同 [`extract_input`]，另外认服务端登记的运营方提示（`system` / `developer` 消息的内容以它开头的那一段归运营方）。
pub fn extract_input_with(
    body: &Map<String, Value>,
    max_bytes: usize,
    trusted: &TrustedOperatorText,
) -> Result<CheckedInput, ExtractError> {
    extract_view(&forwarded_view(body), max_bytes, trusted)
}

/// 从「会被转发的那份请求」（[`forwarded_view`]）里取出要检查的文本。
fn extract_view(
    forwarded: &Map<String, Value>,
    max_bytes: usize,
    trusted: &TrustedOperatorText,
) -> Result<CheckedInput, ExtractError> {
    let mut input = CheckedInput::default();
    let mut total = 0usize;

    let messages = forwarded
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(ExtractError::Malformed)?;
    for message in messages {
        let object = message.as_object().ok_or(ExtractError::Malformed)?;
        let role = object
            .get("role")
            .and_then(Value::as_str)
            .ok_or(ExtractError::Malformed)?;
        let trusted_here = matches!(role, "system" | "developer").then_some(trusted);
        let mut pieces: Vec<String> = Vec::new();
        let mut operator: Vec<String> = Vec::new();
        if !KNOWN_ROLES.contains(&role) {
            // 未知角色的名字也会被转发
            pieces.push(role.to_string());
        }
        for (key, value) in object {
            match key.as_str() {
                "role" => {}
                "content" => collect_content(
                    value,
                    trusted_here,
                    &mut pieces,
                    &mut operator,
                    &mut input.has_uncheckable_content,
                )?,
                _ => {
                    pieces.push(key.clone());
                    match value {
                        Value::String(text) if key == "arguments" => {
                            collect_arguments(text, &mut pieces, 0)?;
                        }
                        _ => collect_value(value, &mut pieces, 0)?,
                    }
                }
            }
        }
        for (origin, texts) in [(Origin::Operator, operator), (origin_of(role), pieces)] {
            if texts.is_empty() {
                continue;
            }
            let text = texts.join("\n");
            total = total.saturating_add(text.len());
            if total > max_bytes {
                return Err(ExtractError::TooLarge);
            }
            if origin == Origin::User && !matches!(role, "system" | "developer") {
                input.last_user_segment = Some(input.segments.len());
            }
            input.segments.push(Segment {
                origin,
                text: Sensitive::new(text),
            });
        }
    }

    // 其余转发出去的字段（`tools`、`tool_choice`、`response_format`、`stop`、数字和布尔……）：
    // **转发的请求里有什么字段就取什么字段**（不是白名单里有什么：将来 `build_chat_body` 多转发一个字段，这里不用改也不会漏），
    // 顺序固定（白名单的顺序在前，其余按名字排序），每个字段里的每一个键和值。客户端写的，一律按孩子写的话。
    let mut fields: Vec<&String> = forwarded
        .keys()
        .filter(|key| key.as_str() != "messages")
        .collect();
    fields.sort_by_key(|key| {
        (
            FORWARDED_FIELDS
                .iter()
                .position(|field| *field == key.as_str())
                .unwrap_or(usize::MAX),
            (*key).clone(),
        )
    });
    for field in fields {
        let Some(value) = forwarded.get(field) else {
            continue;
        };
        let mut pieces: Vec<String> = Vec::new();
        collect_value(value, &mut pieces, 0)?;
        if pieces.is_empty() {
            continue;
        }
        let text = pieces.join("\n");
        total = total.saturating_add(text.len());
        if total > max_bytes {
            return Err(ExtractError::TooLarge);
        }
        input.segments.push(Segment {
            origin: Origin::User,
            text: Sensitive::new(text),
        });
    }
    Ok(input)
}

// ---------------------------------------------------------------------------
// 输出
// ---------------------------------------------------------------------------

/// `finish_reason` 的词表（OpenAI 兼容）。词表之外的值按 `stop` 处理。
const FINISH_REASONS: &[&str] = &["stop", "length", "tool_calls", "content_filter"];

fn numeric_usage(usage: Option<&Value>) -> Option<Value> {
    let object = usage?.as_object()?;
    let mut out = Map::new();
    for key in ["prompt_tokens", "completion_tokens", "total_tokens"] {
        if let Some(n) = object.get(key).and_then(Value::as_u64) {
            out.insert(key.to_string(), json!(n));
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// 重建后的响应：要检查的文本，加上可以发给客户端的字节。
#[derive(Debug, Clone)]
pub struct RebuiltResponse {
    pub output: CheckedOutput,
    pub body: Vec<u8>,
}

/// 响应里由服务端写的标识：不取上游响应里的 `id`、`model`、工具调用 `id`。
#[derive(Debug, Clone, Copy)]
pub struct ResponseIdentity<'a> {
    /// 响应的 `id`：请求号。
    pub id: &'a str,
    /// 响应的 `model`：已选路由的模型。
    pub model: &'a str,
}

impl ResponseIdentity<'_> {
    /// 第 `index` 个工具调用的 `id`：从请求号派生（同一段对话里不同请求的工具调用 `id` 不会撞），只含字母、数字、`_`。
    fn tool_call_id(&self, index: usize) -> String {
        let stem: String = self
            .id
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(12)
            .collect();
        format!("call_{stem}_{index}")
    }
}

/// 从上游的非流式响应里只取会被检查的字段，重建发给客户端的响应体。
/// 标识（响应 `id`、`model`、工具调用 `id`）由 `identity` 和请求号决定，不取上游的。
/// 结构不符合约定返回 `Malformed`（失败即关闭）：包括工具调用的 `arguments` 不是合法的 JSON。
pub fn rebuild_response(
    upstream: &[u8],
    identity: &ResponseIdentity<'_>,
    max_bytes: usize,
) -> Result<RebuiltResponse, ExtractError> {
    let value: Value = serde_json::from_slice(upstream).map_err(|_| ExtractError::Malformed)?;
    let root = value.as_object().ok_or(ExtractError::Malformed)?;
    let choice = root
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(Value::as_object)
        .ok_or(ExtractError::Malformed)?;
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or(ExtractError::Malformed)?;
    if let Some(role) = message.get("role")
        && role.as_str() != Some("assistant")
    {
        return Err(ExtractError::Malformed);
    }

    // content：字符串、null，或分段数组（text 和 refusal 分段）
    let mut content_text: Option<String> = None;
    let mut refusal_text: Option<String> = None;
    match message.get("content") {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) => content_text = Some(text.clone()),
        Some(Value::Array(parts)) => {
            let mut text = String::new();
            let mut refusal = String::new();
            for part in parts {
                let object = part.as_object().ok_or(ExtractError::Malformed)?;
                match object.get("type").and_then(Value::as_str) {
                    Some("text") => match object.get("text") {
                        Some(Value::String(piece)) => text.push_str(piece),
                        _ => return Err(ExtractError::Malformed),
                    },
                    Some("refusal") => match object.get("refusal") {
                        Some(Value::String(piece)) => refusal.push_str(piece),
                        _ => return Err(ExtractError::Malformed),
                    },
                    // 图片、音频……：没法检查，不发
                    _ => return Err(ExtractError::Malformed),
                }
            }
            content_text = Some(text);
            if !refusal.is_empty() {
                refusal_text = Some(refusal);
            }
        }
        Some(_) => return Err(ExtractError::Malformed),
    }
    match message.get("refusal") {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) => {
            refusal_text = Some(match refusal_text {
                Some(existing) => format!("{existing}{text}"),
                None => text.clone(),
            });
        }
        Some(_) => return Err(ExtractError::Malformed),
    }

    // tool_calls
    let mut calls: Vec<(String, String)> = Vec::new(); // (name, arguments)
    match message.get("tool_calls") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for item in items {
                let object = item.as_object().ok_or(ExtractError::Malformed)?;
                if let Some(kind) = object.get("type")
                    && kind.as_str() != Some("function")
                {
                    return Err(ExtractError::Malformed);
                }
                let function = object
                    .get("function")
                    .and_then(Value::as_object)
                    .ok_or(ExtractError::Malformed)?;
                let name = function
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or(ExtractError::Malformed)?
                    .to_string();
                let arguments = match function.get("arguments") {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Null) | None => String::new(),
                    // 有的上游把参数当对象返回：序列化成字符串（OpenAI 的形状），同样要检查
                    Some(other @ (Value::Object(_) | Value::Array(_))) => other.to_string(),
                    Some(_) => return Err(ExtractError::Malformed),
                };
                // 参数必须是合法的 JSON（解析不了、或者套得太深，客户端和我们看到的就可能不是同一份东西）
                decoded_pieces(&arguments)?;
                calls.push((name, arguments));
            }
        }
        Some(_) => return Err(ExtractError::Malformed),
    }

    let output = CheckedOutput {
        content: content_text.clone().map(Sensitive::new),
        refusal: refusal_text.clone().map(Sensitive::new),
        tool_calls: calls
            .iter()
            .map(|(name, arguments)| ToolCallText {
                name: Sensitive::new(name.clone()),
                arguments: Sensitive::new(arguments.clone()),
            })
            .collect(),
    };
    if output.total_bytes() > max_bytes {
        return Err(ExtractError::TooLarge);
    }

    // ---- 重建 ----
    let mut rebuilt_message = Map::new();
    rebuilt_message.insert("role".into(), json!("assistant"));
    rebuilt_message.insert(
        "content".into(),
        content_text.map_or(Value::Null, Value::String),
    );
    if let Some(refusal) = refusal_text {
        rebuilt_message.insert("refusal".into(), Value::String(refusal));
    }
    if !calls.is_empty() {
        rebuilt_message.insert(
            "tool_calls".into(),
            Value::Array(
                calls
                    .iter()
                    .enumerate()
                    .map(|(index, (name, arguments))| {
                        json!({
                            "id": identity.tool_call_id(index),
                            "type": "function",
                            "function": {"name": name, "arguments": arguments},
                        })
                    })
                    .collect(),
            ),
        );
    }
    let finish_reason = if calls.is_empty() {
        choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|reason| FINISH_REASONS.contains(reason))
            .unwrap_or("stop")
    } else {
        "tool_calls"
    };
    let mut rebuilt = Map::new();
    rebuilt.insert("id".into(), json!(identity.id));
    rebuilt.insert("object".into(), json!("chat.completion"));
    rebuilt.insert(
        "created".into(),
        json!(root.get("created").and_then(Value::as_u64).unwrap_or(0)),
    );
    rebuilt.insert("model".into(), json!(identity.model));
    rebuilt.insert(
        "choices".into(),
        json!([{
            "index": 0,
            "message": Value::Object(rebuilt_message),
            "finish_reason": finish_reason,
        }]),
    );
    if let Some(usage) = numeric_usage(root.get("usage")) {
        rebuilt.insert("usage".into(), usage);
    }
    let body = serde_json::to_vec(&Value::Object(rebuilt)).map_err(|_| ExtractError::Malformed)?;
    Ok(RebuiltResponse { output, body })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn body(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    /// 响应里由服务端写的标识：请求号 + 已选路由的模型。
    fn ident(id: &str) -> ResponseIdentity<'_> {
        ResponseIdentity {
            id,
            model: "example-org/chat-70b",
        }
    }

    // 旧的 `every_forwarded_field_is_classified`（白名单里的每个字段都要在「装文本的 / 只会是数字」两张表里）：
    // 取文本不再依赖那两张表——转发的请求由 `build_chat_body` 构造，里面的每个字符串都取，白名单里新增字段不需要分类
    // 也不会漏检。它守的保证由下面的 `every_string_in_the_forwarded_request_is_in_the_checked_text`（更强）接管。

    #[test]
    fn every_string_anywhere_in_the_forwarded_text_fields_is_extracted() {
        let input = extract_input(
            &body(json!({
                "model": "m",
                "messages": [
                    {"role": "system", "content": "SYS"},
                    {"role": "user", "content": "USER", "name": "NAME-FIELD"},
                    {"role": "assistant", "content": null, "tool_calls": [
                        {"id": "c1", "type": "function",
                         "function": {"name": "lookup", "arguments": "{\"q\":\"ARGS\"}"}}]},
                    {"role": "tool", "tool_call_id": "c1", "content": "TOOL-RESULT"},
                    {"role": "user", "content": [{"type": "text", "text": "PART-1"}, {"type": "text", "text": "PART-2"}]},
                ],
                "tools": [{"type": "function", "function": {"name": "f", "description": "TOOL-DESC"}}],
                "response_format": {"type": "json_schema", "json_schema": {"name": "SCHEMA-NAME"}},
                "stop": ["STOP-SEQ"],
                "user": "NOT-FORWARDED",
                "metadata": {"x": "NOT-FORWARDED-EITHER"},
            })),
            1 << 20,
        )
        .unwrap();
        let all: String = input
            .segments
            .iter()
            .map(|s| s.text.expose().to_string())
            .collect::<Vec<_>>()
            .join("|");
        for expected in [
            "SYS",
            "USER",
            "NAME-FIELD",
            "lookup",
            "ARGS",
            "TOOL-RESULT",
            "PART-1",
            "PART-2",
            "TOOL-DESC",
            "SCHEMA-NAME",
            "STOP-SEQ",
        ] {
            assert!(all.contains(expected), "{expected} missing from {all}");
        }
        // 白名单不转发的字段不用检查（模型读不到）
        assert!(!all.contains("NOT-FORWARDED"));
        assert!(!input.has_uncheckable_content);
    }

    #[test]
    fn origins_follow_the_role() {
        let input = extract_input(
            &body(json!({"messages": [
                {"role": "system", "content": "a"},
                {"role": "developer", "content": "b"},
                {"role": "user", "content": "c"},
                {"role": "wizard", "content": "d"},
                {"role": "assistant", "content": "e"},
                {"role": "tool", "content": "f"},
                {"role": "function", "content": "g"},
            ], "tools": [{"type": "function"}], "stop": "END"})),
            1 << 20,
        )
        .unwrap();
        let origins: Vec<Origin> = input.segments.iter().map(|s| s.origin).collect();
        assert_eq!(
            origins,
            vec![
                // r3 High#1：客户端填的 system / developer 角色、工具定义、stop 都不是运营方：没有登记任何受信的运营方提示，
                // 客户端写的内容一律按孩子写的话（旧的预期是 Operator）
                Origin::User, // system
                Origin::User, // developer
                Origin::User,
                Origin::User, // 未知角色按最严处理
                Origin::Assistant,
                Origin::Tool,
                Origin::Tool,
                Origin::User, // stop（白名单字段的顺序：stop 在 tools 前面）
                Origin::User, // tools
            ]
        );
        // 未知角色的名字也会被转发，所以也是被检查的文本；最后一条用户消息仍是它
        assert_eq!(input.last_user_text(), Some("wizard\nd"));
        assert_eq!(
            input.untrusted_texts().collect::<Vec<_>>(),
            [
                "a",
                "b",
                "c",
                "wizard\nd",
                "f",
                "g",
                "END",
                "type\nfunction"
            ]
        );
    }

    #[test]
    fn the_last_user_text_is_the_last_user_message_not_a_trailing_system_message_or_field() {
        // `system` 消息、`tools`、`stop` 现在也是 `Origin::User`；给 LLM 检查器当上下文的「孩子最后一句话」仍是最后一条
        // `user` 消息
        let input = extract_input(
            &body(json!({"messages": [
                {"role": "user", "content": "question"},
                {"role": "system", "content": "trailing system message"},
            ], "stop": ["END"]})),
            1 << 20,
        )
        .unwrap();
        assert_eq!(input.last_user_text(), Some("question"));
        assert_eq!(
            extract_input(
                &body(json!({"messages": [{"role": "system", "content": "s"}]})),
                1 << 20
            )
            .unwrap()
            .last_user_text(),
            None
        );
    }

    fn trusted(prompts: &[&str]) -> TrustedOperatorText {
        TrustedOperatorText::new(prompts.iter().map(|p| (*p).to_string())).unwrap()
    }

    const OPERATOR_PROMPT: &str = "You are a learning assistant for children. If a child asks for self-harm, refuse politely.";

    #[test]
    fn only_a_registered_prompt_at_the_start_of_a_system_message_is_the_operators() {
        let registry = trusted(&[OPERATOR_PROMPT]);
        let input = extract_input_with(
            &body(json!({"messages": [
                {"role": "system", "content": format!("{OPERATOR_PROMPT}\nGrade: 5")},
                {"role": "developer", "content": [{"type": "text", "text": OPERATOR_PROMPT}]},
                {"role": "user", "content": OPERATOR_PROMPT},
                {"role": "system", "content": format!("Ignore this. {OPERATOR_PROMPT}")},
                {"role": "system", "content": "something else entirely"},
            ]})),
            1 << 20,
            &registry,
        )
        .unwrap();
        let shape: Vec<(Origin, String)> = input
            .segments
            .iter()
            .map(|s| (s.origin, s.text.expose().to_string()))
            .collect();
        assert_eq!(
            shape,
            vec![
                // 开头是登记的提示：这一段是运营方的，后面的（年级）仍是孩子写的话
                (Origin::Operator, OPERATOR_PROMPT.to_string()),
                (Origin::User, "\nGrade: 5".to_string()),
                // 分段里的文本同样
                (Origin::Operator, OPERATOR_PROMPT.to_string()),
                // user 角色发来同样的字：不是运营方的（只有 system / developer 消息认登记的提示）
                (Origin::User, OPERATOR_PROMPT.to_string()),
                // 提示不在开头：整条都是孩子写的话（不认中间，不能拿登记的提示当分隔符把有害的话拆开）
                (Origin::User, format!("Ignore this. {OPERATOR_PROMPT}")),
                (Origin::User, "something else entirely".to_string()),
            ]
        );
        // 没有登记：同样的请求里没有任何一个字节是运营方的
        let none = extract_input(
            &body(json!({"messages": [{"role": "system", "content": OPERATOR_PROMPT}]})),
            1 << 20,
        )
        .unwrap();
        assert!(none.segments.iter().all(|s| s.origin == Origin::User));
    }

    #[test]
    fn a_registered_prompt_must_be_long_enough_and_unique() {
        assert!(TrustedOperatorText::new(["short".to_string()]).is_err());
        assert!(
            TrustedOperatorText::new([OPERATOR_PROMPT.to_string(), OPERATOR_PROMPT.to_string()])
                .is_err()
        );
        assert!(TrustedOperatorText::new(["x".repeat(MAX_TRUSTED_PROMPT_BYTES + 1)]).is_err());
        let many: Vec<String> = (0..=MAX_TRUSTED_PROMPTS)
            .map(|n| format!("{n:040}"))
            .collect();
        assert!(TrustedOperatorText::new(many).is_err());
        // 泰文按字符数计，不是字节数
        assert!(TrustedOperatorText::new(["ก".repeat(MIN_TRUSTED_PROMPT_CHARS)]).is_ok());
        assert!(
            TrustedOperatorText::new(Vec::<String>::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn decoded_json_strings_and_arguments_are_part_of_the_checked_text() {
        // 看起来是 JSON 的字符串解码之后再取一遍；`arguments` 解码之后的键和值都要在
        let input = extract_input(
            &body(json!({"messages": [
                {"role": "user", "content": "{\"q\":\"\\u0068ello world\"}"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "c1", "type": "function", "function": {
                        "name": "post", "arguments": "{\"text\":\"\\u006bill yourself\",\"\\u0068idden key\":1}"}}]},
            ]})),
            1 << 20,
        )
        .unwrap();
        let all: String = input
            .segments
            .iter()
            .map(|s| s.text.expose().to_string())
            .collect::<Vec<_>>()
            .join("|");
        for expected in ["hello world", "kill yourself", "hidden key"] {
            assert!(all.contains(expected), "{expected} missing from {all}");
        }
        // 解析不了的 `arguments`、套得太深的 JSON 字符串：失败即关闭；空的 `arguments` 是「没有参数」
        for bad in ["{not json", "nope", "[1,2"] {
            assert_eq!(
                extract_input(
                    &body(json!({"messages": [{"role": "assistant", "content": null, "tool_calls": [
                        {"id": "c", "type": "function", "function": {"name": "f", "arguments": bad}}]}]})),
                    1 << 20
                )
                .unwrap_err(),
                ExtractError::Malformed,
                "{bad}"
            );
        }
        for ok in ["", "  ", "{}"] {
            assert!(
                extract_input(
                    &body(json!({"messages": [{"role": "assistant", "content": null, "tool_calls": [
                        {"id": "c", "type": "function", "function": {"name": "f", "arguments": ok}}]}]})),
                    1 << 20
                )
                .is_ok(),
                "{ok:?}"
            );
        }
        let mut nested = json!("x").to_string();
        for _ in 0..8 {
            nested = json!({"a": nested}).to_string();
        }
        assert_eq!(
            extract_input(
                &body(json!({"messages": [{"role": "user", "content": nested}]})),
                1 << 20
            )
            .unwrap_err(),
            ExtractError::Malformed
        );
    }

    #[test]
    fn a_field_the_builder_forwards_in_the_future_is_checked_without_being_listed_anywhere() {
        // 取文本从「转发的请求里有什么字段」出发，不是白名单里有什么：将来 `build_chat_body` 多转发一个字段，
        // 这里不用改也不会漏
        let view = body(json!({
            "messages": [{"role": "user", "content": "hi"}],
            "a_field_added_to_the_builder_later": {"nested key": ["nested value"]},
            "stop": "END",
        }));
        let input = extract_view(&view, 1 << 20, &TrustedOperatorText::none()).unwrap();
        let all: String = input
            .segments
            .iter()
            .map(|s| s.text.expose().to_string())
            .collect::<Vec<_>>()
            .join("|");
        for expected in ["nested key", "nested value", "END"] {
            assert!(all.contains(expected), "{expected} missing from {all}");
        }
        // 顺序固定：白名单里的字段在前（stop），其余按名字排序在后
        let order: Vec<&str> = input
            .segments
            .iter()
            .map(|s| s.text.expose())
            .skip(1)
            .collect();
        assert_eq!(order, ["END", "nested key\nnested value"]);
    }

    #[test]
    fn a_json_string_nested_deeper_than_the_parser_allows_fails_closed() {
        // 超过 serde_json 的递归上限（128 层）：它不肯解析，我们看不到里面的东西，失败即关闭；
        // 只是以 `[` 开头的普通文本不受影响
        let deep = format!("{}1{}", "[".repeat(200), "]".repeat(200));
        assert_eq!(
            extract_input(
                &body(json!({"messages": [{"role": "user", "content": deep}]})),
                1 << 20
            )
            .unwrap_err(),
            ExtractError::Malformed
        );
        assert!(
            extract_input(
                &body(
                    json!({"messages": [{"role": "user", "content": "[1] first, [2] second {x"}]})
                ),
                1 << 20
            )
            .is_ok()
        );
    }

    #[test]
    fn a_request_the_upstream_builder_refuses_is_still_checked_so_the_check_comes_first() {
        // 字段类型不对（`temperature` 是字符串）：`build_chat_body` 构造不出请求，执行器会在发出任何字节之前拒绝它；
        // 检查仍然先看过客户端写的话（结论和以前一样：有害就是 422，没有害处就让执行器去报 invalid_request）
        let input = extract_input(
            &body(
                json!({"messages": [{"role": "user", "content": "i want to die"}],
                "temperature": "hot", "stop": ["a stop"]}),
            ),
            1 << 20,
        )
        .unwrap();
        let all: String = input
            .segments
            .iter()
            .map(|s| s.text.expose().to_string())
            .collect::<Vec<_>>()
            .join("|");
        assert!(all.contains("i want to die") && all.contains("a stop"));
    }

    #[test]
    fn non_text_parts_are_flagged_not_silently_skipped() {
        for part in [
            json!({"type": "image_url", "image_url": {"url": "https://example.invalid/x.png"}}),
            json!({"type": "input_audio", "input_audio": {"data": "AAAA", "format": "wav"}}),
            json!({"type": "file", "file": {"file_id": "f"}}),
            json!({"type": "something_new"}),
            json!({"no_type": true}),
        ] {
            let input = extract_input(
                &body(json!({"messages": [{"role": "user", "content": [
                    {"type": "text", "text": "hello"}, part]}]})),
                1 << 20,
            )
            .unwrap();
            assert!(input.has_uncheckable_content);
        }
    }

    #[test]
    fn malformed_requests_fail_closed() {
        for bad in [
            json!({"messages": "hi"}),
            json!({"model": "m"}),
            json!({"messages": ["hi"]}),
            json!({"messages": [{"content": "no role"}]}),
            json!({"messages": [{"role": 3, "content": "x"}]}),
            json!({"messages": [{"role": "user", "content": {"a": "b"}}]}),
            json!({"messages": [{"role": "user", "content": [7]}]}),
            json!({"messages": [{"role": "user", "content": [{"type": "text"}]}]}),
        ] {
            assert_eq!(
                extract_input(&body(bad.clone()), 1 << 20).unwrap_err(),
                ExtractError::Malformed,
                "{bad}"
            );
        }
        // 嵌套太深
        let mut deep = json!("x");
        for _ in 0..80 {
            deep = json!([deep]);
        }
        assert_eq!(
            extract_input(
                &body(json!({"messages": [{"role": "user", "content": "a", "extra": deep}]})),
                1 << 20
            )
            .unwrap_err(),
            ExtractError::Malformed
        );
    }

    #[test]
    fn oversized_input_is_too_large() {
        let big = "x".repeat(100);
        assert_eq!(
            extract_input(
                &body(json!({"messages": [{"role": "user", "content": big}]})),
                99
            )
            .unwrap_err(),
            ExtractError::TooLarge
        );
        assert!(
            extract_input(
                &body(json!({"messages": [{"role": "user", "content": big}]})),
                100
            )
            .is_ok()
        );
    }

    fn completion(message: Value) -> Vec<u8> {
        json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1_700_000_000u64,
            "model": "example-org/chat-70b",
            "choices": [{"index": 0, "finish_reason": "stop", "message": message}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 30, "total_tokens": 42,
                      "prompt_tokens_details": {"cached_tokens": 0}, "cost": 0.0001},
            "provider": "SomeProvider",
            "system_fingerprint": "fp",
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn the_rebuilt_response_carries_only_checked_text_and_server_written_identifiers() {
        let upstream = completion(json!({
            "role": "assistant", "content": "ANSWER",
            "reasoning_content": "UNCHECKED-REASONING",
            "reasoning": "UNCHECKED-REASONING-2",
            "annotations": [{"type": "url_citation", "title": "UNCHECKED-TITLE"}],
            "tool_calls": [{"id": "call_a", "type": "function",
                            "function": {"name": "calc", "arguments": "{\"x\":1}"}}],
        }));
        let rebuilt = rebuild_response(&upstream, &ident("fallback"), 1 << 20).unwrap();
        let text = String::from_utf8(rebuilt.body.clone()).unwrap();
        for dropped in [
            "UNCHECKED",
            "SomeProvider",
            "system_fingerprint",
            "cached_tokens",
            "cost",
        ] {
            assert!(!text.contains(dropped), "{dropped} leaked into {text}");
        }
        let value: Value = serde_json::from_slice(&rebuilt.body).unwrap();
        // r3 Medium#1：`id` 是服务端给的（请求号），不是上游的 `chatcmpl-1`；`model` 是已选路由的模型（传进来的值）
        assert_eq!(value["id"], "fallback");
        assert_eq!(value["object"], "chat.completion");
        assert_eq!(value["created"], 1_700_000_000u64);
        assert_eq!(value["model"], "example-org/chat-70b");
        assert_eq!(value["choices"][0]["message"]["content"], "ANSWER");
        assert_eq!(value["choices"][0]["message"]["role"], "assistant");
        assert_eq!(value["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            value["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"x\":1}"
        );
        assert_eq!(value["usage"]["prompt_tokens"], 12);
        assert_eq!(value["usage"]["total_tokens"], 42);
        // 会被检查的文本和发给客户端的文本是同一份
        let texts = rebuilt.output.texts();
        assert!(
            texts.contains(&"ANSWER") && texts.contains(&"calc") && texts.contains(&"{\"x\":1}")
        );
    }

    #[test]
    fn identifiers_are_always_written_by_the_server_whatever_the_upstream_says() {
        let upstream = json!({
            "id": "an id with spaces and Thai สวัสดี",
            "model": "<script>",
            "choices": [{"message": {"role": "assistant", "content": "x",
                "tool_calls": [{"id": "bad id!", "type": "function",
                                "function": {"name": "f", "arguments": {"a": 1}}}]},
                "finish_reason": "weird_reason"}],
        })
        .to_string();
        let rebuilt = rebuild_response(upstream.as_bytes(), &ident("req-123"), 1 << 20).unwrap();
        let value: Value = serde_json::from_slice(&rebuilt.body).unwrap();
        assert_eq!(value["id"], "req-123");
        // 旧的预期是 `unknown`（上游的 model 不是短标识时的兜底）；现在 model 永远是已选路由的模型
        assert_eq!(value["model"], "example-org/chat-70b");
        // 工具调用 id 从请求号派生（旧的预期是 `call_0`），同一段对话里不同请求的不会撞
        assert_eq!(
            value["choices"][0]["message"]["tool_calls"][0]["id"],
            "call_req123_0"
        );
        // 对象形式的参数被序列化成字符串，并且被检查
        assert_eq!(
            value["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":1}"
        );
        assert!(rebuilt.output.texts().contains(&"{\"a\":1}"));
        assert!(value.get("usage").is_none());
        // 有工具调用时 finish_reason 固定为 tool_calls
        assert_eq!(value["choices"][0]["finish_reason"], "tool_calls");
        // 没有工具调用、词表之外的 finish_reason 按 stop
        let plain =
            json!({"choices": [{"message": {"content": "x"}, "finish_reason": "eos"}]}).to_string();
        let value: Value = serde_json::from_slice(
            &rebuild_response(plain.as_bytes(), &ident("r"), 1 << 20)
                .unwrap()
                .body,
        )
        .unwrap();
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn parts_refusals_and_empty_answers() {
        let upstream = completion(json!({"role": "assistant", "content": [
            {"type": "text", "text": "A"}, {"type": "text", "text": "B"},
            {"type": "refusal", "refusal": "NO"}], "refusal": "MORE"}));
        let rebuilt = rebuild_response(&upstream, &ident("r"), 1 << 20).unwrap();
        assert_eq!(rebuilt.output.content.as_ref().unwrap().expose(), "AB");
        assert_eq!(rebuilt.output.refusal.as_ref().unwrap().expose(), "NOMORE");
        // 空回答：没有什么要检查的，也不是错误
        let empty = completion(json!({"role": "assistant", "content": null}));
        let rebuilt = rebuild_response(&empty, &ident("r"), 1 << 20).unwrap();
        assert!(rebuilt.output.texts().is_empty());
    }

    #[test]
    fn tool_call_arguments_must_be_json_and_are_checked_decoded() {
        // r3 High#4：`arguments` 里的转义（`k`）解码之后才是客户端看到的字；解析不了的参数不发
        let upstream = completion(json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "c", "type": "function", "function": {
                "name": "post", "arguments": r#"{"text":"kill yourself","key":"v"}"#}}]}));
        let rebuilt = rebuild_response(&upstream, &ident("r"), 1 << 20).unwrap();
        let call = &rebuilt.output.tool_calls[0];
        let mut decoded = call.decoded_arguments();
        decoded.sort();
        assert_eq!(decoded, ["key", "kill yourself", "text", "v"]);
        // 全部要检查的文本里有解码之后的字，也有原文（原文就是发给客户端的）
        let all = rebuilt.output.all_texts();
        assert!(all.iter().any(|t| t == "kill yourself"));
        assert!(all.iter().any(|t| t.contains(r"kill")));
        // 嵌套的 JSON 字符串再解一层
        let nested = completion(json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "c", "type": "function", "function": {
                "name": "post", "arguments": r#"{"payload":"{\"text\":\"\\u006bill yourself\"}"}"#}}]}));
        let rebuilt = rebuild_response(&nested, &ident("r"), 1 << 20).unwrap();
        assert!(
            rebuilt
                .output
                .all_texts()
                .iter()
                .any(|t| t == "kill yourself")
        );
        // 不是 JSON：失败即关闭。没有参数（空串、null、缺省）不是错误
        for bad in ["{not json", "nope", "{\"a\":"] {
            let upstream = completion(json!({"role": "assistant", "content": null, "tool_calls": [
                {"id": "c", "type": "function", "function": {"name": "f", "arguments": bad}}]}));
            assert_eq!(
                rebuild_response(&upstream, &ident("r"), 1 << 20).unwrap_err(),
                ExtractError::Malformed,
                "{bad}"
            );
        }
        for ok in [json!(""), Value::Null] {
            let upstream = completion(json!({"role": "assistant", "content": null, "tool_calls": [
                {"id": "c", "type": "function", "function": {"name": "f", "arguments": ok}}]}));
            assert!(rebuild_response(&upstream, &ident("r"), 1 << 20).is_ok());
        }
    }

    #[test]
    fn unvettable_responses_fail_closed() {
        for bad in [
            "not json".to_string(),
            "[]".to_string(),
            json!({"choices": []}).to_string(),
            json!({"choices": [1]}).to_string(),
            json!({"choices": [{"text": "legacy completion"}]}).to_string(),
            json!({"choices": [{"message": {"role": "user", "content": "x"}}]}).to_string(),
            json!({"choices": [{"message": {"content": {"a": 1}}}]}).to_string(),
            json!({"choices": [{"message": {"content": [{"type": "image_url"}]}}]}).to_string(),
            json!({"choices": [{"message": {"content": "x", "tool_calls": "nope"}}]}).to_string(),
            json!({"choices": [{"message": {"content": "x", "tool_calls": [{"function": {}}]}}]}).to_string(),
            json!({"choices": [{"message": {"content": "x", "tool_calls": [{"type": "code", "function": {"name": "f"}}]}}]}).to_string(),
            json!({"choices": [{"message": {"content": "x", "refusal": 3}}]}).to_string(),
        ] {
            assert_eq!(
                rebuild_response(bad.as_bytes(), &ident("r"), 1 << 20).unwrap_err(),
                ExtractError::Malformed,
                "{bad}"
            );
        }
        let huge = completion(json!({"role": "assistant", "content": "y".repeat(50)}));
        assert_eq!(
            rebuild_response(&huge, &ident("r"), 49).unwrap_err(),
            ExtractError::TooLarge
        );
    }

    #[test]
    fn nothing_the_upstream_says_about_ids_or_the_model_reaches_the_client() {
        // r3 Medium#1：孩子输入里的电话号码被上游回显成顶层 `id`（或 `model`、工具调用 `id`）。
        // 它们都是合法的「短标识」，所以旧实现原样放行，而且绕过了 PII 输出检查。
        let upstream = json!({
            "id": "0812345678",
            "model": "CHILDINPUT-SENTINEL-4f81c2d7",
            "choices": [{"message": {"role": "assistant", "content": "ok",
                "tool_calls": [{"id": "0899999999", "type": "function",
                                "function": {"name": "f", "arguments": "{}"}}]}}],
        })
        .to_string();
        let rebuilt = rebuild_response(upstream.as_bytes(), &ident("req-1"), 1 << 20).unwrap();
        let text = String::from_utf8(rebuilt.body).unwrap();
        for echoed in ["0812345678", "CHILDINPUT-SENTINEL", "0899999999"] {
            assert!(!text.contains(echoed), "{echoed} leaked into {text}");
        }
        // 取而代之的是服务端写的：请求号、已选路由的模型、从请求号派生的工具调用 id
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["id"], "req-1");
        assert_eq!(value["model"], "example-org/chat-70b");
        assert_eq!(
            value["choices"][0]["message"]["tool_calls"][0]["id"],
            "call_req1_0"
        );
    }

    // ---- 「检查的 = 转发的」：性质测试 ----

    /// 可复现的伪随机数（不引入依赖）。
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// 每个标签都独一无二，也不会撞上任何规则的词。
    struct Tags(usize);

    impl Tags {
        fn tag(&mut self) -> String {
            self.0 += 1;
            format!("zqx{}tag", self.0)
        }
    }

    fn random_value(rng: &mut Rng, tags: &mut Tags, depth: usize) -> Value {
        match if depth >= 3 {
            rng.below(4)
        } else {
            rng.below(7)
        } {
            0 => json!(tags.tag()),
            1 => json!(rng.below(100)),
            2 => json!(rng.below(2) == 0),
            3 => Value::Null,
            4 | 5 => Value::Array(
                (0..rng.below(3))
                    .map(|_| random_value(rng, tags, depth + 1))
                    .collect(),
            ),
            _ => Value::Object(
                (0..rng.below(3))
                    .map(|_| (tags.tag(), random_value(rng, tags, depth + 1)))
                    .collect(),
            ),
        }
    }

    fn random_message(rng: &mut Rng, tags: &mut Tags) -> Value {
        let role = match rng.below(8) {
            0 => "system".to_string(),
            1 => "developer".to_string(),
            2 | 3 => "user".to_string(),
            4 => "assistant".to_string(),
            5 => "tool".to_string(),
            6 => "function".to_string(),
            _ => tags.tag(), // 未知角色：它的名字也会被转发
        };
        let mut message = Map::new();
        message.insert("role".into(), json!(role));
        match rng.below(4) {
            0 => {
                message.insert("content".into(), json!(tags.tag()));
            }
            1 => {
                let mut part = Map::new();
                part.insert("type".into(), json!("text"));
                part.insert("text".into(), json!(tags.tag()));
                if rng.below(2) == 0 {
                    part.insert(tags.tag(), random_value(rng, tags, 1));
                }
                message.insert(
                    "content".into(),
                    json!([Value::Object(part), json!(tags.tag())]),
                );
            }
            2 => {
                message.insert("content".into(), Value::Null);
            }
            _ => {}
        }
        if rng.below(2) == 0 {
            message.insert("name".into(), json!(tags.tag()));
        }
        if rng.below(2) == 0 {
            let arguments = serde_json::to_string(&json!({tags.tag(): tags.tag()})).unwrap();
            message.insert(
                "tool_calls".into(),
                json!([{"id": tags.tag(), "type": "function",
                        "function": {"name": tags.tag(), "arguments": arguments}}]),
            );
        }
        if rng.below(3) == 0 {
            message.insert(tags.tag(), random_value(rng, tags, 1));
        }
        Value::Object(message)
    }

    /// 一个随机的客户端请求体：每个白名单字段都有可能出现，还有不会被转发的字段。
    fn random_client_body(seed: u64) -> Map<String, Value> {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut tags = Tags(0);
        let mut body = Map::new();
        body.insert("model".into(), json!("idoris/agent"));
        let messages: Vec<Value> = (0..=rng.below(4))
            .map(|_| random_message(&mut rng, &mut tags))
            .collect();
        body.insert("messages".into(), Value::Array(messages));
        if rng.below(2) == 0 {
            let tools: Vec<Value> = (0..=rng.below(2))
                .map(|_| {
                    json!({"type": "function", "function": {
                        "name": tags.tag(), "description": tags.tag(),
                        "parameters": {"type": "object", "properties": {
                            tags.tag(): {"type": "string", "description": tags.tag()}}}}})
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
        }
        if rng.below(2) == 0 {
            body.insert(
                "tool_choice".into(),
                if rng.below(2) == 0 {
                    json!(tags.tag())
                } else {
                    json!({"type": "function", "function": {"name": tags.tag()}})
                },
            );
        }
        if rng.below(2) == 0 {
            body.insert(
                "response_format".into(),
                json!({"type": "json_schema", "json_schema": {
                    "name": tags.tag(), "schema": random_value(&mut rng, &mut tags, 0)}}),
            );
        }
        if rng.below(2) == 0 {
            body.insert(
                "stop".into(),
                if rng.below(2) == 0 {
                    json!(tags.tag())
                } else {
                    json!([tags.tag(), tags.tag()])
                },
            );
        }
        for field in NUMBER_FIELDS {
            if rng.below(2) == 0 {
                body.insert((*field).into(), json!(rng.below(10)));
            }
        }
        if rng.below(2) == 0 {
            body.insert("parallel_tool_calls".into(), json!(true));
        }
        // 白名单之外的字段不会被转发，检查与否都不影响「检查的 = 转发的」
        body.insert("user".into(), json!(tags.tag()));
        body.insert("metadata".into(), json!({tags.tag(): tags.tag()}));
        body
    }

    const NUMBER_FIELDS: &[&str] = &[
        "temperature",
        "top_p",
        "top_k",
        "min_p",
        "presence_penalty",
        "frequency_penalty",
        "repetition_penalty",
        "seed",
    ];

    /// 固定的结构词：不是客户端能写的文本（白名单字段名、上游请求里由服务端写的字段、消息结构里固定的键和角色名）。
    /// 它们出现与否不影响检查，因为里面装不下任何内容。
    const STRUCTURAL: &[&str] = &[
        "model",
        "max_tokens",
        "stream",
        "role",
        "content",
        "type",
        "text",
        "system",
        "developer",
        "user",
        "assistant",
        "tool",
        "function",
    ];

    fn every_string(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(text) => out.push(text.clone()),
            Value::Array(items) => items.iter().for_each(|item| every_string(item, out)),
            Value::Object(map) => {
                for (key, item) in map {
                    out.push(key.clone());
                    every_string(item, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn every_string_in_the_forwarded_request_is_in_the_checked_text() {
        // 性质：`build_chat_body` 真正会发给上游的请求里，**每一个**字符串（值和键名，任何深度）都出现在检查内容里。
        // 随机的请求体里每个字符串都是独一无二的标签；上游请求由真正的 `build_chat_body` 构造。
        let none = Map::new();
        for seed in 1..=400u64 {
            let client = random_client_body(seed);
            let forwarded = build_chat_body(&ChatCall {
                client_body: &client,
                upstream_model: "upstream-model",
                max_tokens: 64,
                stream: false,
                extra_body: &none,
            })
            .unwrap();
            let mut forwarded: Value = serde_json::from_slice(&forwarded).unwrap();
            // 服务端写的字段（模型、上限、流式开关）不是客户端的文本
            for field in SERVER_SET_FIELDS {
                forwarded.as_object_mut().unwrap().remove(*field);
            }
            let mut strings = Vec::new();
            every_string(&forwarded, &mut strings);
            let checked = extract_input(&client, 1 << 22).unwrap();
            let all: String = checked
                .segments
                .iter()
                .map(|s| s.text.expose().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            let tagged = strings.iter().filter(|s| s.starts_with("zqx")).count();
            assert!(tagged > 0, "seed {seed} generated nothing to check");
            for string in &strings {
                if STRUCTURAL.contains(&string.as_str())
                    || FORWARDED_FIELDS.contains(&string.as_str())
                {
                    continue;
                }
                assert!(
                    all.contains(string.as_str()),
                    "seed {seed}: the forwarded string {string:?} is not in the checked text\nbody: {}",
                    Value::Object(client.clone())
                );
            }
        }
    }

    /// 一份随机的上游响应：会被检查的位置里是 `chk…` 标签，不会被检查的位置（`id`、`model`、工具调用 `id`、推理内容、
    /// 注释、供应商字段、第二个 choice……）里是 `unk…` 标签。
    fn random_upstream_response(seed: u64) -> (Vec<u8>, usize) {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let (mut checked, mut unchecked) = (0usize, 0usize);
        let mut chk = || {
            checked += 1;
            format!("chk{checked}tag")
        };
        let mut unk = || {
            unchecked += 1;
            format!("unk{unchecked}tag")
        };
        let mut message = Map::new();
        message.insert("role".into(), json!("assistant"));
        match rng.below(3) {
            0 => {
                message.insert("content".into(), json!(chk()));
            }
            1 => {
                message.insert("content".into(), Value::Null);
            }
            _ => {
                message.insert(
                    "content".into(),
                    json!([{"type": "text", "text": chk()}, {"type": "refusal", "refusal": chk()}]),
                );
            }
        }
        if rng.below(2) == 0 {
            message.insert("refusal".into(), json!(chk()));
        }
        message.insert("reasoning_content".into(), json!(unk()));
        message.insert("reasoning".into(), json!(unk()));
        message.insert("annotations".into(), json!([{"title": unk()}]));
        message.insert("audio".into(), json!({"data": unk()}));
        let calls: Vec<Value> = (0..rng.below(3))
            .map(|n| {
                let arguments = if rng.below(2) == 0 {
                    json!(serde_json::to_string(&json!({chk(): chk()})).unwrap())
                } else {
                    json!({chk(): chk()})
                };
                json!({"id": unk(), "type": "function", "index": n, unk(): unk(),
                       "function": {"name": chk(), "arguments": arguments, unk(): unk()}})
            })
            .collect();
        if !calls.is_empty() {
            message.insert("tool_calls".into(), Value::Array(calls));
        }
        let body = json!({
            "id": unk(), "object": unk(), "created": 1_700_000_000u64, "model": unk(),
            "system_fingerprint": unk(), "provider": unk(),
            "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3,
                      "cost": 0.5, "prompt_tokens_details": {"cached_tokens": 1, unk(): unk()}},
            "choices": [
                {"index": 0, "finish_reason": unk(), "logprobs": {"content": unk()},
                 "message": Value::Object(message)},
                {"index": 1, "message": {"role": "assistant", "content": unk()}},
            ],
        });
        (body.to_string().into_bytes(), unchecked)
    }

    #[test]
    fn every_string_the_client_receives_is_checked_text_or_written_by_the_server() {
        // 性质：重建后的响应里，凡是上游写的字符串，要么是会被检查的文本（回答、拒绝语、工具名和参数），要么不在里面；
        // 不会被检查的位置（`id`、`model`、工具调用 `id`、推理内容、注释、供应商字段、第二个 choice）里的任何东西都到不了客户端
        for seed in 1..=300u64 {
            let (upstream, _) = random_upstream_response(seed);
            let rebuilt = rebuild_response(&upstream, &ident("req-r3"), 1 << 20).unwrap();
            let text = String::from_utf8(rebuilt.body.clone()).unwrap();
            assert!(
                !text.contains("unk"),
                "seed {seed}: an unchecked string leaked: {text}"
            );
            // 反过来：每个会被检查的标签都在发给客户端的响应里，也在检查的文本里
            let checked_texts = rebuilt.output.all_texts().join("\n");
            let mut tag = 1;
            while String::from_utf8_lossy(&upstream).contains(&format!("chk{tag}tag")) {
                let needle = format!("chk{tag}tag");
                assert!(
                    text.contains(&needle),
                    "seed {seed}: {needle} was dropped: {text}"
                );
                assert!(
                    checked_texts.contains(&needle),
                    "seed {seed}: {needle} was forwarded but not checked"
                );
                tag += 1;
            }
            assert!(tag > 1 || text.contains("\"content\":null"), "seed {seed}");
            // 服务端写的标识
            let value: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["id"], "req-r3");
            assert_eq!(value["model"], "example-org/chat-70b");
            if let Some(calls) = value["choices"][0]["message"]["tool_calls"].as_array() {
                for (index, call) in calls.iter().enumerate() {
                    assert_eq!(call["id"], format!("call_reqr3_{index}"));
                }
            }
        }
    }

    #[test]
    fn only_the_first_choice_is_forwarded() {
        let upstream = json!({"choices": [
            {"message": {"role": "assistant", "content": "FIRST"}},
            {"message": {"role": "assistant", "content": "SECOND-UNCHECKED"}}]})
        .to_string();
        let rebuilt = rebuild_response(upstream.as_bytes(), &ident("r"), 1 << 20).unwrap();
        let text = String::from_utf8(rebuilt.body).unwrap();
        assert!(text.contains("FIRST") && !text.contains("SECOND"));
    }
}

//! idoris.ai/api 计算服务 **S0：契约冻结**。
//!
//! 权威设计稿：`docs/design/compute-service-design.md`（定稿 1.0）；每条约束为什么存在，见
//! `docs/design/compute-service-codex-review.md`。本 crate 只有**类型、校验、状态机、纯函数和测试**，
//! 没有存储、HTTP、账本、任何接线（那是 S1–S7）。S1–S7 的实现者拿到的是一组已经被测试钉住的契约：
//! 照着调用，不要在别处重新实现这些规则。
//!
//! # 为什么是一个独立的 crate
//!
//! `idoris-contracts` 是 `packages/contracts/schema/*.schema.json` 的 Rust 投影，每个类型都挂在
//! `SchemaShape` 和漂移门上；计算服务契约没有 JSON Schema 真源，也不属于 wire 1.1.0。许可表要从 YAML 加载，
//! 需要 `serde_yaml`，不该给被所有 crate 依赖的 `idoris-contracts` 增加依赖。独立 crate 还让「不改现有契约的
//! 任何序列化」成为物理上的保证：本 crate 单向依赖 `idoris-contracts`（只为 `DeployMode`），反向没有，
//! `idoris-contracts` 一行没改。
//!
//! # 模块地图
//!
//! | 模块 | 设计稿 | 内容 |
//! |---|---|---|
//! | [`ids`] | §3 | id 的 newtype（账户 id 传给要成员 id 的函数编译不过）、`AttemptId`、`EndpointKey`、`TimestampMs`、`BillingMonth` |
//! | [`money`] | §4 | `Unit`（THB / USD / `QUOTA:<节点>`）、微单位整数的 `Money`、显式带汇率的 `convert`、对外展示取整 |
//! | [`pricing`] | §4 | `Cost`（上游成本）与 `Price`（对用户的价）两个不同类型、`RateCard`、`Usage`、用量计价 |
//! | [`grant`] | §4 | `Grant`：每笔金额带用途、有效期、不可转让 |
//! | [`account`] | §3 | `Principal` / `Membership` / `Account` / `AccountTree` / `Directory` / `Allocation`，以及 `validate_allocation` |
//! | [`credential`] | §3、§9 | `Credential`、`resolve_payment_context`、`AdminScope`、`check_scope_delegation`、`check_credential_issuance` |
//! | [`reason`] | §5 | 16 个稳定原因码与四级体验分级 |
//! | [`reserve`] | §5 | 预留额公式 `reserve`、压低 `max_tokens` 的 `fit_max_tokens` |
//! | [`admission`] | §5 | 五项检查的输入输出、`evaluate_admission`、准入决定、`check_grace` |
//! | [`attempt`] | §6 | 单次尝试的状态机与 49 格转移表 |
//! | [`request`] | §6 | `Request`（一组尝试、重新准入、发布边界）、`settle_attempts`（谁付多少）、`split_overage` |
//! | [`permit`] | §8 | 端点级许可表：YAML schema、加载校验、`select_compliant` 硬约束过滤 |
//! | [`form`] | §9 | `DeploymentForm`（个人 / 组织 / 云）及能力开关，与既有 `deploy_mode` 共存 |
//! | [`safety`] | §7 | S6 新增（加法，没有改动上面任何一个字符串）：儿童安全的 3 个稳定原因码、拦截类别、检查阶段、「检查没能给出结论」的原因 |
//!
//! # 全 crate 的约定
//!
//! - **金额**：微单位整数，永不用浮点；不同单位的加、减、比较一律返回错误（不 panic、不静默换算）；
//!   溢出一律返回错误。`Money` 故意没有 `Add` / `PartialOrd`，`a + b`、`a < b` 写不出来。
//!   用量计价整笔只取一次整，默认向上（宁多 1 微单位，不少算）。
//! - **时间**：没有任何函数读系统时钟，所有依赖「现在」的函数都显式收 `now: TimestampMs`。
//!   所有有效期都是**半开区间** `[from, until)`：刚好等于 `until` 的那一毫秒已经过期。
//! - **反序列化**：未知字段一律拒绝，未知枚举值一律拒绝，没有任何地方容忍（与 `idoris-contracts` 的
//!   `deny_unknown_fields` 和 TS 侧 zod `.strict()` 一致）。带不变式的类型（`Grant`、`Request`、
//!   `AccountTree`……）反序列化时会重新校验，从存储里读出来的坏数据进不来。
//! - **不 panic**：`unwrap` / `expect` 在工作区 lint 里是 deny，本 crate 非测试代码没有任何绕过。
//! - **线上字符串不可变**：所有原因码、状态名、事件名、取值的字符串都被 `tests/wire_snapshots.rs`
//!   的固定快照和各模块的逐字测试钉住。新增只能加变体，不能改旧字符串。
//!
//! # 设计稿的不变式 → 在哪里强制 → 哪里测试
//!
//! | 不变式 | 强制点 |
//! |---|---|
//! | 不同单位不能加减比较；溢出有定义 | `money::Money::{checked_add, checked_sub, try_cmp}` |
//! | 换算必须带汇率和取值时间；配额不可兑现 | `money::{FxRate, convert}`（`FxRate::new` 拒绝 `QUOTA`） |
//! | `cost` 与 `price` 不能互相赋值 | `pricing::{Cost, Price}` 是不同类型（`compile_fail` 文档测试） |
//! | 每笔金额不可转让 | `grant::NonTransferable`（反序列化 `true` 报错） |
//! | 每次请求只落到一个付款上下文；由凭证决定，请求不能指定付款方 | `credential::{CredentialBinding, resolve_payment_context}` |
//! | 下发的额度 / 权限不能超过授权者本人 | `account::validate_allocation`、`credential::{check_scope_delegation, check_credential_issuance}` |
//! | 账户树无环、深度有上限 | `account::AccountTree::build`（`MAX_ACCOUNT_DEPTH`） |
//! | 预留 = max_tokens × 输出价 + 输入估算，带硬上限 | `reserve::{reserve, fit_max_tokens}` |
//! | 准入沿所有祖先同时检查五项 | `admission::evaluate_admission` |
//! | 宽限资格独立计算，不依赖 overage | `admission::check_grace` |
//! | 合法转移才能发生；`maybe_executed` 没有无证据的出口 | `attempt::transition` |
//! | 切换候选前要重新准入 | `request::Request::begin_next_attempt` 要求放行类的 `AdmittedCandidate` |
//! | 首字节之后禁止透明重试或切换 | `request::Request::{publish_first_byte, begin_next_attempt}` |
//! | 用户只付一次；额外尝试的成本记给平台；`maybe_executed` 成本保留待核销 | `request::settle_attempts` |
//! | `child_edu` 只能落到 ZDR + 已签 DPA；核验过期自动停用；没有合规候选就拒绝 | `permit::select_compliant` |
//! | 云形态排除 subscription；个人 / 组织 / 云的能力开关 | `form::DeploymentForm::profile`、`permit::select_compliant` |
//!
//! # 给 S1–S7 实现者
//!
//! - **S1**（账户存储）：把库里的行读成 `Account` / `Principal` / `Membership`，用 `AccountTree::build` 和
//!   `Directory::build` 校验；存储里的 `Request` 记录经 `serde` 读回来会重新校验。`Directory` 是校验过的内存
//!   快照、查找是线性的：热路径上按请求取一个自洽的小切片（付款账户的祖先链 + 该主体的成员关系）再构造即可。
//!   既有预算里的 `*_minor` 整数没有币种；**确定了币种之后**用 `money::Money::from_minor` 换成微单位，不要自己乘。
//! - **S2**（控制面）：用 `validate_allocation`、`check_credential_issuance`；免费额度的幂等键用
//!   `account::subsidy_idempotency_key`。
//! - **S3**（准入）：在事务里读出 `tree.ancestors(payer)` 每个账户的计数器，填 `AncestorView`，调
//!   `evaluate_admission`；用 `AdmissionReport::headroom` 喂 `fit_max_tokens`；决定本身（什么时候降级、
//!   什么时候用宽限）由你写，用 `AdmissionDecision` 表达；宽限资格用 `check_grace`。
//! - **S4**（云供应商）：每次尝试用 `Request::apply` 推进状态机；**先记 `WireWriteStarted` 再写字节**；
//!   拿到供应商的请求号就 `Request::record_supplier_request_id`（恢复后对账用，见
//!   `docs/design/compute-service-ops.md` §7）；路由用 `permit::select_compliant`，兜底候选走同一个函数。
//! - **S5**（云入口）：`credential::resolve_payment_context` 得到付款上下文，原因码映射见
//!   `ResolveError::reason_code`；形态判定用 `form`。
//! - **S6**（儿童安全）：`edu` 租户的 `TenantDataPolicy` 含 `child_edu`；本 crate **不**强制「edu 一律非流式」
//!   与安全检查（那是路由层 `idoris-router::cloud::safety` 的事），只提供 S6 新增的稳定字符串：
//!   [`safety::SafetyReasonCode`] / [`safety::SafetyCategory`] / [`safety::SafetyUnavailableCause`]，纯加法。
//! - **S7**（云端档适配器）：`form::DeploymentForm`、`request::Request`。
//!
//! 命名提醒：`pricing::Price` 与 `idoris_tenancy::budget::Price` 同名但无关；同时引用时请用模块路径。
//!
//! # 设计稿里含糊或自相矛盾、本 crate 暂定的理解
//!
//! 没有悄悄拍板：每一条要么在对应模块的文档里注明，要么就写在这里，并列在交付说明里等 owner 确认。
//!
//! 1. §4 `QUOTA` 的精度没写：统一微单位，展示取整数个 token 当量。
//! 2. §4「有效期」：必填，没有「永不过期」。
//! 3. §3 账户树：同一棵树单一单位；最大深度 5；平台可直接挂个人成员；补贴池可以是组织的上级。
//! 4. §3 额度是授权上限：单笔下发不超过授权者本人，**各子额度之和可以超过**父额度（共用一份余额，由祖先检查兜底）。
//! 5. §4 幂等键 `(成员, 月份)`：成员取主体 id，不是成员关系 id。
//! 6. §5 授信与宽限：「钱够不够」= `Funds` 通过或被授信救回；宽限只在「钱不够」时可用，且要求账户尚未透支；
//!    宽限那一轮超出可用额度的部分由谁最终承担，设计稿没写，S3 定。
//! 7. §5 `budget_exhausted` 与既有 402 体里的 `budget_exceeded` 是两个字符串，并存；响应头名不在 S0。
//! 8. §6 状态机图里只有 `sending → not_sent`，本 crate 另外允许 `admitted → not_sent` 和
//!    「上游 4xx 明确拒绝」从 `maybe_executed` / `sending` 到 `not_sent`；5xx、超时都**不能**证明没执行。
//! 9. §6 从 `maybe_executed` 在首字节前切换到下一个候选是允许的（否则「额外尝试的成本由平台承担」无从谈起）；
//!    切换时上一次尝试的价预留何时释放（切换时还是结算时）设计稿没写，S0 只定义最终结算。
//! 10. §6 一次请求最多 4 次尝试；没有尝试给出回答（没有发布）时用户付 0。
//! 11. §8 核验过期的天数是表级策略 `verification_max_age_days`（1..=365）；没有 `enabled` 字段，从表里删掉即停用；
//!     处理区域是集合，必须**全部**被租户允许；区域是与供应商无关的小写标签（`sg`、`sg-1`……），契约里没有任何
//!     厂商专属的区域名、枚举或默认值（`tests/no_vendor_specific_names.rs` 守着这条）。
//! 12. §9 存量的 `deploy_mode=tenant` 没有 `form` 时按组织形态处理；云形态的「补贴额度」按货币单位 +
//!     `FreeTier` 用途处理，不是 `QUOTA`。
//! 13. §8 调研里的「没核实」：契约没有「未知」取值，规则是没核实的值一律写成最坏的那个
//!     （`allowed_data_classes: []`、`retained`、`unsigned`、能力 `false`、区域 `unknown`），表读出来永远不会比
//!     真实情况更宽松；所以 `allowed_data_classes` 允许为空列表（这样的端点永远选不中）。见 `permit` 模块文档。
//! 14. §6 每次尝试的字段列表里没有「供应商请求号」，但 §10 的恢复后对账需要一个能和供应商账单对上的键
//!     （ops 文档 §7.1 也列了 `supplier_request_id`），所以它是 `Attempt` 上的可选字段，不是状态转移。

pub mod account;
pub mod admission;
pub mod attempt;
pub mod credential;
pub mod form;
pub mod grant;
pub mod ids;
pub mod money;
pub mod permit;
pub mod pricing;
pub mod reason;
pub mod request;
pub mod reserve;
pub mod safety;
#[cfg(test)]
pub(crate) mod test_support;

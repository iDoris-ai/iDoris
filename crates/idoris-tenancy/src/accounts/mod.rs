//! 账户存储（计算服务 S1，设计稿 §3、§4、§9）：主体、成员关系、付款账户（额度委派树）、凭证、额度授权。
//!
//! 这是**存储层**：只有读写函数和它们的校验，没有 HTTP 接口（控制面是 S2）、没有沿祖先的准入检查（S3）。
//! 类型都是 S0 契约 crate（`idoris-compute-contracts`）里冻结的，这里不另造一套：账户、成员关系、凭证、额度
//! 就是 `Account` / `Membership` / `Credential` / `Allocation`，金额是 `Money`（单位 + 微单位整数），
//! 库里存「单位列 + 整数列」，没有浮点。校验用 S0 的函数（`AccountTree::build`、`Directory::build`、
//! `validate_allocation`……），不在这里重新实现规则；数据库层再用外键、唯一索引、CHECK 和触发器拦一遍坏数据。
//!
//! 只在组织、云形态打开（`IDORIS_FORM`）。个人形态和存量 tenant 部署不创建这些表。
//!
//! 入口：[`AccountStore`]。写入、读取和各自遵守的规则见 `store.rs` 的模块文档；表结构和约束见
//! `migrations/0001_accounts.sql` 的注释。

mod control;
mod control_reads;
mod error;
/// S3 准入账本：沿付款账户所有祖先的五项原子检查、预留 / 结算 / 释放 / 过期（只追加的分录）、有担保的负值与宽限、
/// 免费额度发放。是 [`AccountStore`] 上的一组方法，说明见模块文档。
pub mod ledger;
mod pairing;
mod rows;
mod store;

#[cfg(test)]
mod control_tests;
#[cfg(test)]
mod pairing_tests;
#[cfg(test)]
mod tests;

pub use control::{
    AuditSpec, CallerInfo, CreateAccount, CreateMembership, CreatePrincipal, Failure,
    GrantAllocation, IssueCredential, KeyCeilings, Meta, RevokeAllocation, RevokeMembership,
    RevokePrincipal, SetAccountStatus, SetCredentialStatus, TargetKind, Who, Written,
};
pub use control_reads::{
    AccountView, AllocationsView, AuditRow, AuditView, CredentialView, CredentialsView, DeviceView,
    MAX_READ_ROWS, MembershipsView, Subtree,
};
pub use error::{AccountStoreError, PaymentContextError};
pub use pairing::{
    CreatePairingCode, DEFAULT_PAIRING_TTL_MS, GateVerdict, MAX_ACTIVE_DEVICES_PER_MEMBERSHIP,
    MAX_DEVICE_LABEL_CHARS, MAX_PAIRING_TTL_MS, MIN_PAIRING_TTL_MS, PairedDevice, PairingRecord,
    PairingStatus, RedeemPairingCode, RevokePairingCode,
};
pub use rows::AllocationRecord;
pub use store::{AccountStore, AllocationOutcome};

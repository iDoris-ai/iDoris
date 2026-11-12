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

mod error;
mod rows;
mod store;

#[cfg(test)]
mod tests;

pub use error::{AccountStoreError, PaymentContextError};
pub use store::{AccountStore, AllocationOutcome};

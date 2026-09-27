//! R2-B：纯函数的准入与路由决策管道。**没有 IO，没有随机数**。
//!
//! TS 参考实现（porting 时的行为基准）：`packages/router/src/{policy,dispatch,
//! locality,registry,roles}.ts` + `packages/tenancy/src/budget.ts`。这些文件
//! 目前在 `main` 分支；本 crate 所在的 Rust 骨架分支线尚未合并它们，读取时用
//! `git show origin/main:<path>` 而不是假设它们在本分支的工作区里存在。
//!
//! 目前有决策候选类型 [`Card`] 和完整的 `role` 模块（解析 + 目录候选筛选）；
//! 隐私/预算/管道逻辑在后续 PR 里加入（这个 crate 按依赖顺序叠加的多个 PR
//! landing，见各 PR 描述）。

pub mod card;
pub mod role;

pub use card::{AdmissionStatus, Card};
pub use role::{ROLES, Role, RoleParseError, is_eligible_for_role, parse_model_role};

//! 决策管道：数据类型定义在 [`types`]；管道逻辑（`decide`/`budget_stage`/
//! `pick`）在后续 PR 里加入这个文件。

mod types;

pub use types::{Decision, Degradation, PolicyCtx, ReasonCode, Rejection, RequestProfile, Stage};

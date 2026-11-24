//! 组件卡集合的**启动前校验**，`serve` 启动与 `setup` 落盘前共用（#M3）。
//!
//! serve 在加载完卡之后还有几道纯配置校验会让它退出 1：引擎型上游的种类解析（[`crate::readiness::plan_upstreams`]）、
//! 托管卡的计划（[`crate::lifecycle::validate_managed_plan`]：endpoint 形状、引擎种类、端口不重复、每种引擎只托管一个实例）。
//! `setup` 在暂存目录里对「保留的卡加新卡」只过了加载器，没有过这几道，结果是 `setup` 成功、serve 起不来（例如目录里已有
//! 一张 id 与端口都不同的托管 oMLX 卡，再 `setup omlx --force --managed`：同一种引擎两个实例）。这里把它们合成一个函数，
//! `setup` 在发布之前对暂存目录里的卡集合调用一次，错误就是 serve 会报的那一条。
//!
//! 这里只放纯配置校验：不读环境、不碰文件系统、不探测引擎、不构造 backend。要读环境或状态目录的检查（例如托管引擎需要
//! `IDORIS_STATE_DIR`）是运行环境的事，不属于卡集合本身，不在这里。

use idoris_contracts::ComponentCard;

use crate::lifecycle::{managed_backend_kind, validate_managed_plan};
use crate::readiness::plan_upstreams;

/// 对（已通过加载器的）卡集合做 serve 启动时的纯配置校验；失败时返回 serve 会打印的那句话。
pub fn validate_card_set(cards: &[ComponentCard]) -> Result<(), String> {
    // 运行时对卡 `form` 的确定拒绝（serve 在构造运行时注册表时报，托管时在完整 Router 创建时报）。
    for card in cards {
        crate::runtime::check_runtime_form(card)?;
    }
    let plans = plan_upstreams(cards).map_err(|error| error.to_string())?;
    validate_managed_plan(&plans, managed_backend_kind).map_err(|error| error.to_string())?;
    Ok(())
}

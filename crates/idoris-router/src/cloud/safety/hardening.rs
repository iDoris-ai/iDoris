//! 进程级「不落盘」加固（设计稿 §7：「反向代理、错误采样、崩溃转储、安全分类器、Modal 日志都在『不落盘』的范围内」）。
//!
//! 进程里的正文（孩子的输入、模型的回答）只在内存里。内存里的东西有两条路会被写到磁盘或日志里，业务代码管不到：
//!
//! | 路 | 怎么写出去 | 这里怎么堵 |
//! |---|---|---|
//! | **panic 信息** | Rust 默认的 panic hook 把 panic 的 payload 打到 stderr。`unwrap()` 一个 `Err(e)` 或 `assert_eq!(left, right)` 会把值的 `Debug` 放进信息里——里面可能就是正文 | [`install_panic_redaction`]：换成只打位置（文件:行:列）的 hook，**不打 payload** |
//! | **崩溃转储（core dump）** | 进程崩溃时操作系统把整个地址空间（含内存里的正文）写成 core 文件 | [`disable_core_dumps`]：把 `RLIMIT_CORE` 的软硬限制都设成 0（硬限制降下去就拉不回来） |
//!
//! 只有进程里配置了处理儿童数据的租户时才需要（[`harden_process_for_child_data`]）：个人形态和非 edu 云租户的 panic 信息、
//! core dump 行为一字不变（开发者需要看到 panic 的完整信息）。
//!
//! 这两样是**进程**层面的。进程**外面**的落盘（反向代理的访问日志和错误日志、容器平台的崩溃上报、`kubectl logs` 的采集、
//! 备份）不在代码里：上线验收清单见交付说明（反向代理不得开启请求体日志，错误采样不得采集请求体，
//! 备份只含数据库——数据库里没有正文，见 `tests/cloud_child_safety_scan.rs`）。

use std::panic::PanicHookInfo;

/// panic 报告：只有位置，**没有 payload**。
pub fn redacted_panic_report(info: &PanicHookInfo<'_>) -> String {
    match info.location() {
        Some(location) => format!(
            "[idoris] panic at {}:{}:{} (the message is withheld: this process serves child-data tenants)",
            location.file(),
            location.line(),
            location.column()
        ),
        None => "[idoris] panic at an unknown location (the message is withheld: this process serves child-data tenants)".to_string(),
    }
}

/// 把 panic hook 换成只打位置的版本。
pub fn install_panic_redaction() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("{}", redacted_panic_report(info));
    }));
}

/// 关闭 core dump：`RLIMIT_CORE` 的软硬限制都设成 0。失败返回错误（调用方应当拒绝启动）。
#[cfg(unix)]
pub fn disable_core_dumps() -> Result<(), String> {
    use nix::sys::resource::{Resource, setrlimit};
    setrlimit(Resource::RLIMIT_CORE, 0, 0)
        .map_err(|error| format!("无法关闭 core dump（setrlimit RLIMIT_CORE）：{error}"))
}

/// 非 Unix 平台没有等价的、进程内可靠的办法：处理儿童数据的部署只支持 Unix，拒绝启动。
#[cfg(not(unix))]
pub fn disable_core_dumps() -> Result<(), String> {
    Err("处理儿童数据的部署只支持 Unix：无法在本平台关闭崩溃转储".to_string())
}

/// 当前 `RLIMIT_CORE` 的 (软, 硬) 限制（测试用）。
#[cfg(unix)]
pub fn core_dump_limits() -> Result<(u64, u64), String> {
    use nix::sys::resource::{Resource, getrlimit};
    getrlimit(Resource::RLIMIT_CORE)
        .map(|(soft, hard)| (soft as u64, hard as u64))
        .map_err(|error| error.to_string())
}

/// 进程里有处理儿童数据的租户时调用：panic 信息脱敏 + 关闭 core dump。
pub fn harden_process_for_child_data() -> Result<(), String> {
    disable_core_dumps()?;
    install_panic_redaction();
    Ok(())
}

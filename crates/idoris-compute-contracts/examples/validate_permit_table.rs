//! 校验一份端点许可表 YAML，并列出每个端点此刻是否有效。给 S8 的数据产出者自检用：
//!
//! ```text
//! cargo run -p idoris-compute-contracts --example validate_permit_table -- <file.yaml> [now]
//! ```
//!
//! `now` 可选，RFC 3339 或 `YYYY-MM-DD`；不给就用系统时钟（库本身从不读系统时钟，只有这个示例读）。
//! 校验失败以非 0 退出码结束，并打印是哪一个端点、哪一条规则。
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use idoris_compute_contracts::ids::TimestampMs;
use idoris_compute_contracts::permit::{PermitTable, format_verified_at, parse_verified_at};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: validate_permit_table <file.yaml> [now]");
        return ExitCode::from(2);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("cannot read {path}: {err}");
            return ExitCode::from(2);
        }
    };
    let now = match args.next() {
        Some(raw) => match parse_verified_at(&raw) {
            Ok(ts) => ts,
            Err(err) => {
                eprintln!("bad now {raw:?}: {err}");
                return ExitCode::from(2);
            }
        },
        None => {
            let ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
                .unwrap_or(0);
            TimestampMs(ms)
        }
    };
    let table = match PermitTable::from_yaml_str(&text) {
        Ok(table) => table,
        Err(err) => {
            eprintln!("INVALID {path}: {err}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "OK {path}: {} endpoint(s), verification_max_age_days = {}",
        table.len(),
        table.verification_max_age_days()
    );
    println!(
        "evaluated at {}",
        format_verified_at(now).unwrap_or_else(|| now.millis().to_string())
    );
    println!("（fresh = 核验未过期；是否会被某个请求选中，还要看数据类别、留存、DPA、区域、能力）");
    for entry in table.endpoints() {
        let state = if table.is_verified_at(entry, now) {
            "fresh"
        } else {
            "STALE"
        };
        let expires = table
            .expires_at(entry)
            .and_then(format_verified_at)
            .unwrap_or_else(|| "?".to_string());
        let classes: Vec<String> = entry
            .allowed_data_classes
            .iter()
            .map(|c| format!("{c:?}"))
            .collect();
        let permitted = if classes.is_empty() {
            "NONE (never selected)".to_string()
        } else {
            classes.join(",")
        };
        println!(
            "  {state:5} {}  verification expires {expires}; data classes: {permitted}",
            entry.key()
        );
    }
    ExitCode::SUCCESS
}

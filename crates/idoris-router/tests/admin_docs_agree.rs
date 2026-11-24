//! Codex 预审（第 4 层）：README 与接口规范 §2.11.4 对 Admin `upstreams` 写接口说的，和代码做的是同一件事。
//!
//! 这是一份**文字的**回归测试：代码一侧的行为各有自己的 HTTP 测试钉着（冻结的上游的 `restart` 是 409 `upstream_not_owned`、`start` 是
//! 409 `restart_required`：`lifecycle_admin::a_frozen_upstream_reads_restart_required_and_refuses_start_while_stop_and_restart_are_not_owned`；
//! 未确认的停止：`m4_…`；`restart` 做完之后的重传是新的重启：`l4_…`；不是 UTF-8 的 `{id}`：`admin::upstreams` 的单元测试）。这里只看文档
//! 有没有重新说出与它们相反的话，以及纠正之后的几句是不是还在。它读文件，不起任何进程。
#![allow(clippy::unwrap_used)]

use std::path::Path;

const README: &str = "README.md";
const SPEC: &str = "docs/interfaces/iDoris-Agent24-边界与接口规范.md";

fn read(relative: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read_to_string(root.join(relative))
        .unwrap_or_else(|error| panic!("cannot read {relative}: {error}"))
}

/// L2：文档曾经说冻结的上游的 `start`、`restart` 都是 409 `restart_required`，而代码（和规范自己「`owned` 为 false 的 `stop`、`restart` 一律 409
/// `upstream_not_owned`」那一条）给 `restart` 的是 `upstream_not_owned`。按旧说法写的调用方会漏掉正确的恢复提示。
#[test]
fn the_docs_do_not_promise_restart_required_for_a_frozen_upstreams_restart() {
    for path in [README, SPEC] {
        let text = read(path);
        for stale in [
            "`start`、`restart` 是 409 `restart_required`",
            "冻结的上游 `start`、`restart` 是 409",
            "冻结的上游的 `start`、`restart`）",
        ] {
            assert!(
                !text.contains(stale),
                "{path} says {stale:?} again: a frozen upstream's restart is 409 upstream_not_owned (it has no engine of its own)"
            );
        }
    }
}

#[test]
fn the_docs_carry_the_corrected_contract_of_the_write_endpoints() {
    let readme = read(README);
    let spec = read(SPEC);
    for (name, text, phrases) in [
        (
            "README",
            &readme,
            vec![
                // L2：只有 `start` 得到 `restart_required`，`restart`、`stop` 是 `upstream_not_owned`，调用方读 `GET` 的字段。
                "`start` 是 409 `restart_required`",
                "`stop`、`restart` 一律是 409 `upstream_not_owned`",
                "读 `GET` 元素的 `restart_required` 字段",
                // M1：202 体是受理那一刻的快照。
                "受理那一刻自己的快照",
                // M4：完成判据与「未确认」。
                "`state` 是 `stopped` **并且** `owned` 是 false 才算做完",
                "没能确认进程已经消失",
                // L4：重传。
                "`restart` 的幂等只在第一次操作进行期间成立",
                // L1、M3。
                "400 `invalid_upstream_id`",
                "没有被实例任务认领是 503 `idoris_stopping`",
            ],
        ),
        (
            "spec",
            &spec,
            vec![
                "`restart_required` 只出现在冻结上游的 `start` 上",
                "冻结的上游的 `start` 是 409 `restart_required`",
                "读 `GET` 元素的 `restart_required` 字段",
                // M1、M3。
                "是实例任务**自己**在受理那一刻的快照",
                "抢同一个决定",
                // M2。
                "把归属与并入**一起**决定",
                // M4。
                "`stop` 要看 `state` 是 `stopped` **并且** `owned` 是 false",
                "**未确认**",
                // L4、L1、L3。
                "只在第一次操作**进行期间**成立",
                "400 `invalid_upstream_id`",
                "一条审计只占一行",
            ],
        ),
    ] {
        for phrase in phrases {
            assert!(text.contains(phrase), "{name} lost: {phrase}");
        }
    }
}

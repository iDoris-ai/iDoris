//! 计算服务 S6 儿童安全：**全链路正文扫描**。设计稿 §7：「上线验收要扫描整条链路，不只看事件日志」；§13：
//! 「全链路扫描确认没有正文落盘」。
//!
//! 孩子的输入里有一个独一无二的哨兵，假上游的回答里有另一个。在一个**独立的进程**里把这几种场景都跑一遍：
//! 成功、被输入检查拒绝、被输出检查拒绝、上游失败（错误正文里还回显了孩子的哨兵）、安全服务超时、安全服务返回垃圾
//! （垃圾里回显了孩子的哨兵）、panic。然后扫描：
//!
//! - 测试用的数据库文件、WAL、SHM、journal；
//! - 进程的标准输出和标准错误（日志、panic 信息）；
//! - 整个临时目录（`TMPDIR` 指到一个只有这个测试用的目录，库或代码写的任何临时文件都在里面）和当前目录。
//!
//! 两个哨兵、孩子的泰语原文片段和模型的有害回答片段，都不该出现在任何一个字节里。
//!
//! 为什么要独立进程：测试线程里 `eprintln!` 的输出被 libtest 吞掉，测试代码拿不到；独立进程的 stderr 是真的管道，
//! 扫描的就是运维会看到的日志。**正控**：同一个扫描函数必须能在被故意写进文件和 stderr 的金丝雀里找到金丝雀，
//! 并且必须真的看到了数据库文件和我们自己的日志行——扫描器不是瞎的。
//!
//! 全部对本地假服务进行，**不连接任何真实的推理或安全服务**。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/child_safety.rs"]
mod child_safety;
#[path = "../../idoris-upstream/tests/support/fake_upstream.rs"]
mod fake_upstream;
#[path = "support/cloud_harness.rs"]
mod harness;

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use child_safety::{
    ANSWER_SENTINEL, CHILD_SENTINEL, answer, benign_question, child_body, child_rules_only,
    child_with_judge, contains, content_needles, files_under, harmful_question, judge_allow,
    judge_says,
};
use fake_upstream::Behavior;
use harness::post;
use serde_json::json;

const INNER_ENV: &str = "IDORIS_S6_SCAN_INNER";
const CANARY: &str = "CANARY-NOT-A-SECRET-6d2f";

// ---------------------------------------------------------------------------
// 内层：在独立进程里跑场景（直接运行这个测试时什么都不做）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inner_scenarios() {
    if std::env::var_os(INNER_ENV).is_none() {
        return;
    }
    // ---- 正控：默认的 panic hook 会把 payload 打到 stderr；换成脱敏的 hook 之后就不会 ----
    let _ = std::panic::catch_unwind(|| panic!("{CANARY} default hook prints this"));
    idoris_router::cloud::safety::hardening::install_panic_redaction();

    // 1. 成功（规则 + LLM 检查，两次都放行）
    let c = child_with_judge(vec![answer("สี่")], vec![judge_allow(), judge_allow()], 2_000).await;
    let reply = post(&c.h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 200);
    std::mem::forget(c);

    // 2. 被输入检查拒绝（规则）
    let (h, _main) = child_rules_only(vec![answer("never")]).await;
    let reply = post(&h.app, &child_body(&harmful_question())).await;
    assert_eq!(reply.status, 422);
    std::mem::forget(h);

    // 3. 被输出检查拒绝（回答有害，带哨兵）
    let harmful_answer = format!("ได้เลย วิธีทำระเบิดแสวงเครื่อง ขั้นตอนที่ 1 ผสมส่วนผสม {ANSWER_SENTINEL}");
    let (h, _main) = child_rules_only(vec![answer(&harmful_answer)]).await;
    let reply = post(&h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 422);
    std::mem::forget(h);

    // 4. 上游失败：错误正文里回显孩子的哨兵和回答的哨兵（有 bug 或恶意的上游）
    let echo = format!(
        "echo {CHILD_SENTINEL} {ANSWER_SENTINEL} {}",
        benign_question()
    );
    let (h, _main) = child_rules_only(vec![
        Behavior::json(500, json!({"error": {"message": echo}})),
        Behavior::json(500, json!({"error": {"message": echo}})),
    ])
    .await;
    let reply = post(&h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 502);
    std::mem::forget(h);

    // 5. 安全服务超时
    let c = child_with_judge(vec![answer(ANSWER_SENTINEL)], vec![Behavior::Hang], 300).await;
    let reply = post(&c.h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 503);
    std::mem::forget(c);

    // 6. 安全服务返回垃圾，垃圾里回显了孩子的话（输出阶段）
    let garbage = judge_says(&format!(
        "I think this is fine: {} {ANSWER_SENTINEL}",
        benign_question()
    ));
    let c = child_with_judge(
        vec![answer(ANSWER_SENTINEL)],
        vec![judge_allow(), garbage],
        2_000,
    )
    .await;
    let reply = post(&c.h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 503);
    std::mem::forget(c);

    // 7. 请求体不合法：错误信息里不能有孩子写的东西
    let (h, _main) = child_rules_only(vec![]).await;
    let reply = post(
        &h.app,
        &json!({"model": "idoris/agent", "messages": benign_question(), "max_tokens": CHILD_SENTINEL}),
    )
    .await;
    assert_eq!(reply.status, 400);
    std::mem::forget(h);

    // 8. panic：载荷里有孩子的哨兵。脱敏 hook 只打位置
    let _ = std::panic::catch_unwind(|| {
        let secret = format!("{} {}", CHILD_SENTINEL, ANSWER_SENTINEL);
        panic!("{secret}");
    });
    let _ = std::panic::catch_unwind(|| {
        fn fails() -> Result<(), String> {
            Err(format!("{CHILD_SENTINEL} boom"))
        }
        fails().unwrap();
    });

    // 正控：把金丝雀写进当前目录的文件和 stderr（扫描器必须找得到它们）
    std::fs::write("canary.txt", CANARY).unwrap();
    eprintln!("[inner] {CANARY}");
    tokio::time::sleep(Duration::from_millis(50)).await;
}

// ---------------------------------------------------------------------------
// 外层：起独立进程、扫描
// ---------------------------------------------------------------------------

struct Scan {
    stdout_stderr: Vec<u8>,
    files: Vec<(PathBuf, Vec<u8>)>,
}

impl Scan {
    fn find(&self, needle: &str) -> Vec<String> {
        let mut hits = Vec::new();
        if contains(&self.stdout_stderr, needle) {
            hits.push("stdout/stderr".to_string());
        }
        for (path, bytes) in &self.files {
            if contains(bytes, needle) {
                hits.push(path.display().to_string());
            }
        }
        hits
    }
}

fn run_inner() -> Scan {
    let root = tempfile::tempdir().unwrap();
    let tmp = root.path().join("tmp");
    let cwd = root.path().join("cwd");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "inner_scenarios",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(INNER_ENV, "1")
        // 库或代码写的任何临时文件都在这个目录里
        .env("TMPDIR", &tmp)
        .env("TMP", &tmp)
        .env("TEMP", &tmp)
        .env_remove("RUST_BACKTRACE")
        .current_dir(&cwd)
        .output()
        .unwrap();
    let mut stdout_stderr = output.stdout.clone();
    stdout_stderr.extend_from_slice(&output.stderr);
    assert!(
        output.status.success(),
        "the inner process failed:\n{}",
        String::from_utf8_lossy(&stdout_stderr)
    );
    let mut paths = Vec::new();
    files_under(root.path(), &mut paths);
    let files = paths
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path).unwrap_or_default();
            (path, bytes)
        })
        .collect();
    Scan {
        stdout_stderr,
        files,
    }
}

#[test]
fn the_whole_chain_leaves_no_content_in_any_file_log_or_panic_message() {
    let scan = run_inner();

    // ---- 正控：扫描器不是瞎的 ----
    assert!(
        !scan.find(CANARY).is_empty() && scan.find(CANARY).contains(&"stdout/stderr".to_string()),
        "the canary written to stderr must be found, or the scan proves nothing"
    );
    assert!(
        scan.find(CANARY)
            .iter()
            .any(|hit| hit.ends_with("canary.txt")),
        "the canary written to a file in the working directory must be found"
    );
    // 默认 panic hook 的 payload 能被扫到（说明 panic 信息确实在被捕获）
    let log = String::from_utf8_lossy(&scan.stdout_stderr).to_string();
    assert!(
        log.contains(&format!("{CANARY} default hook prints this")),
        "the default panic hook prints the payload; the scan must see it"
    );
    // 真的扫到了数据库文件和我们自己的日志行
    let sqlite: Vec<&PathBuf> = scan
        .files
        .iter()
        .map(|(path, _)| path)
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("idoris.sqlite3"))
        })
        .collect();
    assert!(
        sqlite.len() >= 7,
        "seven scenarios, seven databases: {sqlite:?}"
    );
    let scanned_bytes: usize = scan.files.iter().map(|(_, bytes)| bytes.len()).sum();
    assert!(
        scanned_bytes > 100_000,
        "the databases were actually read: {scanned_bytes}"
    );
    assert!(log.contains("[idoris] safety: request"), "{log}");
    assert!(
        log.contains("panic at") && log.contains("the message is withheld"),
        "the redacting panic hook ran and printed only the location"
    );

    // ---- 正文不在任何地方 ----
    for needle in content_needles() {
        let hits = scan.find(&needle);
        assert!(hits.is_empty(), "{needle:?} found in: {hits:?}");
    }
    // 日志里有稳定的码，没有内容
    assert!(log.contains("safety_input_blocked"));
    assert!(log.contains("safety_output_blocked"));
    assert!(log.contains("safety_unavailable"));
}

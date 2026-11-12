//! 计算服务 S6：进程级「不落盘」加固（设计稿 §7：崩溃转储在「不落盘」范围内；S6 要求 panic 信息里不出现正文）。
//!
//! 这个文件只有一个测试：panic hook 和 `RLIMIT_CORE` 都是进程全局的，不能和别的测试并行。
//! 整个云配置打开、binary 启动之后的接线（启动时真的调用了加固）由 `cloud_child_safety_binary.rs` 在真实进程里验证。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::panic::PanicHookInfo;
use std::sync::Mutex;

use idoris_router::cloud::safety::hardening::{install_panic_redaction, redacted_panic_report};

const SECRET: &str = "CHILDINPUT-SENTINEL-hardening-77";

static REPORTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record(info: &PanicHookInfo<'_>) {
    REPORTS.lock().unwrap().push(redacted_panic_report(info));
}

#[test]
fn panic_reports_carry_the_location_and_never_the_payload_and_core_dumps_are_off() {
    // 用和真实 hook 同一个函数生成报告，换成记录版的 hook 来观察
    std::panic::set_hook(Box::new(record));

    // 各种会把正文带进 panic 信息的写法
    let _ = std::panic::catch_unwind(|| panic!("{SECRET}"));
    let _ = std::panic::catch_unwind(|| panic!("literal {} formatted", SECRET));
    let _ = std::panic::catch_unwind(|| {
        fn fails() -> Result<(), String> {
            Err(SECRET.to_string())
        }
        fails().unwrap();
    });
    let _ = std::panic::catch_unwind(|| {
        fn nothing() -> Option<u8> {
            None
        }
        nothing().expect(SECRET);
    });
    let _ = std::panic::catch_unwind(|| {
        assert_eq!(SECRET, "something else entirely");
    });
    let _ = std::panic::catch_unwind(|| {
        std::panic::panic_any(SECRET.to_string());
    });

    let reports = REPORTS.lock().unwrap().clone();
    assert_eq!(reports.len(), 6);
    for report in &reports {
        assert!(!report.contains(SECRET), "{report}");
        assert!(!report.contains("something else"), "{report}");
        assert!(
            report.contains("child_safety_hardening.rs:") && report.contains("withheld"),
            "{report}"
        );
    }

    // 真实的安装函数：能装上（之后 hook 就是只打位置的那个；这里不再触发 panic）
    install_panic_redaction();
    let _ = std::panic::take_hook();

    #[cfg(unix)]
    {
        use idoris_router::cloud::safety::hardening::{core_dump_limits, disable_core_dumps};
        disable_core_dumps().unwrap();
        assert_eq!(core_dump_limits().unwrap(), (0, 0));
        // 幂等
        disable_core_dumps().unwrap();
        assert_eq!(core_dump_limits().unwrap(), (0, 0));
    }
}

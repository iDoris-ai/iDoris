#![allow(clippy::unwrap_used, clippy::expect_used)]

use rusqlite::Connection;

use super::*;
use crate::test_support::TempDir;

fn store() -> CloudTierStore {
    CloudTierStore::open_in_memory().unwrap()
}

const LOCAL: &str = "7f2c1a52-0d0e-4a39-9c1f-aaaaaaaaaaaa";
const CLOUD: &str = "0b9f5c1e-3d71-4f2a-8f55-bbbbbbbbbbbb";

fn served<'a>(cloud: &'a str) -> CallFinish<'a> {
    CallFinish {
        outcome: CallOutcome::Served,
        finished_at_ms: 2_000,
        cloud_record_id: Some(cloud),
        http_status: Some(200),
        cloud_reason: None,
        tokens_in: Some(12),
        tokens_out: Some(30),
    }
}

// ---------------------------------------------------------------------------
// 线上字符串
// ---------------------------------------------------------------------------

#[test]
fn every_outcome_string_is_pinned_and_round_trips() {
    // 数据库里的字符串是契约：只能加，不能改。
    let pinned = [
        (CallOutcome::Served, "served"),
        (CallOutcome::Unreachable, "unreachable"),
        (CallOutcome::Timeout, "timeout"),
        (CallOutcome::Cancelled, "cancelled"),
        (CallOutcome::Unauthorized, "unauthorized"),
        (CallOutcome::Forbidden, "forbidden"),
        (CallOutcome::BudgetExhausted, "budget_exhausted"),
        (CallOutcome::RateLimited, "rate_limited"),
        (CallOutcome::Rejected, "rejected"),
        (CallOutcome::CloudError, "cloud_error"),
        (CallOutcome::InvalidResponse, "invalid_response"),
        (CallOutcome::Interrupted, "interrupted"),
    ];
    assert_eq!(pinned.len(), CallOutcome::ALL.len());
    for ((outcome, text), listed) in pinned.iter().zip(CallOutcome::ALL) {
        assert_eq!(*outcome, listed);
        assert_eq!(outcome.as_str(), *text);
        assert_eq!(CallOutcome::parse(text), Some(*outcome));
    }
    assert_eq!(CallOutcome::parse("pending"), None);
    assert_eq!(CallOutcome::parse("SERVED"), None);
    assert_eq!(CallState::Pending.as_str(), "pending");
}

// ---------------------------------------------------------------------------
// 先写后出网，终态写一次
// ---------------------------------------------------------------------------

#[test]
fn a_call_is_written_as_pending_before_anything_leaves_and_finishes_exactly_once() {
    let store = store();
    let ticket = store.begin_call(LOCAL, 1, 1_000).unwrap();
    let pending = store.call_by_local_id(LOCAL, 1).unwrap().unwrap();
    assert_eq!(pending.state, CallState::Pending);
    assert_eq!(pending.seq, ticket.seq);
    assert_eq!(pending.finished_at_ms, None);
    assert_eq!(pending.cloud_record_id, None);
    assert_eq!(pending.tokens_in, None);

    store.finish_call(&ticket, &served(CLOUD)).unwrap();
    let done = store.call_by_local_id(LOCAL, 1).unwrap().unwrap();
    assert_eq!(done.state, CallState::Final(CallOutcome::Served));
    assert_eq!(done.cloud_record_id.as_deref(), Some(CLOUD));
    assert_eq!(done.http_status, Some(200));
    assert_eq!((done.tokens_in, done.tokens_out), (Some(12), Some(30)));
    assert_eq!(done.finished_at_ms, Some(2_000));
    // 云端 record id 能反查到本机请求
    assert_eq!(store.call_by_cloud_id(CLOUD).unwrap().unwrap(), done);
    assert_eq!(store.call_by_cloud_id("nope").unwrap(), None);
    assert_eq!(store.call_count().unwrap(), 1);
}

#[test]
fn the_same_attempt_cannot_begin_twice() {
    let store = store();
    store.begin_call(LOCAL, 1, 1).unwrap();
    match store.begin_call(LOCAL, 1, 2) {
        Err(CloudTierStoreError::Duplicate {
            local_request_id,
            attempt_no,
        }) => {
            assert_eq!((local_request_id.as_str(), attempt_no), (LOCAL, 1));
        }
        other => panic!("{other:?}"),
    }
    // 另一次尝试序号是另一行（将来重试时幂等键仍然成立）
    store.begin_call(LOCAL, 2, 3).unwrap();
    assert_eq!(store.call_count().unwrap(), 2);
}

#[test]
fn replaying_the_same_final_outcome_is_a_no_op_and_a_different_one_is_refused() {
    let store = store();
    let ticket = store.begin_call(LOCAL, 1, 1_000).unwrap();
    store.finish_call(&ticket, &served(CLOUD)).unwrap();
    // 重放：完全一样 -> 空操作
    store.finish_call(&ticket, &served(CLOUD)).unwrap();
    // 不一样（换了 token 数、换了终态）-> 拒绝，库里不变
    let mut other = served(CLOUD);
    other.tokens_out = Some(31);
    assert!(matches!(
        store.finish_call(&ticket, &other),
        Err(CloudTierStoreError::AlreadyFinal(seq)) if seq == ticket.seq
    ));
    assert!(matches!(
        store.finish_call(
            &ticket,
            &CallFinish::without_answer(CallOutcome::Timeout, 3_000)
        ),
        Err(CloudTierStoreError::AlreadyFinal(_))
    ));
    let row = store.call_by_local_id(LOCAL, 1).unwrap().unwrap();
    assert_eq!(row.tokens_out, Some(30));
    assert_eq!(row.state, CallState::Final(CallOutcome::Served));
}

#[test]
fn a_ticket_for_another_row_is_not_accepted() {
    let store = store();
    let ticket = store.begin_call(LOCAL, 1, 1).unwrap();
    let forged = CallTicket {
        seq: ticket.seq,
        local_request_id: "someone-else".to_string(),
        attempt_no: 1,
    };
    assert!(matches!(
        store.finish_call(
            &forged,
            &CallFinish::without_answer(CallOutcome::Timeout, 5)
        ),
        Err(CloudTierStoreError::NotFound(_))
    ));
    let missing = CallTicket {
        seq: 999,
        local_request_id: LOCAL.to_string(),
        attempt_no: 1,
    };
    assert!(matches!(
        store.finish_call(
            &missing,
            &CallFinish::without_answer(CallOutcome::Timeout, 5)
        ),
        Err(CloudTierStoreError::NotFound(999))
    ));
}

// ---------------------------------------------------------------------------
// 离线：没有云端答复的结局不可能带金额
// ---------------------------------------------------------------------------

#[test]
fn an_outcome_without_a_cloud_answer_can_never_carry_amounts_or_a_cloud_id() {
    let store = store();
    for (index, outcome) in [
        CallOutcome::Unreachable,
        CallOutcome::Timeout,
        CallOutcome::Cancelled,
        CallOutcome::Interrupted,
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("local-{index}");
        let ticket = store.begin_call(&id, 1, 10).unwrap();
        // 带 token 用量：被 CHECK 拒绝（离线时不可能在本地「记出」一笔云端用量）
        let mut with_tokens = CallFinish::without_answer(outcome, 20);
        with_tokens.tokens_in = Some(1);
        with_tokens.tokens_out = Some(1);
        assert!(
            matches!(
                store.finish_call(&ticket, &with_tokens),
                Err(CloudTierStoreError::InvalidField(_))
            ),
            "{outcome:?}"
        );
        // 行还是 pending（被拒的写入没有留下任何东西）
        assert_eq!(
            store.call_by_local_id(&id, 1).unwrap().unwrap().state,
            CallState::Pending
        );
        // 正常的写法：没有 id、没有金额
        store
            .finish_call(&ticket, &CallFinish::without_answer(outcome, 20))
            .unwrap();
        let row = store.call_by_local_id(&id, 1).unwrap().unwrap();
        assert_eq!(row.state, CallState::Final(outcome));
        assert_eq!((row.tokens_in, row.tokens_out), (None, None));
        assert_eq!(row.cloud_record_id, None);
    }
}

#[test]
fn served_needs_a_cloud_id_and_a_success_status() {
    let store = store();
    let ticket = store.begin_call(LOCAL, 1, 1).unwrap();
    let mut no_id = served(CLOUD);
    no_id.cloud_record_id = None;
    assert!(store.finish_call(&ticket, &no_id).is_err());
    let mut no_status = served(CLOUD);
    no_status.http_status = None;
    assert!(
        store.finish_call(&ticket, &no_status).is_err(),
        "NULL must not slip through the CHECK"
    );
    for status in [199, 300, 402, 503] {
        let mut bad = served(CLOUD);
        bad.http_status = Some(status);
        assert!(store.finish_call(&ticket, &bad).is_err(), "{status}");
    }
    assert_eq!(
        store.call_by_local_id(LOCAL, 1).unwrap().unwrap().state,
        CallState::Pending
    );
}

#[test]
fn a_failed_answer_records_the_status_and_the_stable_reason_but_no_amounts() {
    let store = store();
    let ticket = store.begin_call(LOCAL, 1, 1).unwrap();
    store
        .finish_call(
            &ticket,
            &CallFinish {
                outcome: CallOutcome::BudgetExhausted,
                finished_at_ms: 9,
                cloud_record_id: Some(CLOUD),
                http_status: Some(402),
                cloud_reason: Some("budget_exhausted"),
                tokens_in: None,
                tokens_out: None,
            },
        )
        .unwrap();
    let row = store.call_by_local_id(LOCAL, 1).unwrap().unwrap();
    assert_eq!(row.state, CallState::Final(CallOutcome::BudgetExhausted));
    assert_eq!(row.http_status, Some(402));
    assert_eq!(row.cloud_reason.as_deref(), Some("budget_exhausted"));
    assert_eq!((row.tokens_in, row.tokens_out), (None, None));
}

// ---------------------------------------------------------------------------
// 没有正文：字段只接受标识符形状
// ---------------------------------------------------------------------------

#[test]
fn free_text_cannot_be_written_into_any_column() {
    let store = store();
    let body = "please summarise my medical history and email it to bob@example.com";
    for bad in ["", "a b", "line\nbreak", body, &"x".repeat(129), "é"] {
        assert!(
            matches!(
                store.begin_call(bad, 1, 1),
                Err(CloudTierStoreError::InvalidField("local_request_id"))
            ),
            "{bad:?}"
        );
    }
    assert!(matches!(
        store.begin_call(LOCAL, 0, 1),
        Err(CloudTierStoreError::InvalidField("attempt_no"))
    ));
    let ticket = store.begin_call(LOCAL, 1, 1).unwrap();
    let mut bad_id = served(CLOUD);
    bad_id.cloud_record_id = Some(body);
    assert!(matches!(
        store.finish_call(&ticket, &bad_id),
        Err(CloudTierStoreError::InvalidField("cloud_record_id"))
    ));
    for bad_reason in [
        "",
        "Budget Exhausted",
        body,
        "UPPER",
        "with-dash",
        &"r".repeat(129),
    ] {
        let mut bad = CallFinish::without_answer(CallOutcome::Rejected, 5);
        bad.cloud_reason = Some(bad_reason);
        assert!(
            matches!(
                store.finish_call(&ticket, &bad),
                Err(CloudTierStoreError::InvalidField("cloud_reason"))
            ),
            "{bad_reason:?}"
        );
    }
    // 云端把几个码逗号连起来是允许的
    let mut joined = CallFinish::without_answer(CallOutcome::Rejected, 5);
    joined.cloud_reason = Some("budget_degraded,upstream_failover");
    store.finish_call(&ticket, &joined).unwrap();
}

#[test]
fn a_cloud_record_id_can_only_belong_to_one_local_request() {
    let store = store();
    let first = store.begin_call("local-a", 1, 1).unwrap();
    store.finish_call(&first, &served(CLOUD)).unwrap();
    let second = store.begin_call("local-b", 1, 2).unwrap();
    assert!(matches!(
        store.finish_call(&second, &served(CLOUD)),
        Err(CloudTierStoreError::CloudRecordConflict)
    ));
    // 冲突的写入什么都没留下；这次调用仍然可以记成没有云端 id 的结局
    assert_eq!(
        store.call_by_local_id("local-b", 1).unwrap().unwrap().state,
        CallState::Pending
    );
    store
        .finish_call(
            &second,
            &CallFinish::without_answer(CallOutcome::InvalidResponse, 3),
        )
        .unwrap();
}

// ---------------------------------------------------------------------------
// 数据库自己的防线（绕过 Rust API 直接写 SQL 也不行）
// ---------------------------------------------------------------------------

#[test]
fn the_database_itself_keeps_the_log_append_only_and_the_identity_immutable() {
    let dir = TempDir::new("cloud-tier-triggers");
    let path = dir.db("tier.sqlite3");
    let store = CloudTierStore::open(&path).unwrap();
    let pending = store.begin_call("local-pending", 1, 10).unwrap();
    let done = store.begin_call("local-done", 1, 11).unwrap();
    store.finish_call(&done, &served(CLOUD)).unwrap();
    let raw = Connection::open(&path).unwrap();

    // 删除：任何行都不行
    for seq in [pending.seq, done.seq] {
        let err = raw
            .execute("DELETE FROM cloud_tier_calls WHERE seq = ?1", [seq])
            .unwrap_err();
        assert!(err.to_string().contains("append-only"), "{err}");
    }
    // 终态不能再改（比如把 token 数改大）
    let err = raw
        .execute(
            "UPDATE cloud_tier_calls SET tokens_out = 999999 WHERE seq = ?1",
            [done.seq],
        )
        .unwrap_err();
    assert!(err.to_string().contains("final outcome"), "{err}");
    // pending 不能被改回 pending，也不能改身份
    let err = raw
        .execute(
            "UPDATE cloud_tier_calls SET outcome = 'pending' WHERE seq = ?1",
            [pending.seq],
        )
        .unwrap_err();
    assert!(err.to_string().contains("final outcome"), "{err}");
    for set in [
        "local_request_id = 'forged'",
        "attempt_no = 2",
        "started_at_ms = 0",
        "seq = 99",
    ] {
        let err = raw
            .execute(
                &format!(
                    "UPDATE cloud_tier_calls SET {set}, outcome = 'timeout', finished_at_ms = 50 WHERE seq = ?1"
                ),
                [pending.seq],
            )
            .unwrap_err();
        assert!(err.to_string().contains("immutable"), "{set}: {err}");
    }
    // 绕过 Rust API 也不能写出「没答复却有金额」或「served 没有 id」
    let err = raw
        .execute(
            "UPDATE cloud_tier_calls SET outcome = 'unreachable', finished_at_ms = 50, tokens_in = 5 WHERE seq = ?1",
            [pending.seq],
        )
        .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("check"), "{err}");
    let err = raw
        .execute(
            "UPDATE cloud_tier_calls SET outcome = 'served', finished_at_ms = 50 WHERE seq = ?1",
            [pending.seq],
        )
        .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("check"), "{err}");
    // 一直没有被改动
    assert_eq!(
        store
            .call_by_local_id("local-pending", 1)
            .unwrap()
            .unwrap()
            .state,
        CallState::Pending
    );
}

#[test]
fn seq_is_never_reused_even_after_the_largest_row_would_have_gone() {
    // AUTOINCREMENT：seq 单调递增、永不复用（对账游标靠它）。删除是被禁止的，
    // 这里用另一种方式证明：连续写入的 seq 严格递增，并且和 rowid 的最大值一致。
    let store = store();
    let mut previous = 0;
    for index in 0..20 {
        let ticket = store.begin_call(&format!("local-{index}"), 1, 1).unwrap();
        assert!(ticket.seq > previous);
        previous = ticket.seq;
        store
            .finish_call(
                &ticket,
                &CallFinish::without_answer(CallOutcome::Timeout, 2),
            )
            .unwrap();
    }
    let rows = store.calls_after(0, 100).unwrap();
    assert_eq!(rows.len(), 20);
    assert!(rows.windows(2).all(|pair| pair[0].seq < pair[1].seq));
}

// ---------------------------------------------------------------------------
// 崩溃恢复
// ---------------------------------------------------------------------------

#[test]
fn leftover_pending_rows_become_interrupted_at_startup_and_nothing_else_changes() {
    let dir = TempDir::new("cloud-tier-recover");
    let path = dir.db("tier.sqlite3");
    {
        let store = CloudTierStore::open(&path).unwrap();
        let done = store.begin_call("local-done", 1, 1).unwrap();
        store.finish_call(&done, &served(CLOUD)).unwrap();
        store.begin_call("local-crashed", 1, 50).unwrap();
        // 进程在这里「崩溃」：store 被丢弃，pending 行留在库里
    }
    let store = CloudTierStore::open(&path).unwrap();
    assert_eq!(store.recover_interrupted(100).unwrap(), 1);
    let crashed = store.call_by_local_id("local-crashed", 1).unwrap().unwrap();
    assert_eq!(crashed.state, CallState::Final(CallOutcome::Interrupted));
    assert_eq!(crashed.cloud_record_id, None);
    assert_eq!((crashed.tokens_in, crashed.tokens_out), (None, None));
    assert_eq!(crashed.finished_at_ms, Some(100));
    let done = store.call_by_local_id("local-done", 1).unwrap().unwrap();
    assert_eq!(done.state, CallState::Final(CallOutcome::Served));
    // 再来一次什么都不会发生
    assert_eq!(store.recover_interrupted(200).unwrap(), 0);
}

#[test]
fn a_clock_that_ran_backwards_does_not_break_the_finished_after_started_rule() {
    let store = store();
    let ticket = store.begin_call(LOCAL, 1, 5_000).unwrap();
    // 完成时间早于开始时间（系统时钟被调回）：存成「不早于开始」，而不是让整次写入失败
    store
        .finish_call(
            &ticket,
            &CallFinish::without_answer(CallOutcome::Timeout, 4_000),
        )
        .unwrap();
    let row = store.call_by_local_id(LOCAL, 1).unwrap().unwrap();
    assert_eq!(row.finished_at_ms, Some(5_000));
    store.begin_call("crash", 1, 9_000).unwrap();
    store.recover_interrupted(1_000).unwrap();
    assert_eq!(
        store
            .call_by_local_id("crash", 1)
            .unwrap()
            .unwrap()
            .finished_at_ms,
        Some(9_000)
    );
}

// ---------------------------------------------------------------------------
// 游标
// ---------------------------------------------------------------------------

fn finished(store: &CloudTierStore, count: usize) -> Vec<CallTicket> {
    (0..count)
        .map(|index| {
            let ticket = store.begin_call(&format!("local-{index}"), 1, 1).unwrap();
            store
                .finish_call(
                    &ticket,
                    &CallFinish::without_answer(CallOutcome::Timeout, 2),
                )
                .unwrap();
            ticket
        })
        .collect()
}

#[test]
fn the_cursor_starts_at_zero_and_advancing_is_idempotent_and_monotonic() {
    let store = store();
    let tickets = finished(&store, 5);
    assert_eq!(store.cursor("reconciler").unwrap(), 0);
    assert_eq!(
        store
            .advance_cursor("reconciler", tickets[2].seq, 10)
            .unwrap(),
        tickets[2].seq
    );
    assert_eq!(store.cursor("reconciler").unwrap(), tickets[2].seq);
    // 重复推进到同一个位置：空操作
    assert_eq!(
        store
            .advance_cursor("reconciler", tickets[2].seq, 11)
            .unwrap(),
        tickets[2].seq
    );
    // 推进到更早的位置：空操作，不会倒退
    assert_eq!(
        store
            .advance_cursor("reconciler", tickets[0].seq, 12)
            .unwrap(),
        tickets[2].seq
    );
    assert_eq!(store.cursor("reconciler").unwrap(), tickets[2].seq);
    // 往前走
    assert_eq!(
        store
            .advance_cursor("reconciler", tickets[4].seq, 13)
            .unwrap(),
        tickets[4].seq
    );
    // 另一个命名游标互不影响
    assert_eq!(store.cursor("other").unwrap(), 0);
    // 对账方按游标往后读：只读到游标之后的行
    let tail = store
        .calls_after(store.cursor("reconciler").unwrap(), 10)
        .unwrap();
    assert!(tail.is_empty());
    let after_two = store.calls_after(tickets[2].seq, 10).unwrap();
    assert_eq!(after_two.len(), 2);
    assert_eq!(after_two[0].seq, tickets[3].seq);
    // limit 生效
    assert_eq!(store.calls_after(0, 2).unwrap().len(), 2);
}

#[test]
fn the_cursor_cannot_pass_the_last_call_or_a_call_that_has_no_outcome_yet() {
    let store = store();
    let tickets = finished(&store, 2);
    let in_flight = store.begin_call("still-running", 1, 5).unwrap();
    let later = store.begin_call("later", 1, 6).unwrap();
    store
        .finish_call(&later, &CallFinish::without_answer(CallOutcome::Timeout, 7))
        .unwrap();
    // 越过最后一行
    assert!(matches!(
        store.advance_cursor("r", later.seq + 1, 9),
        Err(CloudTierStoreError::CursorRefused { .. })
    ));
    // 越过还在路上的调用：对账永远不会跳过一笔还没有结局的钱
    assert!(matches!(
        store.advance_cursor("r", later.seq, 9),
        Err(CloudTierStoreError::CursorRefused { why, .. }) if why.contains("no outcome")
    ));
    // 在它之前停下是可以的
    assert_eq!(
        store.advance_cursor("r", tickets[1].seq, 9).unwrap(),
        tickets[1].seq
    );
    // 它有了结局之后就能过去
    store
        .finish_call(
            &in_flight,
            &CallFinish::without_answer(CallOutcome::Cancelled, 8),
        )
        .unwrap();
    assert_eq!(store.advance_cursor("r", later.seq, 10).unwrap(), later.seq);
    assert!(matches!(
        store.advance_cursor("r", -1, 10),
        Err(CloudTierStoreError::CursorRefused { .. })
    ));
    assert!(matches!(
        store.advance_cursor("bad name", 1, 10),
        Err(CloudTierStoreError::InvalidField("cursor name"))
    ));
}

#[test]
fn the_database_refuses_to_move_the_cursor_backwards_or_delete_it() {
    let dir = TempDir::new("cloud-tier-cursor");
    let path = dir.db("tier.sqlite3");
    let store = CloudTierStore::open(&path).unwrap();
    let tickets = finished(&store, 3);
    store.advance_cursor("r", tickets[2].seq, 1).unwrap();
    let raw = Connection::open(&path).unwrap();
    let err = raw
        .execute(
            "UPDATE cloud_tier_cursor SET last_seq = 0 WHERE name = 'r'",
            [],
        )
        .unwrap_err();
    assert!(err.to_string().contains("forward"), "{err}");
    let err = raw
        .execute("DELETE FROM cloud_tier_cursor WHERE name = 'r'", [])
        .unwrap_err();
    assert!(err.to_string().contains("cannot be deleted"), "{err}");
    assert_eq!(store.cursor("r").unwrap(), tickets[2].seq);
}

// ---------------------------------------------------------------------------
// 与别的 store 共用一个库文件
// ---------------------------------------------------------------------------

#[test]
fn it_coexists_with_the_other_stores_in_one_database_file_and_reopens_cleanly() {
    let dir = TempDir::new("cloud-tier-coexist");
    let path = dir.db("shared.sqlite3");
    let budget = crate::budget::BudgetLedger::open(&path).unwrap();
    let attempts = crate::attempts::AttemptStore::open(&path).unwrap();
    let tier = CloudTierStore::open(&path).unwrap();
    let ticket = tier.begin_call(LOCAL, 1, 1).unwrap();
    tier.finish_call(&ticket, &served(CLOUD)).unwrap();
    drop((budget, attempts, tier));
    // 重新打开：迁移是幂等的，数据还在
    let again = CloudTierStore::open(&path).unwrap();
    assert_eq!(again.call_count().unwrap(), 1);
    let again_twice = CloudTierStore::open(&path).unwrap();
    assert_eq!(again_twice.call_count().unwrap(), 1);
    // 别的 store 也还能正常打开
    crate::budget::BudgetLedger::open(&path).unwrap();
    crate::attempts::AttemptStore::open(&path).unwrap();
}

#[test]
fn the_store_has_no_column_that_could_hold_a_balance_or_a_body() {
    // 这张表不是账本：schema 里没有余额、额度、金额、正文一类的列。列名清单就是契约。
    let store = store();
    let conn = store.lock();
    let mut statement = conn
        .prepare("SELECT name FROM pragma_table_info('cloud_tier_calls')")
        .unwrap();
    let columns: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        columns,
        [
            "seq",
            "local_request_id",
            "attempt_no",
            "started_at_ms",
            "outcome",
            "finished_at_ms",
            "cloud_record_id",
            "http_status",
            "cloud_reason",
            "tokens_in",
            "tokens_out"
        ]
    );
}

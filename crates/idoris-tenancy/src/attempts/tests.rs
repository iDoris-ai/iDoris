#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use idoris_compute_contracts::admission::{AdmissionDecision, Admitted, Reservation};
use idoris_compute_contracts::attempt::{AttemptEvent, AttemptEventKind, SettleBasis};
use idoris_compute_contracts::ids::{AccountId, AttemptIndex, EndpointKey, PriceVersion};
use idoris_compute_contracts::pricing::{Cost, Price, RateCard};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_compute_contracts::request::AdmittedCandidate;

use super::*;

fn rid(s: &str) -> RequestId {
    RequestId::new(s).unwrap()
}

fn thb(micros: i64) -> Price {
    Price::from_micros(Unit::Thb, micros).unwrap()
}

fn usd(micros: i64) -> Cost {
    Cost::from_micros(Unit::Usd, micros).unwrap()
}

fn payment() -> PaymentContext {
    serde_json::from_value(json!({
        "credential": "cred-1",
        "payer": "acct-1",
        "unit": "THB",
    }))
    .unwrap()
}

/// 1000 入 200 出：价 = 10_000 + 8_000 = 18_000 微泰铢；成本 = 280 + 84 = 364 微美元。
fn usage() -> Usage {
    Usage::new(1_000, 200, 0).unwrap()
}

fn candidate(model: &str) -> AdmittedCandidate {
    AdmittedCandidate {
        decision: AdmissionDecision::Allow {
            admitted: Admitted {
                endpoint: EndpointKey::new("example-provider", model, "openai-compat").unwrap(),
                payer: AccountId::new("acct-1").unwrap(),
                effective_max_tokens: 400,
                reservation: Reservation {
                    price: thb(20_000),
                    cost: usd(400),
                },
            },
        },
        price_version: PriceVersion::new("pv-1").unwrap(),
        cost_rates: RateCard::new(usd(280_000), usd(420_000), None).unwrap(),
        price_rates: RateCard::new(thb(10_000_000), thb(40_000_000), None).unwrap(),
    }
}

const NOW: TimestampMs = TimestampMs(1_793_836_800_000);

fn classes() -> BTreeSet<DataClass> {
    BTreeSet::from([DataClass::Adult])
}

fn new_request<'a>(
    id: &'a RequestId,
    payment: &'a PaymentContext,
    classes: &'a BTreeSet<DataClass>,
) -> NewRequest<'a> {
    NewRequest {
        id,
        payment,
        form: DeploymentForm::Cloud,
        billing: Billing::Paid,
        stream: false,
        data_classes: classes,
        created_at: NOW,
    }
}

/// 创建一个请求和第一次尝试，返回内存里的 `Request` 与库。
fn started(id: &str) -> (AttemptStore, Request) {
    let store = AttemptStore::open_in_memory().unwrap();
    let request = Request::admit(rid(id), payment(), candidate("model-a")).unwrap();
    let classes = classes();
    store
        .create_request(
            &new_request(request.id(), request.payment(), &classes),
            &request.attempts()[0],
        )
        .unwrap();
    (store, request)
}

fn idx(n: u8) -> AttemptIndex {
    AttemptIndex(n)
}

fn at(ms: i64) -> TimestampMs {
    TimestampMs(NOW.0 + ms)
}

/// 同一个事件同时应用到库和内存里的 `Request`，并断言两边的新状态一致（执行器就是这么用的）。
fn apply(
    store: &AttemptStore,
    request: &mut Request,
    index: u8,
    events: &[AttemptEvent],
    t: i64,
) -> AttemptState {
    let id = request.attempt(idx(index)).unwrap().id().clone();
    let from_db = store.apply_events(&id, events, at(t)).unwrap();
    let mut last = None;
    for event in events {
        last = Some(request.apply(idx(index), event.clone()).unwrap().clone());
    }
    assert_eq!(
        Some(from_db.clone()),
        last,
        "store and in-memory state agree"
    );
    from_db
}

#[test]
fn migrations_are_idempotent_and_reopening_keeps_the_data() {
    let dir = tempfile_dir();
    let path = dir.join("attempts.sqlite3");
    let request = Request::admit(rid("req-1"), payment(), candidate("model-a")).unwrap();
    {
        let store = AttemptStore::open(&path).unwrap();
        let classes = classes();
        store
            .create_request(
                &new_request(request.id(), request.payment(), &classes),
                &request.attempts()[0],
            )
            .unwrap();
    }
    let store = AttemptStore::open(&path).unwrap();
    let loaded = store.load_request(&rid("req-1")).unwrap().unwrap();
    assert_eq!(loaded, request);
    // 版本表每个迁移一行（0001、0002），重开不重复应用
    let conn = store.lock();
    let versions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM compute_attempt_schema_migrations",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(versions, 2);
}

fn tempfile_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "idoris-attempts-test-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_request_round_trips_through_the_database_at_every_step() {
    let (store, mut request) = started("req-rt");
    let check = |store: &AttemptStore, request: &Request, label: &str| {
        let loaded = store.load_request(request.id()).unwrap().unwrap();
        assert_eq!(&loaded, request, "{label}");
    };
    check(&store, &request, "admitted");
    apply(
        &store,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        1,
    );
    check(&store, &request, "maybe_executed");
    // 第一次尝试 5xx：没有事件能证明没执行，保持 maybe_executed；切换到第二个候选
    request.begin_next_attempt(candidate("model-b")).unwrap();
    store
        .append_attempt(request.id(), &request.attempts()[1], at(2))
        .unwrap();
    check(&store, &request, "second attempt admitted");
    apply(
        &store,
        &mut request,
        1,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        3,
    );
    apply(
        &store,
        &mut request,
        1,
        &[AttemptEvent::UsageReported { usage: usage() }],
        4,
    );
    let supplier = SupplierRequestId::new("chatcmpl-123").unwrap();
    store
        .record_supplier_request_id(request.attempts()[1].id(), &supplier)
        .unwrap();
    request
        .record_supplier_request_id(idx(1), supplier.clone())
        .unwrap();
    check(&store, &request, "usage known with supplier id");
    store
        .record_publish(request.attempts()[1].id(), at(5))
        .unwrap();
    request.publish_first_byte(idx(1)).unwrap();
    check(&store, &request, "published");
}

#[test]
fn an_illegal_transition_is_refused_and_leaves_no_trace() {
    let (store, request) = started("req-illegal");
    let attempt = request.attempts()[0].id().clone();
    // admitted 状态不能直接 settle
    let err = store
        .apply_events(&attempt, &[AttemptEvent::Settle], at(1))
        .unwrap_err();
    assert!(matches!(err, AttemptStoreError::Transition(_)), "{err}");
    assert!(store.events(&attempt).unwrap().is_empty());
    // 批量里后面的事件非法，整批回滚：前面合法的 Dispatch 也不留
    let err = store
        .apply_events(
            &attempt,
            &[AttemptEvent::Dispatch, AttemptEvent::Settle],
            at(2),
        )
        .unwrap_err();
    assert!(matches!(err, AttemptStoreError::Transition(_)));
    assert!(store.events(&attempt).unwrap().is_empty());
    assert_eq!(
        store.attempt_rows(request.id()).unwrap()[0].state,
        AttemptState::Admitted
    );
    // 4xx 以外的状态码不能证明没执行
    store
        .apply_events(
            &attempt,
            &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
            at(3),
        )
        .unwrap();
    let err = store
        .apply_events(
            &attempt,
            &[AttemptEvent::UpstreamRefused { http_status: 503 }],
            at(4),
        )
        .unwrap_err();
    assert!(
        matches!(
            err,
            AttemptStoreError::Transition(TransitionError::InvalidEvidence { .. })
        ),
        "{err}"
    );
    assert_eq!(
        store.attempt_rows(request.id()).unwrap()[0].state,
        AttemptState::MaybeExecuted
    );
    // 日志里每个合法转移一行，顺序与时间都在
    let events = store.events(&attempt).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].from, AttemptStateKind::Admitted);
    assert_eq!(events[0].to, AttemptStateKind::Sending);
    assert_eq!(events[1].to, AttemptStateKind::MaybeExecuted);
}

// ---------------------------------------------------------------------------
// 数据库自己的防线
// ---------------------------------------------------------------------------

fn state_of(kind: AttemptStateKind) -> AttemptState {
    match kind {
        AttemptStateKind::Admitted => AttemptState::Admitted,
        AttemptStateKind::Sending => AttemptState::Sending,
        AttemptStateKind::NotSent => AttemptState::NotSent,
        AttemptStateKind::MaybeExecuted => AttemptState::MaybeExecuted,
        AttemptStateKind::ExecutedUsageKnown => AttemptState::ExecutedUsageKnown { usage: usage() },
        AttemptStateKind::ExecutedUsageUnknown => AttemptState::ExecutedUsageUnknown,
        AttemptStateKind::Settled => AttemptState::Settled {
            basis: SettleBasis::ReservedAmount,
        },
    }
}

fn event_of(kind: AttemptEventKind) -> AttemptEvent {
    match kind {
        AttemptEventKind::Dispatch => AttemptEvent::Dispatch,
        AttemptEventKind::NeverLeftHost => AttemptEvent::NeverLeftHost,
        AttemptEventKind::WireWriteStarted => AttemptEvent::WireWriteStarted,
        AttemptEventKind::UpstreamRefused => AttemptEvent::UpstreamRefused { http_status: 429 },
        AttemptEventKind::UsageReported => AttemptEvent::UsageReported { usage: usage() },
        AttemptEventKind::ExecutionObservedUsageUnknown => {
            AttemptEvent::ExecutionObservedUsageUnknown
        }
        AttemptEventKind::Settle => AttemptEvent::Settle,
    }
}

/// 触发器里写死的 10 条状态边，必须与 S0 转移表**逐格一致**：对 7×7 个（来源，目标）状态对，
/// 在库里直接 UPDATE，数据库放行当且仅当 S0 `transition` 里存在某个事件把来源推到这个目标。
#[test]
fn state_edges_match_the_contract_transition_table() {
    let store = AttemptStore::open_in_memory().unwrap();
    let mut counter = 0;
    let mut accepted = 0;
    for from in AttemptStateKind::ALL {
        for to in AttemptStateKind::ALL {
            if from == to {
                continue;
            }
            counter += 1;
            // 在 `from` 状态上直接插入一行（INSERT 不受状态边触发器约束），再 UPDATE 到 `to`。
            let request = Request::admit(
                rid(&format!("req-edge-{counter}")),
                payment(),
                candidate("model-a"),
            )
            .unwrap();
            let attempt_id = request.attempts()[0].id().to_string();
            let conn = store.lock();
            conn.execute(
                "INSERT INTO compute_requests
                   (request_id, payment_json, payer_account, credential_id, price_unit, form,
                    billing, stream, data_classes, created_at_ms)
                 VALUES (?1, ?2, 'acct-1', 'cred-1', 'THB', 'cloud', 'paid', 0, 'adult', 0)",
                params![
                    request.id().as_str(),
                    serde_json::to_string(request.payment()).unwrap()
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO compute_attempts
                   (attempt_id, request_id, attempt_index, provider, model, endpoint, price_version,
                    effective_max_tokens, reserved_price_micros, reserved_price_unit,
                    reserved_cost_micros, reserved_cost_unit, snapshot_json, state, state_json,
                    created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, 0, 'p', 'm', 'e', 'pv', 1, 0, 'THB', 0, 'USD', '{}', ?3, ?4, 0, 0)",
                params![
                    attempt_id,
                    request.id().as_str(),
                    from.as_str(),
                    serde_json::to_string(&state_of(from)).unwrap(),
                ],
            )
            .unwrap();
            let result = conn.execute(
                "UPDATE compute_attempts SET state = ?2, state_json = ?3 WHERE attempt_id = ?1",
                params![
                    attempt_id,
                    to.as_str(),
                    serde_json::to_string(&state_of(to)).unwrap(),
                ],
            );
            let contract_allows = AttemptEventKind::ALL.into_iter().any(|event| {
                transition(&state_of(from), &event_of(event))
                    .map(|next| next.kind() == to)
                    .unwrap_or(false)
            });
            assert_eq!(
                result.is_ok(),
                contract_allows,
                "{from:?} -> {to:?}: database {result:?}, contract allows {contract_allows}"
            );
            if result.is_ok() {
                accepted += 1;
            }
        }
    }
    // S0 转移表里共有 10 条不同的边
    assert_eq!(accepted, 10);
}

#[test]
fn history_and_postings_are_append_only_and_identity_is_immutable() {
    let (store, mut request) = started("req-append");
    apply(
        &store,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        1,
    );
    apply(
        &store,
        &mut request,
        0,
        &[AttemptEvent::UsageReported { usage: usage() }],
        2,
    );
    store
        .record_publish(request.attempts()[0].id(), at(3))
        .unwrap();
    request.publish_first_byte(idx(0)).unwrap();
    let plan = request.plan_settlement().unwrap();
    store
        .finalize_request(
            &plan,
            &[request.attempts()[0].id().clone()],
            RequestClose {
                outcome: RequestOutcome::Answered,
                reason: None,
                at: at(4),
            },
        )
        .unwrap();

    let conn = store.lock();
    let must_fail = |sql: &str| {
        let result = conn.execute(sql, []);
        assert!(result.is_err(), "should be refused: {sql}");
    };
    // 转移日志与分录：不能改、不能删
    must_fail("UPDATE compute_attempt_events SET at_ms = 0");
    must_fail("DELETE FROM compute_attempt_events");
    must_fail("UPDATE compute_postings SET amount_micros = 0");
    must_fail("DELETE FROM compute_postings");
    // 身份与定价不可改，行不可删
    must_fail("UPDATE compute_attempts SET price_version = 'pv-2'");
    must_fail("UPDATE compute_attempts SET reserved_price_micros = 0");
    must_fail("UPDATE compute_attempts SET provider = 'other'");
    must_fail("UPDATE compute_attempts SET snapshot_json = '{}'");
    must_fail("DELETE FROM compute_attempts");
    must_fail("DELETE FROM compute_requests");
    must_fail("UPDATE compute_requests SET payer_account = 'someone-else'");
    // 终态冻结：settled 不能回到别的状态
    must_fail(
        "UPDATE compute_attempts SET state = 'maybe_executed', state_json = '{\"state\":\"maybe_executed\"}'",
    );
    // 请求已关闭：摘要不能再改
    must_fail("UPDATE compute_requests SET charged_micros = 0");
    must_fail("UPDATE compute_requests SET published_attempt = NULL, published_at_ms = NULL");
    // 状态与 JSON 必须一致
    let (store2, _) = started("req-append-2");
    let conn2 = store2.lock();
    let err = conn2.execute(
        "UPDATE compute_attempts SET state = 'sending', state_json = '{\"state\":\"admitted\"}'",
        [],
    );
    assert!(err.is_err(), "state and state_json must agree");
}

/// PR-Daemon S1 F5：`INSERT OR REPLACE` 在主键 / 唯一约束冲突时先删旧行再插新行，而 `recursive_triggers` 默认关闭，
/// DELETE 触发器不触发——「只追加」「身份不可改」「请求关闭后冻结」都能被它绕过去，直接改写已经落定的分录。
/// 迁移 0002 给每张表的每个主键和唯一约束配一条 BEFORE INSERT 的「不许冲突」触发器（和账户、账本的迁移同一个做法）。
#[test]
fn insert_or_replace_cannot_rewrite_postings_events_requests_or_attempts() {
    let (store, mut request) = started("req-replace");
    apply(
        &store,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        1,
    );
    apply(
        &store,
        &mut request,
        0,
        &[AttemptEvent::UsageReported { usage: usage() }],
        2,
    );
    store
        .record_publish(request.attempts()[0].id(), at(3))
        .unwrap();
    request.publish_first_byte(idx(0)).unwrap();
    let plan = request.plan_settlement().unwrap();
    store
        .finalize_request(
            &plan,
            &[request.attempts()[0].id().clone()],
            RequestClose {
                outcome: RequestOutcome::Answered,
                reason: None,
                at: at(4),
            },
        )
        .unwrap();
    let (open_store, _open_request) = started("req-replace-open");

    let postings_total = |store: &AttemptStore| -> i64 {
        store
            .lock()
            .query_row(
                "SELECT COALESCE(SUM(amount_micros), 0) FROM compute_postings",
                [],
                |row| row.get(0),
            )
            .unwrap()
    };
    let before = postings_total(&store);
    assert!(before > 0, "the finalized request has postings");
    {
        let conn = store.lock();
        for sql in [
            // 分录：同一个 posting_id / 同一个幂等键，金额改成 0
            "INSERT OR REPLACE INTO compute_postings
                (posting_id, request_id, attempt_id, kind, role, basis, unit, amount_micros, idempotency_key, created_at_ms)
             SELECT posting_id, request_id, attempt_id, kind, role, basis, unit, 0, idempotency_key, created_at_ms
               FROM compute_postings",
            "REPLACE INTO compute_postings
                (request_id, attempt_id, kind, role, basis, unit, amount_micros, idempotency_key, created_at_ms)
             SELECT request_id, attempt_id, kind, role, basis, unit, 0, idempotency_key, created_at_ms
               FROM compute_postings",
            // 转移日志
            "INSERT OR REPLACE INTO compute_attempt_events (event_seq, attempt_id, from_state, event_json, to_state, at_ms)
             SELECT event_seq, attempt_id, from_state, '{}', to_state, 0 FROM compute_attempt_events",
            // 已经关闭的请求：改写它的付款账户和结算摘要
            "INSERT OR REPLACE INTO compute_requests
                (request_id, payment_json, payer_account, credential_id, price_unit, form, billing, stream, data_classes,
                 created_at_ms, published_attempt, published_at_ms, outcome, reason_code, closed_at_ms, charged_micros,
                 charged_basis, reserved_micros, overage_micros)
             SELECT request_id, payment_json, 'someone-else', credential_id, price_unit, form, billing, stream, data_classes,
                    created_at_ms, published_attempt, published_at_ms, outcome, reason_code, closed_at_ms, 0,
                    charged_basis, reserved_micros, overage_micros
               FROM compute_requests",
        ] {
            let result = conn.execute(sql, []);
            assert!(result.is_err(), "must be refused: {sql}");
        }
    }
    assert_eq!(postings_total(&store), before, "no posting was rewritten");
    let payer: String = store
        .lock()
        .query_row("SELECT payer_account FROM compute_requests", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_ne!(payer, "someone-else");
    // 还开着的请求里的尝试：身份和定价（预留额）不能被改写
    let conn = open_store.lock();
    let result = conn.execute(
        "INSERT OR REPLACE INTO compute_attempts
            (attempt_id, request_id, attempt_index, provider, model, endpoint, price_version, effective_max_tokens,
             reserved_price_micros, reserved_price_unit, reserved_cost_micros, reserved_cost_unit, snapshot_json,
             state, state_json, pays_user, created_at_ms, updated_at_ms)
         SELECT attempt_id, request_id, attempt_index, provider, model, endpoint, 'pv-forged', effective_max_tokens,
                0, reserved_price_unit, reserved_cost_micros, reserved_cost_unit, snapshot_json,
                state, state_json, pays_user, created_at_ms, updated_at_ms
           FROM compute_attempts",
        [],
    );
    assert!(result.is_err(), "an attempt cannot be replaced");
    let version: String = conn
        .query_row("SELECT price_version FROM compute_attempts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_ne!(version, "pv-forged");
}

/// 「用户只付一次」在数据库里也是结构保证：一个请求至多一个向用户收费的尝试、至多一条正的 user_price 分录。
/// 进程在「已发布、还没结算」时崩溃：只靠数据库就能恢复并结算（重放幂等），用户只付一次。
#[test]
fn a_crashed_request_is_found_and_settled_from_the_database_alone() {
    let dir = tempfile_dir();
    let path = dir.join("crash.sqlite3");
    let id = rid("req-crash");
    {
        let store = AttemptStore::open(&path).unwrap();
        let mut request = Request::admit(id.clone(), payment(), candidate("model-a")).unwrap();
        let classes = classes();
        store
            .create_request(
                &new_request(request.id(), request.payment(), &classes),
                &request.attempts()[0],
            )
            .unwrap();
        apply(
            &store,
            &mut request,
            0,
            &[
                AttemptEvent::Dispatch,
                AttemptEvent::WireWriteStarted,
                AttemptEvent::UsageReported { usage: usage() },
            ],
            1,
        );
        store
            .record_publish(request.attempts()[0].id(), at(2))
            .unwrap();
        // 崩溃：进程在这里消失，store 被丢弃，没有任何结算
    }
    let store = AttemptStore::open(&path).unwrap();
    // 太新的请求不算（可能还在进行中）
    assert!(
        store
            .open_requests(TimestampMs(NOW.0), 10)
            .unwrap()
            .is_empty()
    );
    let open = store.open_requests(at(60_000), 10).unwrap();
    assert_eq!(open, vec![id.clone()]);

    let mut recovered = store.load_request(&id).unwrap().unwrap();
    let plan = recovered.plan_settlement().unwrap();
    let to_settle: Vec<AttemptId> = recovered
        .attempts()
        .iter()
        .filter(|a| {
            matches!(
                a.state().kind(),
                AttemptStateKind::ExecutedUsageKnown | AttemptStateKind::ExecutedUsageUnknown
            )
        })
        .map(|a| a.id().clone())
        .collect();
    let close = RequestClose {
        outcome: RequestOutcome::Answered,
        reason: None,
        at: at(61_000),
    };
    let first = store.finalize_request(&plan, &to_settle, close).unwrap();
    assert_eq!(first.settled, to_settle);
    assert_eq!(first.postings_added, 3);
    // 恢复过程本身再崩一次、重放：幂等
    let again = store.finalize_request(&plan, &to_settle, close).unwrap();
    assert!(again.replayed && again.postings_added == 0);
    assert!(store.open_requests(at(120_000), 10).unwrap().is_empty());
    let kinds: Vec<(String, i64)> = store
        .postings(&id)
        .unwrap()
        .into_iter()
        .map(|p| (p.kind, p.money.micros))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("user_price".to_string(), 18_000),
            ("platform_cost".to_string(), 364),
            ("release".to_string(), 2_000)
        ]
    );
    recovered
        .apply(AttemptIndex(0), AttemptEvent::Settle)
        .unwrap();
}

#[test]
fn the_user_can_only_be_charged_once_per_request_at_the_database_level() {
    let (store, mut request) = started("req-once");
    apply(
        &store,
        &mut request,
        0,
        &[
            AttemptEvent::Dispatch,
            AttemptEvent::WireWriteStarted,
            AttemptEvent::UsageReported { usage: usage() },
        ],
        1,
    );
    request.begin_next_attempt(candidate("model-b")).unwrap();
    store
        .append_attempt(request.id(), &request.attempts()[1], at(2))
        .unwrap();
    apply(
        &store,
        &mut request,
        1,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        3,
    );
    // 最新的一次（序号 1）发布，它向用户收费
    store
        .record_publish(request.attempts()[1].id(), at(4))
        .unwrap();
    let conn = store.lock();
    let a0 = request.attempts()[0].id().to_string();
    let a1 = request.attempts()[1].id().to_string();
    // 第一次尝试也被标成向用户收费：违反唯一索引
    let second_payer = conn.execute(
        "UPDATE compute_attempts SET pays_user = 1 WHERE attempt_id = ?1",
        [&a0],
    );
    assert!(
        second_payer.is_err(),
        "a second paying attempt must be refused"
    );
    // 一条正的 user_price 分录可以，第二条（即使是另一次尝试、另一个键）不行；负数冲正可以
    let insert = |attempt: &str, amount: i64, key: &str| {
        conn.execute(
            "INSERT INTO compute_postings
               (request_id, attempt_id, kind, role, basis, unit, amount_micros, idempotency_key, created_at_ms)
             VALUES (?1, ?2, 'user_price', 'answering', 'actual_usage', 'THB', ?3, ?4, 0)",
            params![request.id().as_str(), attempt, amount, key],
        )
    };
    insert(&a0, 100, "k1").unwrap();
    assert!(
        insert(&a1, 100, "k2").is_err(),
        "a second positive user_price must be refused"
    );
    assert!(insert(&a0, 7, "k3").is_err());
    insert(&a0, -100, "k4").unwrap();
}

#[test]
fn the_publish_boundary_is_enforced_by_the_database_too() {
    let (store, mut request) = started("req-boundary");
    apply(
        &store,
        &mut request,
        0,
        &[
            AttemptEvent::Dispatch,
            AttemptEvent::WireWriteStarted,
            AttemptEvent::ExecutionObservedUsageUnknown,
        ],
        1,
    );
    store
        .record_publish(request.attempts()[0].id(), at(2))
        .unwrap();
    request.publish_first_byte(idx(0)).unwrap();

    // 契约本来就拒绝；这里**绕过契约**，直接构造第二次尝试交给库：库自己也必须拒绝
    let mut shadow = Request::admit(rid("req-boundary"), payment(), candidate("model-a")).unwrap();
    shadow.apply(idx(0), AttemptEvent::NeverLeftHost).unwrap();
    shadow.begin_next_attempt(candidate("model-b")).unwrap();
    let err = store
        .append_attempt(request.id(), &shadow.attempts()[1], at(3))
        .unwrap_err();
    assert!(
        err.to_string().contains("publish boundary"),
        "the database refuses a new attempt after the first byte: {err}"
    );
    assert_eq!(store.attempt_rows(request.id()).unwrap().len(), 1);
    // 发布记录写一次
    let err = store
        .record_publish(request.attempts()[0].id(), at(4))
        .unwrap_err();
    assert!(matches!(err, AttemptStoreError::Invariant(_)), "{err}");
}

#[test]
fn attempts_must_be_appended_in_order_and_only_after_the_previous_one_left_flight() {
    let (store, mut request) = started("req-order");
    // 上一次尝试还在 admitted：不能追加
    let mut shadow = Request::admit(rid("req-order"), payment(), candidate("model-a")).unwrap();
    shadow.apply(idx(0), AttemptEvent::NeverLeftHost).unwrap();
    shadow.begin_next_attempt(candidate("model-b")).unwrap();
    let err = store
        .append_attempt(request.id(), &shadow.attempts()[1], at(1))
        .unwrap_err();
    assert!(
        matches!(err, AttemptStoreError::Invariant(m) if m.contains("in flight")),
        "{err}"
    );
    // 让第一次尝试离开 admitted，再追加：可以
    apply(&store, &mut request, 0, &[AttemptEvent::NeverLeftHost], 2);
    request.begin_next_attempt(candidate("model-b")).unwrap();
    store
        .append_attempt(request.id(), &request.attempts()[1], at(3))
        .unwrap();
    // 同一个序号再追加一次：序号必须等于已有尝试数，被拒绝
    let err = store
        .append_attempt(request.id(), &request.attempts()[1], at(4))
        .unwrap_err();
    assert!(
        matches!(err, AttemptStoreError::Invariant(_)),
        "a repeated index is refused: {err}"
    );
    assert_eq!(store.attempt_rows(request.id()).unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// 结算
// ---------------------------------------------------------------------------

fn money(unit: Unit, micros: i64) -> Money {
    Money::new(unit, micros)
}

/// 第一次尝试 5xx（maybe_executed，不释放），第二次尝试完成并发布：用户只付第二次，
/// 第一次的成本按预留额挂账待核销，状态保持 maybe_executed。
#[test]
fn finalize_charges_the_user_once_and_holds_the_maybe_executed_cost() {
    let (store, mut request) = started("req-settle");
    apply(
        &store,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        1,
    );
    request.begin_next_attempt(candidate("model-b")).unwrap();
    store
        .append_attempt(request.id(), &request.attempts()[1], at(2))
        .unwrap();
    apply(
        &store,
        &mut request,
        1,
        &[
            AttemptEvent::Dispatch,
            AttemptEvent::WireWriteStarted,
            AttemptEvent::UsageReported { usage: usage() },
        ],
        3,
    );
    store
        .record_publish(request.attempts()[1].id(), at(4))
        .unwrap();
    request.publish_first_byte(idx(1)).unwrap();

    let plan = request.plan_settlement().unwrap();
    let report = store
        .finalize_request(
            &plan,
            &[request.attempts()[1].id().clone()],
            RequestClose {
                outcome: RequestOutcome::Answered,
                reason: Some(ReasonCode::UpstreamFailover.as_str()),
                at: at(5),
            },
        )
        .unwrap();
    assert_eq!(report.settled, vec![request.attempts()[1].id().clone()]);
    assert!(!report.replayed);

    let rows = store.attempt_rows(request.id()).unwrap();
    assert_eq!(
        rows[0].state,
        AttemptState::MaybeExecuted,
        "not released, not settled"
    );
    assert!(!rows[0].pays_user);
    assert_eq!(
        rows[1].state,
        AttemptState::Settled {
            basis: SettleBasis::ActualUsage(usage())
        }
    );
    assert!(rows[1].pays_user);
    assert_eq!(rows[1].usage(), Some(usage()));

    let postings = store.postings(request.id()).unwrap();
    let find = |attempt: u8, kind: &str| {
        postings
            .iter()
            .filter(|p| p.attempt.index().0 == attempt && p.kind == kind)
            .map(|p| p.money.clone())
            .collect::<Vec<_>>()
    };
    // 用户只付第二次：实际价 18_000 微泰铢；第一次没有 user_price
    assert_eq!(find(1, "user_price"), vec![money(Unit::Thb, 18_000)]);
    assert!(find(0, "user_price").is_empty());
    // 第二次的实际成本 364 微美元；第一次按预留成本 400 挂账（hold），不是 platform_cost
    assert_eq!(find(1, "platform_cost"), vec![money(Unit::Usd, 364)]);
    assert_eq!(find(0, "hold"), vec![money(Unit::Usd, 400)]);
    assert!(find(0, "platform_cost").is_empty());
    // 用户侧的价预留：第一次整笔释放；第二次释放 预留 - 实际 = 2_000
    assert_eq!(find(0, "release"), vec![money(Unit::Thb, 20_000)]);
    assert_eq!(find(1, "release"), vec![money(Unit::Thb, 2_000)]);

    let row = store.request_row(request.id()).unwrap().unwrap();
    assert_eq!(row.outcome, Some(RequestOutcome::Answered));
    assert_eq!(row.reason_code.as_deref(), Some("upstream_failover"));
    assert_eq!(row.published_attempt, Some(1));
    assert_eq!(row.charged_micros, Some(18_000));
    assert_eq!(row.charged_basis.as_deref(), Some("actual_usage"));
    assert_eq!(row.reserved_micros, Some(20_000));
    assert_eq!(row.overage_micros, Some(0));

    // 重放同一份计划：幂等，什么都不再追加，状态不变
    let again = store
        .finalize_request(
            &plan,
            &[request.attempts()[1].id().clone()],
            RequestClose {
                outcome: RequestOutcome::Answered,
                reason: Some(ReasonCode::UpstreamFailover.as_str()),
                at: at(6),
            },
        )
        .unwrap();
    assert!(again.replayed);
    assert_eq!(again.postings_added, 0);
    assert!(again.settled.is_empty());
    assert_eq!(store.postings(request.id()).unwrap(), postings);

    // 重放一份金额不同的计划：拒绝，不覆盖
    let mut tampered = plan.clone();
    if let Some(charge) = tampered.user_charge.as_mut() {
        charge.charged = thb(1);
    }
    let err = store
        .finalize_request(
            &tampered,
            &[],
            RequestClose {
                outcome: RequestOutcome::Answered,
                reason: Some(ReasonCode::UpstreamFailover.as_str()),
                at: at(7),
            },
        )
        .unwrap_err();
    assert!(
        matches!(err, AttemptStoreError::PostingConflict { .. }),
        "{err}"
    );

    // 重建出来的请求算出同样的计划
    let loaded = store.load_request(request.id()).unwrap().unwrap();
    assert_eq!(loaded.plan_settlement().unwrap(), plan);
}

/// 评审 r2 Medium#1 的场景：第一次尝试 A 停在 `maybe_executed`，B 作答并结算（A 留下 `hold`）。返回库、内存里的请求，
/// 以及已经收尾的计划。
fn finalized_after_a_failover(id: &str) -> (AttemptStore, Request, SettlementPlan) {
    let (store, mut request) = started(id);
    apply(
        &store,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        1,
    );
    request.begin_next_attempt(candidate("model-b")).unwrap();
    store
        .append_attempt(request.id(), &request.attempts()[1], at(2))
        .unwrap();
    apply(
        &store,
        &mut request,
        1,
        &[
            AttemptEvent::Dispatch,
            AttemptEvent::WireWriteStarted,
            AttemptEvent::UsageReported { usage: usage() },
        ],
        3,
    );
    store
        .record_publish(request.attempts()[1].id(), at(4))
        .unwrap();
    request.publish_first_byte(idx(1)).unwrap();
    let plan = request.plan_settlement().unwrap();
    store
        .finalize_request(
            &plan,
            &[request.attempts()[1].id().clone()],
            RequestClose {
                outcome: RequestOutcome::Answered,
                reason: Some(ReasonCode::UpstreamFailover.as_str()),
                at: at(5),
            },
        )
        .unwrap();
    (store, request, plan)
}

fn failover_close() -> RequestClose<'static> {
    RequestClose {
        outcome: RequestOutcome::Answered,
        reason: Some("upstream_failover"),
        at: at(6),
    }
}

/// A 的 usage：100 入 50 出，成本 = 100×0.28 + 50×0.42 = 49 微美元（夹具的成本价目）。
fn late_usage() -> Usage {
    Usage::new(100, 50, 0).unwrap()
}

/// 评审 r2 Medium#1：对账把 A 更新成已知 usage 之后，用重建的计划再调一次 `finalize_request`——修复之前它会跳过 A 的状态
/// 结算，却追加一条新的 `platform_cost`，原来的 `hold` 还保留：成本重复占账，A 也没有结清。重放必须和当初的收尾完全一致。
#[test]
fn a_replayed_settlement_cannot_add_postings_that_the_original_did_not_write() {
    let (store, request, _plan) = finalized_after_a_failover("req-replay-drift");
    let a = request.attempts()[0].id().clone();
    let before = store.postings(request.id()).unwrap();
    assert_eq!(
        before
            .iter()
            .filter(|p| p.attempt == a)
            .map(|p| (p.kind.as_str(), p.money.micros))
            .collect::<Vec<_>>(),
        vec![("hold", 400), ("release", 20_000)]
    );

    // 对账把 A 更新成已知 usage，重建计划
    store
        .apply_events(
            &a,
            &[AttemptEvent::UsageReported {
                usage: late_usage(),
            }],
            at(100),
        )
        .unwrap();
    let rebuilt = store.load_request(request.id()).unwrap().unwrap();
    let drifted = rebuilt.plan_settlement().unwrap();
    let error = store
        .finalize_request(&drifted, std::slice::from_ref(&a), failover_close())
        .unwrap_err();
    assert!(matches!(error, AttemptStoreError::Invariant(_)), "{error}");
    assert!(error.to_string().contains("would add postings"), "{error}");
    // 什么都没有追加、什么都没有改
    assert_eq!(store.postings(request.id()).unwrap(), before);
    assert_eq!(
        store.attempt_rows(request.id()).unwrap()[0].state,
        AttemptState::ExecutedUsageKnown {
            usage: late_usage()
        },
        "the replay must not pretend to settle the attempt"
    );

    // 对照：和当初完全一样的计划，仍然是幂等重放
    let (store, request, plan) = finalized_after_a_failover("req-replay-same");
    let again = store
        .finalize_request(
            &plan,
            &[request.attempts()[1].id().clone()],
            failover_close(),
        )
        .unwrap();
    assert!(again.replayed && again.postings_added == 0);
}

/// 重放一份**少**了当初分录的计划同样被拒绝（不是悄悄放过）。
#[test]
fn a_replayed_plan_that_drops_an_original_posting_is_refused_too() {
    let (store, request, plan) = finalized_after_a_failover("req-replay-fewer");
    let mut fewer = plan.clone();
    // 把用户的收费去掉：当初写过的 user_price 不在这份计划里
    fewer.user_charge = None;
    let error = store
        .finalize_request(&fewer, &[], failover_close())
        .unwrap_err();
    assert!(matches!(error, AttemptStoreError::Invariant(_)), "{error}");
    assert_eq!(store.postings(request.id()).unwrap().len(), 5);
}

/// 迟到 usage 的独立对账事务：一个事务里按 S0 的转移推进状态（`UsageReported` 之后 `Settle`）、冲销旧挂账、记实际成本。
#[test]
fn late_usage_reverses_the_hold_books_the_actual_cost_and_settles_in_one_transaction() {
    let (store, request, _plan) = finalized_after_a_failover("req-late");
    let a = request.attempts()[0].id().clone();
    assert_eq!(
        store.attempt_rows(request.id()).unwrap()[0].state,
        AttemptState::MaybeExecuted
    );

    let outcome = store
        .reconcile_late_usage(&a, late_usage(), at(1_000))
        .unwrap();
    assert_eq!(
        outcome,
        LateUsageOutcome::Reconciled {
            postings_added: 2,
            actual_cost: money(Unit::Usd, 49),
        }
    );
    // 状态：按实际用量结清，转移日志里是合法的两步
    let row = &store.attempt_rows(request.id()).unwrap()[0];
    assert_eq!(
        row.state,
        AttemptState::Settled {
            basis: SettleBasis::ActualUsage(late_usage())
        }
    );
    let events: Vec<(AttemptStateKind, AttemptStateKind)> = store
        .events(&a)
        .unwrap()
        .into_iter()
        .map(|e| (e.from, e.to))
        .collect();
    assert_eq!(
        events[events.len() - 2..],
        [
            (
                AttemptStateKind::MaybeExecuted,
                AttemptStateKind::ExecutedUsageKnown
            ),
            (
                AttemptStateKind::ExecutedUsageKnown,
                AttemptStateKind::Settled
            ),
        ]
    );
    // 分录：旧挂账 400 被一条 -400 冲销，净挂账为 0；实际成本 49
    let postings = store.postings(request.id()).unwrap();
    let of_a = |kind: &str| -> Vec<i64> {
        postings
            .iter()
            .filter(|p| p.attempt == a && p.kind == kind)
            .map(|p| p.money.micros)
            .collect()
    };
    assert_eq!(of_a("hold"), vec![400, -400]);
    assert_eq!(
        of_a("hold").iter().sum::<i64>(),
        0,
        "the pending hold is gone"
    );
    assert_eq!(of_a("platform_cost"), vec![49]);
    // 用户侧一分没动：A 不向用户收费，当初整笔释放的价预留还是那一条
    assert_eq!(of_a("release"), vec![20_000]);
    assert!(of_a("user_price").is_empty());
    let reconcile_keys: Vec<&str> = postings
        .iter()
        .filter(|p| p.idempotency_key.ends_with("|reconcile"))
        .map(|p| p.idempotency_key.as_str())
        .collect();
    assert_eq!(
        reconcile_keys,
        [
            "req-late#0|hold|reconcile",
            "req-late#0|platform_cost|reconcile"
        ]
    );

    // 幂等：同一个 usage 再来一次什么都不做
    let count = postings.len();
    assert_eq!(
        store
            .reconcile_late_usage(&a, late_usage(), at(2_000))
            .unwrap(),
        LateUsageOutcome::Replayed
    );
    assert_eq!(store.postings(request.id()).unwrap().len(), count);
    // 不同的 usage：冲突，不覆盖
    let other = Usage::new(1, 1, 0).unwrap();
    assert!(matches!(
        store
            .reconcile_late_usage(&a, other, at(3_000))
            .unwrap_err(),
        AttemptStoreError::PostingConflict { .. }
    ));
}

#[test]
fn late_usage_only_reconciles_a_maybe_executed_attempt_that_did_not_charge_the_user() {
    let (store, request, _plan) = finalized_after_a_failover("req-late-scope");
    let a = request.attempts()[0].id().clone();
    let b = request.attempts()[1].id().clone();
    // 向用户收费的那一次：迟到 usage 要不要退差价是准入账本的事，这里不碰用户的钱
    let error = store
        .reconcile_late_usage(&b, late_usage(), at(10))
        .unwrap_err();
    assert!(
        matches!(error, AttemptStoreError::Invariant(_)) && error.to_string().contains("ledger"),
        "{error}"
    );
    // 不存在的尝试
    let ghost: AttemptId = "req-nope#0".parse().unwrap();
    assert!(matches!(
        store
            .reconcile_late_usage(&ghost, late_usage(), at(10))
            .unwrap_err(),
        AttemptStoreError::AttemptNotFound(_)
    ));
    // 还没关闭的请求走 finalize_request，不走这里
    let (open_store, mut open_request) = started("req-late-open");
    apply(
        &open_store,
        &mut open_request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        1,
    );
    let open_a = open_request.attempts()[0].id().clone();
    assert!(matches!(
        open_store
            .reconcile_late_usage(&open_a, late_usage(), at(10))
            .unwrap_err(),
        AttemptStoreError::Invariant(_)
    ));
    // A 对账成功之后状态已经是 settled；对 usage 未知、已按预留额结清的尝试（没有事件能再改它）同样拒绝
    store
        .reconcile_late_usage(&a, late_usage(), at(1_000))
        .unwrap();
    let (store2, mut request2) = started("req-late-unknown");
    apply(
        &store2,
        &mut request2,
        0,
        &[
            AttemptEvent::Dispatch,
            AttemptEvent::WireWriteStarted,
            AttemptEvent::ExecutionObservedUsageUnknown,
        ],
        1,
    );
    let plan2 = request2.plan_settlement().unwrap();
    store2
        .finalize_request(
            &plan2,
            &[request2.attempts()[0].id().clone()],
            RequestClose {
                outcome: RequestOutcome::Unanswered,
                reason: None,
                at: at(2),
            },
        )
        .unwrap();
    assert!(matches!(
        store2
            .reconcile_late_usage(request2.attempts()[0].id(), late_usage(), at(3))
            .unwrap_err(),
        AttemptStoreError::Invariant(_)
    ));
}

#[test]
fn an_unpublished_request_charges_the_user_nothing_and_holds_what_may_have_run() {
    let (store, mut request) = started("req-unpublished");
    apply(
        &store,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        1,
    );
    // 没有任何尝试发布：用户付 0；maybe_executed 的成本按预留额挂账；用户侧预留整笔释放
    let plan = request.plan_settlement().unwrap();
    assert!(plan.user_charge.is_none());
    let report = store
        .finalize_request(
            &plan,
            &[],
            RequestClose {
                outcome: RequestOutcome::Unanswered,
                reason: None,
                at: at(2),
            },
        )
        .unwrap();
    assert!(report.settled.is_empty());
    let postings = store.postings(request.id()).unwrap();
    let kinds: Vec<(&str, i64)> = postings
        .iter()
        .map(|p| (p.kind.as_str(), p.money.micros))
        .collect();
    assert_eq!(kinds, vec![("hold", 400), ("release", 20_000)]);
    let row = store.request_row(request.id()).unwrap().unwrap();
    assert_eq!(row.outcome, Some(RequestOutcome::Unanswered));
    assert_eq!(row.charged_micros, None);
    assert_eq!(
        store.attempt_rows(request.id()).unwrap()[0].state,
        AttemptState::MaybeExecuted
    );
    // 关闭的请求既不能再发布，也不能再追加尝试
    assert!(
        store
            .record_publish(request.attempts()[0].id(), at(3))
            .is_err()
    );
    let mut shadow =
        Request::admit(rid("req-unpublished"), payment(), candidate("model-a")).unwrap();
    shadow.begin_next_attempt(candidate("model-b")).unwrap_err();
}

#[test]
fn usage_unknown_settles_at_the_reserved_amount_and_stays_pending() {
    let (store, mut request) = started("req-unknown");
    apply(
        &store,
        &mut request,
        0,
        &[
            AttemptEvent::Dispatch,
            AttemptEvent::WireWriteStarted,
            AttemptEvent::ExecutionObservedUsageUnknown,
        ],
        1,
    );
    store
        .record_publish(request.attempts()[0].id(), at(2))
        .unwrap();
    request.publish_first_byte(idx(0)).unwrap();
    let plan = request.plan_settlement().unwrap();
    store
        .finalize_request(
            &plan,
            &[request.attempts()[0].id().clone()],
            RequestClose {
                outcome: RequestOutcome::Answered,
                reason: None,
                at: at(3),
            },
        )
        .unwrap();
    let postings = store.postings(request.id()).unwrap();
    let by_kind = |kind: &str| -> Vec<(i64, String)> {
        postings
            .iter()
            .filter(|p| p.kind == kind)
            .map(|p| (p.money.micros, p.basis.clone()))
            .collect()
    };
    assert_eq!(
        by_kind("user_price"),
        vec![(20_000, "reserved_amount".to_string())]
    );
    assert_eq!(
        by_kind("hold"),
        vec![(400, "reserved_pending_reconciliation".to_string())]
    );
    assert!(
        by_kind("release").is_empty(),
        "nothing to release when charged the reservation"
    );
    let row = &store.attempt_rows(request.id()).unwrap()[0];
    assert_eq!(
        row.state,
        AttemptState::Settled {
            basis: SettleBasis::ReservedAmount
        }
    );
}

#[test]
fn finalize_refuses_a_plan_that_disagrees_with_the_recorded_publish() {
    let (store, mut request) = started("req-mismatch");
    apply(
        &store,
        &mut request,
        0,
        &[
            AttemptEvent::Dispatch,
            AttemptEvent::WireWriteStarted,
            AttemptEvent::UsageReported { usage: usage() },
        ],
        1,
    );
    // 内存里发布了，库里没有记发布：计划里有 user_charge，库里没有发布记录 -> 拒绝
    request.publish_first_byte(idx(0)).unwrap();
    let plan = request.plan_settlement().unwrap();
    let err = store
        .finalize_request(
            &plan,
            &[request.attempts()[0].id().clone()],
            RequestClose {
                outcome: RequestOutcome::Answered,
                reason: None,
                at: at(2),
            },
        )
        .unwrap_err();
    assert!(matches!(err, AttemptStoreError::Invariant(_)), "{err}");
    assert_eq!(
        store.attempt_rows(request.id()).unwrap()[0].state,
        AttemptState::ExecutedUsageKnown { usage: usage() },
        "nothing was settled"
    );
    assert!(store.postings(request.id()).unwrap().is_empty());
}

#[test]
fn a_rejected_request_leaves_an_audit_row_and_no_attempts() {
    let store = AttemptStore::open_in_memory().unwrap();
    let id = rid("req-rejected");
    let payment = payment();
    let classes = BTreeSet::from([DataClass::ChildEdu]);
    store
        .record_rejected_request(
            &new_request(&id, &payment, &classes),
            ReasonCode::NoCompliantEndpoint.as_str(),
        )
        .unwrap();
    let row = store.request_row(&id).unwrap().unwrap();
    assert_eq!(row.outcome, Some(RequestOutcome::Rejected));
    assert_eq!(row.reason_code.as_deref(), Some("no_compliant_endpoint"));
    assert_eq!(row.data_classes, "child_edu");
    assert_eq!(row.closed_at, Some(NOW));
    assert!(store.attempt_rows(&id).unwrap().is_empty());
    assert!(store.postings(&id).unwrap().is_empty());
    // 没有尝试的请求没法重建成 S0 的 Request（至少一次尝试），这是预期的
    assert!(store.load_request(&id).is_err());
}

// ---------------------------------------------------------------------------
// 供应商请求号与标注
// ---------------------------------------------------------------------------

#[test]
fn supplier_request_id_rules() {
    let (store, mut request) = started("req-supplier");
    let a0 = request.attempts()[0].id().clone();
    let id = SupplierRequestId::new("chatcmpl-1").unwrap();
    // 还没有任何东西回来：不能记
    assert!(matches!(
        store.record_supplier_request_id(&a0, &id).unwrap_err(),
        AttemptStoreError::Invariant(_)
    ));
    apply(
        &store,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        1,
    );
    assert_eq!(
        store.record_supplier_request_id(&a0, &id).unwrap(),
        SupplierIdOutcome::Recorded
    );
    // 同值幂等（流里每个 chunk 都带同一个 id）
    assert_eq!(
        store.record_supplier_request_id(&a0, &id).unwrap(),
        SupplierIdOutcome::Unchanged
    );
    // 不同值：保留第一个，标注冲突
    let other = SupplierRequestId::new("chatcmpl-2").unwrap();
    assert_eq!(
        store.record_supplier_request_id(&a0, &other).unwrap(),
        SupplierIdOutcome::Conflict
    );
    let row = &store.attempt_rows(request.id()).unwrap()[0];
    assert_eq!(row.supplier_request_id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(
        row.supplier_id_anomaly.as_deref(),
        Some("id_changed_midstream")
    );

    // 第二次尝试用了同一个供应商的同一个请求号：两条都保留，后写的标注 duplicate_of
    apply(
        &store,
        &mut request,
        0,
        &[AttemptEvent::ExecutionObservedUsageUnknown],
        2,
    );
    request.begin_next_attempt(candidate("model-b")).unwrap();
    store
        .append_attempt(request.id(), &request.attempts()[1], at(3))
        .unwrap();
    apply(
        &store,
        &mut request,
        1,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        4,
    );
    let a1 = request.attempts()[1].id().clone();
    assert_eq!(
        store.record_supplier_request_id(&a1, &id).unwrap(),
        SupplierIdOutcome::DuplicateOf(a0.clone())
    );
    let rows = store.attempt_rows(request.id()).unwrap();
    assert_eq!(rows[1].supplier_request_id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(
        rows[1].supplier_id_anomaly.as_deref(),
        Some(format!("duplicate_of:{a0}").as_str())
    );
    // 按供应商请求号反查：两条都能找到（对账时能看出这个号对不上单一尝试）
    let found = store
        .attempts_by_supplier_request_id("example-provider", "chatcmpl-1")
        .unwrap();
    assert_eq!(found.len(), 2);
    // 数据库层面：已经写过的请求号不能被改写
    let conn = store.lock();
    assert!(
        conn.execute(
            "UPDATE compute_attempts SET supplier_request_id = 'forged' WHERE attempt_id = ?1",
            [a0.to_string()]
        )
        .is_err()
    );
}

#[test]
fn annotations_are_write_once() {
    let (store, mut request) = started("req-notes");
    apply(
        &store,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        1,
    );
    let a0 = request.attempts()[0].id().clone();
    let claim = UsageClaim {
        verdict: "invalid:cached_exceeds_prompt".into(),
        raw: Some(json!({"prompt_tokens": 5, "completion_tokens": 7, "cached_tokens": 99})),
    };
    assert!(store.record_usage_claim(&a0, &claim).unwrap());
    assert!(
        !store
            .record_usage_claim(
                &a0,
                &UsageClaim {
                    verdict: "valid".into(),
                    raw: None
                }
            )
            .unwrap(),
        "the first claim is kept"
    );
    assert!(
        store
            .record_failure(
                &a0,
                &FailureNote {
                    kind: "http_status".into(),
                    http_status: Some(503)
                }
            )
            .unwrap()
    );
    assert!(
        !store
            .record_failure(
                &a0,
                &FailureNote {
                    kind: "connect_failed".into(),
                    http_status: None
                }
            )
            .unwrap()
    );
    let row = &store.attempt_rows(request.id()).unwrap()[0];
    assert_eq!(row.failure_kind.as_deref(), Some("http_status"));
    assert_eq!(row.failure_http_status, Some(503));
    let claim = row.usage_claim.as_ref().unwrap();
    assert_eq!(claim["verdict"], "invalid:cached_exceeds_prompt");
    assert_eq!(claim["raw"]["cached_tokens"], 99);
}

#[test]
fn reconciliation_can_find_unsettled_attempts() {
    let (store, mut request) = started("req-recon");
    apply(
        &store,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        1,
    );
    let rows = store
        .attempts_in_states(&[AttemptStateKind::MaybeExecuted], 10)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].attempt, *request.attempts()[0].id());
    assert_eq!(rows[0].price_version, "pv-1");
    assert_eq!(rows[0].reserved_cost, money(Unit::Usd, 400));
    assert!(
        store
            .attempts_in_states(&[AttemptStateKind::Settled], 10)
            .unwrap()
            .is_empty()
    );
    assert!(store.attempts_in_states(&[], 10).unwrap().is_empty());
}

#[test]
fn no_content_columns_exist() {
    // 设计稿 §7：正文只在内存里处理。表结构里没有任何能放正文的列名。
    let store = AttemptStore::open_in_memory().unwrap();
    let conn = store.lock();
    for table in [
        "compute_requests",
        "compute_attempts",
        "compute_attempt_events",
        "compute_postings",
    ] {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for column in columns {
            for forbidden in [
                "prompt",
                "message",
                "content",
                "completion",
                "body",
                "text",
                "tool",
            ] {
                assert!(
                    !column.contains(forbidden),
                    "{table}.{column} looks like a content column"
                );
            }
        }
    }
}

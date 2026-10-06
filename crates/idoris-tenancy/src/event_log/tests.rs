#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{collections::BTreeMap, time::Duration};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::json;

use super::*;

fn sample(tenant: &str, record: &str) -> NewEvent {
    NewEvent {
        event_id: Uuid::new_v4().to_string(),
        tenant_id: tenant.into(),
        record_id: record.into(),
        event_type: EventType::Decided,
        ts_utc_ms: 1_800_000_000_000,
        request_id: Some("req-1".into()),
        session_id: Some("session-1".into()),
        trace_id: Some("trace-1".into()),
        parent_id: None,
        origin_record_id: None,
        metadata: BTreeMap::from([
            ("status".into(), json!("ok")),
            (
                "reason_codes".into(),
                json!(["intent_match", "within_budget"]),
            ),
        ]),
    }
}

fn temp_db(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "idoris-event-log-{label}-{}.sqlite3",
        Uuid::new_v4()
    ))
}

#[test]
fn every_read_and_write_requires_nonblank_scope() {
    let store = EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap();
    let event = sample("tenant-a", "r1");
    for scope in [None, Some(""), Some("   ")] {
        assert!(matches!(
            store.append(scope, &event),
            Err(EventLogError::ScopeRequired)
        ));
        assert!(matches!(
            store.events_for_record(scope, "r1"),
            Err(EventLogError::ScopeRequired)
        ));
    }
}

#[test]
fn tenant_mismatch_is_rejected_and_queries_are_isolated() {
    let store = EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap();
    assert!(matches!(
        store.append(Some("tenant-b"), &sample("tenant-a", "shared")),
        Err(EventLogError::TenantMismatch)
    ));
    store
        .append(Some("tenant-a"), &sample("tenant-a", "shared"))
        .unwrap();
    store
        .append(Some("tenant-b"), &sample("tenant-b", "shared"))
        .unwrap();
    let a = store.events_for_record(Some("tenant-a"), "shared").unwrap();
    let b = store.events_for_record(Some("tenant-b"), "shared").unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);
    assert_eq!(a[0].event.tenant_id, "tenant-a");
    assert_eq!(b[0].event.tenant_id, "tenant-b");
}

#[test]
fn sequence_is_global_increasing_and_survives_reopen() {
    let db = temp_db("sequence");
    let store = EventLogStore::open(&db).unwrap();
    let first = store.append(Some("a"), &sample("a", "r")).unwrap();
    let second = store.append(Some("b"), &sample("b", "r")).unwrap();
    assert!(second > first);
    drop(store);
    let reopened = EventLogStore::open(&db).unwrap();
    let third = reopened.append(Some("a"), &sample("a", "r")).unwrap();
    assert!(third > second);
    assert_eq!(
        reopened
            .events_for_record(Some("a"), "r")
            .unwrap()
            .iter()
            .map(|e| e.sequence)
            .collect::<Vec<_>>(),
        vec![first, third]
    );
}

#[test]
fn exact_event_id_retry_returns_same_sequence_without_raw_json_comparison() {
    let db = temp_db("retry");
    let store = EventLogStore::open(&db).unwrap();
    let event = sample("a", "r1");
    let raw = Connection::open(&db).unwrap();
    raw.execute(
        "INSERT INTO event_log_events(event_id,tenant_id,record_id,event_type,ts_utc_ms,request_id,session_id,trace_id,parent_id,origin_record_id,metadata) VALUES (?1,'a','r1','decided',?2,'req-1','session-1','trace-1',NULL,NULL,?3)",
        rusqlite::params![event.event_id, event.ts_utc_ms, r#"{"status":"ok","reason_codes":["intent_match","within_budget"]}"#],
    )
    .unwrap();
    let original: i64 = raw
        .query_row(
            "SELECT sequence FROM event_log_events WHERE event_id=?1",
            [&event.event_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(store.append(Some("a"), &event).unwrap(), original);
    assert_eq!(
        raw.query_row("SELECT count(*) FROM event_log_events", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn conflicting_event_id_replay_is_rejected() {
    let store = EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap();
    let event = sample("a", "r1");
    store.append(Some("a"), &event).unwrap();
    let mut changed = event.clone();
    changed.metadata.insert("status".into(), json!("failed"));
    assert!(matches!(
        store.append(Some("a"), &changed),
        Err(EventLogError::IdempotencyConflict)
    ));
    let mut other_tenant = event;
    other_tenant.tenant_id = "b".into();
    assert!(matches!(
        store.append(Some("b"), &other_tenant),
        Err(EventLogError::IdempotencyConflict)
    ));
}

#[test]
fn event_type_strings_are_stable() {
    let cases = [
        (EventType::RequestReceived, "request.received"),
        (EventType::Inspected, "inspected"),
        (EventType::Profiled, "profiled"),
        (EventType::Decided, "decided"),
        (EventType::BudgetReserved, "budget.reserved"),
        (EventType::Dispatched, "dispatched"),
        (EventType::StreamStarted, "stream.started"),
        (EventType::StreamChunk, "stream.chunk"),
        (EventType::StreamCompleted, "stream.completed"),
        (EventType::Completed, "completed"),
        (EventType::BudgetSettled, "budget.settled"),
        (EventType::FeedbackReceived, "feedback.received"),
    ];
    for (kind, encoded) in cases {
        assert_eq!(kind.as_str(), encoded);
        assert_eq!(EventType::parse(encoded).unwrap(), kind);
    }
}

#[test]
fn sql_update_and_delete_are_blocked_by_triggers() {
    let db = temp_db("triggers");
    let store = EventLogStore::open(&db).unwrap();
    let event = sample("a", "r1");
    store.append(Some("a"), &event).unwrap();
    let conn = Connection::open(&db).unwrap();
    assert!(
        conn.execute("UPDATE event_log_events SET record_id='x'", [])
            .is_err()
    );
    assert!(conn.execute("DELETE FROM event_log_events", []).is_err());
    assert_eq!(
        conn.query_row("SELECT count(*) FROM event_log_events", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn raw_second_connection_cannot_replace_event_id_history() {
    let db = temp_db("replace");
    let store = EventLogStore::open(&db).unwrap();
    let event = sample("a", "original");
    let original_sequence = store.append(Some("a"), &event).unwrap();

    let raw = Connection::open(&db).unwrap();
    assert_eq!(
        raw.query_row("PRAGMA recursive_triggers", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0,
        "regression must hold even for SQLite's default trigger mode"
    );
    let replace = raw.execute(
        "INSERT OR REPLACE INTO event_log_events(event_id,tenant_id,record_id,event_type,ts_utc_ms,metadata) VALUES (?1,'a','REPLACED','decided',2,'{}')",
        [&event.event_id],
    );
    assert!(
        replace.is_err(),
        "INSERT OR REPLACE must not rewrite history"
    );

    let stored: (i64, String, i64) = raw
        .query_row(
            "SELECT sequence,record_id,ts_utc_ms FROM event_log_events WHERE event_id=?1",
            [&event.event_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        stored,
        (original_sequence, "original".into(), event.ts_utc_ms)
    );

    assert_eq!(
        store.append(Some("a"), &event).unwrap(),
        original_sequence,
        "EventLogStore exact retry must remain idempotent"
    );
}

#[test]
fn raw_connection_cannot_forge_sequence_order() {
    let db = temp_db("sequence-forge");
    let store = EventLogStore::open(&db).unwrap();
    let first = sample("a", "first");
    let first_sequence = store.append(Some("a"), &first).unwrap();
    let raw = Connection::open(&db).unwrap();

    for sequence in [99_i64, -1] {
        let event_id = Uuid::new_v4().to_string();
        let result = raw.execute(
            "INSERT INTO event_log_events(sequence,event_id,tenant_id,record_id,event_type,ts_utc_ms,metadata) VALUES (?1,?2,'a','forged','decided',2,'{}')",
            rusqlite::params![sequence, event_id],
        );
        assert!(
            result.is_err(),
            "explicit sequence {sequence} must be rejected"
        );
    }

    let second_sequence = store.append(Some("a"), &sample("a", "second")).unwrap();
    assert_eq!(first_sequence, 1);
    assert_eq!(second_sequence, 2);
    assert_eq!(
        raw.query_row("SELECT count(*) FROM event_log_events", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn metadata_whitelist_and_bounds_fail_closed_before_insert() {
    let store = EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap();
    let mut valid = sample("a", "r1");
    valid.metadata.insert("cost_minor".into(), json!(42));
    valid
        .metadata
        .insert("labels".into(), json!(["good", "safe"]));
    store.append(Some("a"), &valid).unwrap();

    let bad_values = [
        ("prompt", json!("secret")),
        ("unknown_key", json!(true)),
        ("status", json!({"nested": true})),
        ("status", json!("has\ncontrol")),
        ("status", json!("x".repeat(MAX_STRING_UTF16_UNITS + 1))),
        ("labels", json!([1, 2])),
        (
            "labels",
            json!(
                (0..=MAX_ARRAY_ITEMS)
                    .map(|i| format!("l{i}"))
                    .collect::<Vec<_>>()
            ),
        ),
    ];
    for (key, value) in bad_values {
        let mut event = sample("a", "bad");
        event.metadata.clear();
        event.metadata.insert(key.into(), value);
        assert!(matches!(
            store.append(Some("a"), &event),
            Err(EventLogError::InvalidMetadata(_))
        ));
    }
    assert!(
        store
            .events_for_record(Some("a"), "bad")
            .unwrap()
            .is_empty()
    );

    let mut utf16_ok = sample("a", "utf16-ok");
    utf16_ok
        .metadata
        .insert("status".into(), json!("😀".repeat(250)));
    store.append(Some("a"), &utf16_ok).unwrap();
    let mut utf16_too_long = sample("a", "utf16-too-long");
    utf16_too_long
        .metadata
        .insert("status".into(), json!("😀".repeat(251)));
    assert!(matches!(
        store.append(Some("a"), &utf16_too_long),
        Err(EventLogError::InvalidMetadata(_))
    ));
}

#[test]
fn identifiers_are_bounded_nonblank_and_control_free_before_sql_and_on_decode() {
    let db = temp_db("identifiers");
    let store = EventLogStore::open(&db).unwrap();
    for mutate in [0, 1, 2] {
        let mut event = sample("a", "r1");
        match mutate {
            0 => event.record_id = "x".repeat(129),
            1 => event.request_id = Some(" ".into()),
            _ => event.trace_id = Some("bad\ntrace".into()),
        }
        assert!(matches!(
            store.append(Some("a"), &event),
            Err(EventLogError::InvalidIdentifier(_))
        ));
    }
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO event_log_events(event_id,tenant_id,record_id,event_type,ts_utc_ms,request_id,metadata) VALUES (?1,'a','corrupt','decided',1,' ','{}')",
        [Uuid::new_v4().to_string()],
    )
    .unwrap();
    assert!(matches!(
        store.events_for_record(Some("a"), "corrupt"),
        Err(EventLogError::InvalidIdentifier("request_id"))
    ));
}

#[test]
fn event_id_must_be_high_entropy_v4_and_unknown_db_types_fail_closed() {
    let db = temp_db("decode");
    let store = EventLogStore::open(&db).unwrap();
    let mut bad = sample("a", "r1");
    bad.event_id = Uuid::nil().to_string();
    assert!(matches!(
        store.append(Some("a"), &bad),
        Err(EventLogError::InvalidEventId)
    ));
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO event_log_events(event_id,tenant_id,record_id,event_type,ts_utc_ms,metadata) VALUES (?1,'a','future','future.kind',1,'{}')",
        [Uuid::new_v4().to_string()],
    )
    .unwrap();
    assert!(matches!(
        store.events_for_record(Some("a"), "future"),
        Err(EventLogError::UnknownEventType)
    ));
}

#[test]
fn event_log_migration_coexists_is_idempotent_and_collision_rolls_back() {
    use crate::store::{RecordKind, TenantRecord, TenantStore};

    let db = temp_db("coexist");
    let ledger = crate::budget::BudgetLedger::open(&db).unwrap();
    drop(ledger);
    let tenant_store = TenantStore::open(&db).unwrap();
    tenant_store
        .put(
            Some("a"),
            &TenantRecord {
                tenant_id: "a".into(),
                kind: RecordKind::Audit,
                record_id: "legacy".into(),
                request_id: "legacy".into(),
                origin_record_id: None,
                payload: serde_json::Map::new(),
            },
        )
        .unwrap();
    drop(tenant_store);
    EventLogStore::open(&db).unwrap();
    EventLogStore::open(&db).unwrap();
    assert_eq!(
        TenantStore::open(&db)
            .unwrap()
            .get(Some("a"), RecordKind::Audit, "legacy")
            .unwrap()
            .unwrap()
            .record_id,
        "legacy"
    );

    let collision = temp_db("collision");
    let conn = Connection::open(&collision).unwrap();
    conn.execute("CREATE TABLE event_log_events (wrong INTEGER)", [])
        .unwrap();
    assert!(EventLogStore::new(conn).is_err());
    let check = Connection::open(&collision).unwrap();
    let migration_table: Option<String> = check
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='event_log_schema_migrations'",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    assert!(
        migration_table.is_none(),
        "failed migration must roll back its marker table"
    );
}

#[test]
fn recorded_migration_with_missing_schema_object_fails_at_open() {
    let db = temp_db("schema");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(include_str!("migrations/0001_events.sql"))
        .unwrap();
    conn.execute_batch(
        "CREATE TABLE event_log_schema_migrations(version INTEGER PRIMARY KEY); \
         INSERT INTO event_log_schema_migrations VALUES(1); \
         DROP TRIGGER event_log_guard_insert;",
    )
    .unwrap();
    assert!(matches!(
        EventLogStore::new(conn),
        Err(EventLogError::SchemaIncomplete)
    ));
}

#[test]
fn recorded_migration_with_noop_append_only_triggers_fails_at_open() {
    let db = temp_db("schema-noop-guards");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(include_str!("migrations/0001_events.sql"))
        .unwrap();
    conn.execute_batch(
        "CREATE TABLE event_log_schema_migrations(version INTEGER PRIMARY KEY); \
         INSERT INTO event_log_schema_migrations VALUES(1); \
         DROP TRIGGER event_log_guard_insert; \
         DROP TRIGGER event_log_sequence_positive; \
         DROP TRIGGER event_log_no_update; \
         DROP TRIGGER event_log_no_delete; \
         CREATE TRIGGER event_log_guard_insert BEFORE INSERT ON event_log_events WHEN NEW.sequence != -1 OR EXISTS (SELECT 1 FROM event_log_events WHERE event_id = NEW.event_id) BEGIN SELECT 'RAISE(ABORT'; END; \
         CREATE TRIGGER event_log_sequence_positive AFTER INSERT ON event_log_events WHEN NEW.sequence <= 0 BEGIN SELECT 'RAISE(ABORT'; END; \
         CREATE TRIGGER event_log_no_update BEFORE UPDATE ON event_log_events BEGIN SELECT 'RAISE(ABORT'; END; \
         CREATE TRIGGER event_log_no_delete BEFORE DELETE ON event_log_events BEGIN SELECT 'RAISE(ABORT'; END;",
    )
    .unwrap();
    assert!(matches!(
        EventLogStore::new(conn),
        Err(EventLogError::SchemaIncomplete)
    ));
}

#[test]
fn recorded_migration_with_probe_aware_guards_fails_at_open() {
    let db = temp_db("schema-probe-aware-guards");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(include_str!("migrations/0001_events.sql"))
        .unwrap();
    conn.execute_batch(
        "CREATE TABLE event_log_schema_migrations(version INTEGER PRIMARY KEY); \
         INSERT INTO event_log_schema_migrations VALUES(1); \
         DROP TRIGGER event_log_no_update; \
         DROP TRIGGER event_log_no_delete; \
         CREATE TRIGGER event_log_no_update BEFORE UPDATE ON event_log_events \
           WHEN OLD.tenant_id LIKE 'schema-probe-%' BEGIN SELECT RAISE(ABORT, 'event log is append-only'); END; \
         CREATE TRIGGER event_log_no_delete BEFORE DELETE ON event_log_events \
           WHEN OLD.tenant_id LIKE 'schema-probe-%' BEGIN SELECT RAISE(ABORT, 'event log is append-only'); END;",
    )
    .unwrap();
    assert!(matches!(
        EventLogStore::new(conn),
        Err(EventLogError::SchemaIncomplete)
    ));
}

#[test]
fn main_migration_trigger_is_rejected_before_marker_insert_can_forge_event() {
    let db = temp_db("main-migration-trigger");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(include_str!("migrations/0001_events.sql"))
        .unwrap();
    conn.execute_batch(
        "CREATE TABLE event_log_schema_migrations(version INTEGER PRIMARY KEY); \
         CREATE TRIGGER forge_on_migration AFTER INSERT ON event_log_schema_migrations BEGIN \
           INSERT INTO event_log_events(event_id,tenant_id,record_id,event_type,ts_utc_ms,metadata) \
           VALUES('00000000-0000-4000-8000-000000000001','attacker','forged','decided',0,'{}'); \
         END;",
    )
    .unwrap();

    assert!(matches!(
        EventLogStore::new(conn),
        Err(EventLogError::SchemaIncomplete)
    ));
    let check = Connection::open(&db).unwrap();
    assert_eq!(
        check
            .query_row("SELECT count(*) FROM event_log_events", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        0,
        "migration trigger must be rejected before version insertion can fire it"
    );
}

#[test]
fn temp_migration_trigger_is_rejected_before_marker_insert_can_forge_event() {
    let db = temp_db("temp-migration-trigger");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(include_str!("migrations/0001_events.sql"))
        .unwrap();
    conn.execute_batch(
        "CREATE TABLE event_log_schema_migrations(version INTEGER PRIMARY KEY); \
         CREATE TEMP TRIGGER forge_on_migration AFTER INSERT ON main.event_log_schema_migrations BEGIN \
           INSERT INTO event_log_events(event_id,tenant_id,record_id,event_type,ts_utc_ms,metadata) \
           VALUES('00000000-0000-4000-8000-000000000002','attacker','forged','decided',0,'{}'); \
         END;",
    )
    .unwrap();

    assert!(matches!(
        EventLogStore::new(conn),
        Err(EventLogError::SchemaIncomplete)
    ));
    let check = Connection::open(&db).unwrap();
    assert_eq!(
        check
            .query_row("SELECT count(*) FROM event_log_events", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        0,
        "TEMP migration trigger must be rejected before version insertion can fire it"
    );
}

#[test]
fn unsupported_migration_version_fails_closed_without_applying_older_schema() {
    let db = temp_db("unsupported-migration-version");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE event_log_schema_migrations(version INTEGER PRIMARY KEY); \
         INSERT INTO event_log_schema_migrations(version) VALUES(2);",
    )
    .unwrap();

    assert!(matches!(
        EventLogStore::new(conn),
        Err(EventLogError::SchemaIncomplete)
    ));
    let check = Connection::open(&db).unwrap();
    let versions: Vec<i64> = check
        .prepare("SELECT version FROM event_log_schema_migrations ORDER BY version")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(versions, vec![2]);
    let event_table: Option<String> = check
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='event_log_events'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert!(
        event_table.is_none(),
        "unsupported future schema must not be backfilled with migration 1"
    );
}

#[test]
fn temp_shadow_is_rejected_even_with_canonical_main_schema() {
    let db = temp_db("temp-shadow");
    drop(EventLogStore::open(&db).unwrap());
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TEMP TABLE event_log_events (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id TEXT NOT NULL UNIQUE, tenant_id TEXT NOT NULL, record_id TEXT NOT NULL,
            event_type TEXT NOT NULL, ts_utc_ms INTEGER NOT NULL, request_id TEXT, session_id TEXT,
            trace_id TEXT, parent_id TEXT, origin_record_id TEXT, metadata TEXT NOT NULL
         );
         CREATE TEMP TRIGGER event_log_guard_insert BEFORE INSERT ON event_log_events WHEN NEW.sequence != -1 OR EXISTS (SELECT 1 FROM event_log_events WHERE event_id = NEW.event_id) BEGIN SELECT RAISE(ABORT, 'event insert violates append-only invariants'); END;
         CREATE TEMP TRIGGER event_log_sequence_positive AFTER INSERT ON event_log_events WHEN NEW.sequence <= 0 BEGIN SELECT RAISE(ABORT, 'event sequence must be database-assigned'); END;
         CREATE TEMP TRIGGER event_log_no_update BEFORE UPDATE ON event_log_events BEGIN SELECT RAISE(ABORT, 'event log is append-only'); END;
         CREATE TEMP TRIGGER event_log_no_delete BEFORE DELETE ON event_log_events BEGIN SELECT RAISE(ABORT, 'event log is append-only'); END;",
    )
    .unwrap();
    assert!(matches!(
        EventLogStore::new(conn),
        Err(EventLogError::SchemaIncomplete)
    ));
}

#[test]
fn attached_shadow_cannot_capture_main_migration_or_event_writes() {
    let db = temp_db("attached-main");
    let shadow = temp_db("attached-shadow");
    let shadow_conn = Connection::open(&shadow).unwrap();
    shadow_conn
        .execute_batch(
            "CREATE TABLE event_log_events(sequence INTEGER PRIMARY KEY, marker TEXT); \
             CREATE TABLE event_log_schema_migrations(version INTEGER PRIMARY KEY);",
        )
        .unwrap();
    drop(shadow_conn);

    let conn = Connection::open(&db).unwrap();
    conn.execute("ATTACH DATABASE ?1 AS shadow", [shadow.to_str().unwrap()])
        .unwrap();
    let store = EventLogStore::new(conn).unwrap();
    let event = sample("tenant-a", "attached-record");
    store.append(Some("tenant-a"), &event).unwrap();
    drop(store);

    let main = Connection::open(&db).unwrap();
    assert_eq!(
        main.query_row(
            "SELECT count(*) FROM main.event_log_events WHERE event_id=?1",
            [&event.event_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        1
    );
    let shadow = Connection::open(&shadow).unwrap();
    assert_eq!(
        shadow
            .query_row("SELECT count(*) FROM event_log_events", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        0,
        "attached schemas must not capture Event Log migration/runtime writes"
    );
}

#[test]
fn poisoned_connection_mutex_fails_closed() {
    let store = EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap();
    let _ = std::panic::catch_unwind(|| {
        let _guard = store.0.lock().unwrap();
        panic!("poison event-log mutex");
    });
    assert!(matches!(
        store.append(Some("a"), &sample("a", "r1")),
        Err(EventLogError::LockPoisoned)
    ));
}

#[test]
fn second_connection_write_lock_makes_append_fail_without_partial_row() {
    let db = temp_db("busy");
    let store_conn = Connection::open(&db).unwrap();
    store_conn.busy_timeout(Duration::from_millis(10)).unwrap();
    let store = EventLogStore::new(store_conn).unwrap();
    let mut blocker = Connection::open(&db).unwrap();
    let tx = blocker
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert!(matches!(
        store.append(Some("a"), &sample("a", "r1")),
        Err(EventLogError::Sql(_))
    ));
    drop(tx);
    let check = Connection::open(&db).unwrap();
    assert_eq!(
        check
            .query_row("SELECT count(*) FROM event_log_events", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

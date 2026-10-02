#![allow(clippy::unwrap_used)]

use super::*;
use rusqlite::{Connection, params};

fn path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("idoris-record-schema-{}.db", uuid::Uuid::new_v4()))
}

fn insert(conn: &Connection, values: impl rusqlite::Params) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO tenant_records(tenant_id,kind,record_id,request_id,origin_record_id,payload) \
         VALUES (?1,?2,?3,?4,?5,?6)",
        values,
    )
}

#[test]
fn initialize_record_schema_persists_usage_and_audit_records() {
    let path = path();
    {
        let mut conn = Connection::open(&path).unwrap();
        initialize_record_schema(&mut conn).unwrap();
        for (kind, id, origin, payload) in [
            ("usage", "u1", Some("u0"), r#"{"tokens_in":4}"#),
            ("audit", "a1", None, r#"{"reason":"intent_match"}"#),
        ] {
            insert(
                &conn,
                params!["tenant-a", kind, id, "shared-request", origin, payload],
            )
            .unwrap();
        }
    }
    {
        let mut conn = Connection::open(&path).unwrap();
        initialize_record_schema(&mut conn).unwrap();
        let rows: Vec<(String, String, String, Option<String>, String)> = conn
            .prepare("SELECT kind,record_id,request_id,origin_record_id,payload FROM tenant_records ORDER BY kind")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap().map(Result::unwrap).collect();
        assert_eq!(
            rows,
            vec![
                (
                    "audit".into(),
                    "a1".into(),
                    "shared-request".into(),
                    None,
                    r#"{"reason":"intent_match"}"#.into()
                ),
                (
                    "usage".into(),
                    "u1".into(),
                    "shared-request".into(),
                    Some("u0".into()),
                    r#"{"tokens_in":4}"#.into()
                ),
            ]
        );
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn migration_is_idempotent_and_isolated_from_budget_migrations() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY);")
        .unwrap();
    conn.execute("INSERT INTO schema_migrations VALUES (1)", [])
        .unwrap();
    initialize_record_schema(&mut conn).unwrap();
    initialize_record_schema(&mut conn).unwrap();
    let versions: Vec<i64> = conn
        .prepare("SELECT version FROM record_schema_migrations ORDER BY version")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(versions, vec![1]);
    let budget_count: i64 = conn
        .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(budget_count, 1);
}

#[test]
fn request_ids_repeat_and_primary_key_scopes_tenant_and_kind() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_record_schema(&mut conn).unwrap();
    for (tenant, kind, record, origin) in [
        ("t1", "audit", "r1", None),
        ("t1", "audit", "r2", Some("r1")),
        ("t1", "usage", "r1", None),
        ("t2", "audit", "r1", None),
    ] {
        insert(&conn, params![tenant, kind, record, "same", origin, "{}"]).unwrap();
    }
    assert!(
        insert(
            &conn,
            params![
                "t1",
                "audit",
                "r1",
                "other",
                None::<String>,
                r#"{"reason":"degraded"}"#
            ]
        )
        .is_err()
    );
    let payload: String = conn.query_row("SELECT payload FROM tenant_records WHERE tenant_id='t1' AND kind='audit' AND record_id='r1'", [], |r| r.get(0)).unwrap();
    assert_eq!(payload, "{}");
    let count: i64 = conn
        .query_row("SELECT count(*) FROM tenant_records", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 4);
}

#[test]
fn required_record_fields_reject_null() {
    for kind in ["usage", "audit"] {
        for (index, field) in ["tenant_id", "kind", "record_id", "request_id", "payload"]
            .into_iter()
            .enumerate()
        {
            let mut conn = Connection::open_in_memory().unwrap();
            initialize_record_schema(&mut conn).unwrap();
            let mut values = [Some("t"), Some(kind), Some("r"), Some("q"), Some("{}")];
            values[index] = None;
            let result = insert(
                &conn,
                params![
                    values[0],
                    values[1],
                    values[2],
                    values[3],
                    None::<String>,
                    values[4]
                ],
            );
            assert!(result.is_err(), "{kind}.{field} must reject NULL");
            let error = result.unwrap_err();
            assert_eq!(
                error.sqlite_error().unwrap().extended_code,
                rusqlite::ffi::SQLITE_CONSTRAINT_NOTNULL,
                "{kind}.{field}: {error}"
            );
            insert(&conn, params!["t", kind, "r", "q", None::<String>, "{}"]).unwrap();
        }
    }
}

#[test]
fn required_fields_kind_and_object_payload_are_constrained() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_record_schema(&mut conn).unwrap();
    for values in [
        (None, "usage", Some("r"), Some("q"), None::<String>, "{}"),
        (Some(""), "usage", Some("r"), Some("q"), None, "{}"),
        (Some("t"), "other", Some("r"), Some("q"), None, "{}"),
        (Some("t"), "usage", Some(""), Some("q"), None, "{}"),
        (Some("t"), "usage", Some("r"), Some(""), None, "{}"),
        (Some("t"), "usage", Some("r"), Some("q"), None, "[]"),
        (Some("t"), "usage", Some("r"), Some("q"), None, "not-json"),
    ] {
        assert!(
            insert(
                &conn,
                params![values.0, values.1, values.2, values.3, values.4, values.5]
            )
            .is_err()
        );
    }
}

#[test]
fn migration_failure_rolls_back_without_marking_version_applied() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE tenant_records(unrelated TEXT)")
        .unwrap();
    assert!(initialize_record_schema(&mut conn).is_err());
    assert!(!conn.table_exists(None, "record_schema_migrations").unwrap());
    conn.execute_batch("DROP TABLE tenant_records").unwrap();
    initialize_record_schema(&mut conn).unwrap();
    insert(&conn, params!["t", "audit", "r", "q", None::<String>, "{}"]).unwrap();
}

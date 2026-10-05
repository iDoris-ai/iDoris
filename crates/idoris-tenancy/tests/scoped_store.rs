#![allow(clippy::unwrap_used)]

use idoris_tenancy::store::{RecordKind, StoreError, TenantRecord, TenantStore};
use rusqlite::Connection;
use serde_json::{Map, json};

fn store() -> TenantStore {
    TenantStore::new(Connection::open_in_memory().unwrap()).unwrap()
}
fn record(tenant: &str, kind: RecordKind, id: &str) -> TenantRecord {
    TenantRecord {
        tenant_id: tenant.into(),
        kind,
        record_id: id.into(),
        request_id: format!("req-{tenant}-{id}"),
        origin_record_id: Some(format!("origin-{tenant}-{id}")),
        payload: Map::from_iter([
            ("tenant".into(), json!(tenant)),
            ("kind".into(), json!(format!("{kind:?}"))),
            ("id".into(), json!(id)),
        ]),
    }
}

#[test]
fn same_ids_are_isolated_and_list_filters_kind() {
    let s = store();
    let rows = [
        record("a", RecordKind::Usage, "shared"),
        record("b", RecordKind::Usage, "shared"),
        record("a", RecordKind::Audit, "shared"),
        record("b", RecordKind::Audit, "shared"),
    ];
    for r in &rows {
        s.put(Some(&r.tenant_id), r).unwrap();
    }
    for r in &rows {
        assert_eq!(
            s.get(Some(&r.tenant_id), r.kind, &r.record_id).unwrap(),
            Some(r.clone())
        );
        let listed = s.list(Some(&r.tenant_id), Some(r.kind)).unwrap();
        assert_eq!(listed, vec![r.clone()]);
    }
    for (tenant, audit, usage) in [("a", 2, 0), ("b", 3, 1)] {
        let expected = vec![rows[audit].clone(), rows[usage].clone()];
        assert_eq!(s.list(Some(tenant), None).unwrap(), expected);
    }
    let mut unique = record("b", RecordKind::Usage, "private");
    unique.origin_record_id = None;
    s.put(Some("b"), &unique).unwrap();
    assert_eq!(
        s.get(Some("a"), RecordKind::Usage, "private").unwrap(),
        None
    );
    assert_eq!(
        s.get(Some("b"), RecordKind::Usage, "private").unwrap(),
        Some(unique)
    );
}

#[test]
fn missing_or_blank_scope_fails_all_operations() {
    let s = store();
    let r = record("valid", RecordKind::Usage, "x");
    for scope in [None, Some(""), Some(" \t\n"), Some("\u{00a0}\u{2003}")] {
        assert!(matches!(s.put(scope, &r), Err(StoreError::ScopeRequired)));
        assert!(matches!(
            s.list(scope, None),
            Err(StoreError::ScopeRequired)
        ));
        assert!(matches!(
            s.get(scope, r.kind, "x"),
            Err(StoreError::ScopeRequired)
        ));
    }
    assert!(s.list(Some("valid"), None).unwrap().is_empty());
}

#[test]
fn mismatch_is_rejected_without_changing_either_tenant() {
    let s = store();
    let a = record("a", RecordKind::Usage, "keep");
    let b = record("b", RecordKind::Audit, "keep");
    s.put(Some("a"), &a).unwrap();
    s.put(Some("b"), &b).unwrap();
    let wrong = record("b", RecordKind::Usage, "new");
    assert!(matches!(
        s.put(Some("a"), &wrong),
        Err(StoreError::TenantMismatch)
    ));
    assert_eq!(s.list(Some("a"), None).unwrap(), vec![a]);
    assert_eq!(s.list(Some("b"), None).unwrap(), vec![b]);
}

#[test]
fn duplicate_insert_does_not_overwrite() {
    let s = store();
    let original = record("a", RecordKind::Usage, "same");
    let replacement = TenantRecord {
        request_id: "changed".into(),
        ..original.clone()
    };
    s.put(Some("a"), &original).unwrap();
    assert!(matches!(
        s.put(Some("a"), &replacement),
        Err(StoreError::Sql(_))
    ));
    assert_eq!(
        s.get(Some("a"), original.kind, &original.record_id)
            .unwrap(),
        Some(original)
    );
}

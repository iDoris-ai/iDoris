#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::Command;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::DeployMode;
use idoris_router::budget::PERSONAL_TENANT_ID;
use idoris_router::queries::usage::{UsageQuery, query_usage};
use idoris_router::{AppState, build_app, storage};
use idoris_tenancy::budget::{BudgetLedger, SpendGate};
use idoris_tenancy::store::{RecordKind, TenantRecord, TenantStore};
use rusqlite::Connection;
use serde_json::{Map, Value, json};
use tower::ServiceExt;

fn request(method: Method, uri: &str, tenant: Option<&str>, body: &'static str) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(tenant) = tenant {
        builder = builder.header("x-idoris-tenant", tenant);
    }
    builder.body(Body::from(body)).unwrap()
}

fn state_from(db: &std::path::Path) -> AppState {
    let persistent = storage::bootstrap(db, None, DeployMode::Personal).unwrap();
    AppState {
        deploy_mode: DeployMode::Personal,
        budget_ledger: Some(persistent.budget),
        record_store: Some(persistent.records),
        event_log: Some(persistent.event_log),
        ..AppState::default()
    }
}

async fn audit_record_ids(app: axum::Router, record_id: &str) -> Vec<String> {
    let uri = format!("/idoris/tenants/{PERSONAL_TENANT_ID}/audit?record_id={record_id}");
    let response = app
        .oneshot(request(Method::GET, &uri, Some(PERSONAL_TENANT_ID), ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();
    json["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["record_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn request_record_is_queryable_and_survives_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("query-acceptance.sqlite3");

    let app = build_app(state_from(&db));
    let response = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/v1/chat/completions",
            None,
            "{not valid json",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let record_id = response
        .headers()
        .get("x-idoris-record-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    assert_eq!(
        audit_record_ids(app, &record_id).await,
        vec![record_id.clone()]
    );

    let restarted = build_app(state_from(&db));
    assert_eq!(audit_record_ids(restarted, &record_id).await, [record_id]);
}

fn usage_record() -> TenantRecord {
    let mut payload = Map::new();
    payload.insert("ts_utc".into(), json!(1_789_430_400_000_i64));
    payload.insert("cost_minor".into(), json!(7));
    payload.insert("tokens_in".into(), json!(70));
    payload.insert("tokens_out".into(), json!(30));
    TenantRecord {
        tenant_id: "acme".into(),
        kind: RecordKind::Usage,
        record_id: "usage-1".into(),
        request_id: "request-1".into(),
        origin_record_id: None,
        payload,
    }
}

#[test]
fn timezone_child_probe() {
    let Ok(timezone) = std::env::var("IDORIS_QUERY_ACCEPTANCE_CHILD_TZ") else {
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("timezone.sqlite3");
    let store = TenantStore::new(Connection::open(&db).unwrap()).unwrap();
    let ledger = BudgetLedger::open(&db).unwrap();
    ledger
        .configure_tenant("acme", 10_000, &timezone, SpendGate::PaidOnly)
        .unwrap();
    store.put(Some("acme"), &usage_record()).unwrap();
    let usage = query_usage(
        &store,
        &ledger,
        "acme",
        Some("acme"),
        &UsageQuery {
            period: "2026-09".into(),
        },
    )
    .unwrap();
    println!(
        "QUERY_ACCEPTANCE:{}",
        serde_json::to_string(&usage).unwrap()
    );
}

#[test]
fn three_timezone_child_processes_return_fixed_monthly_answers() {
    let executable = std::env::current_exe().unwrap();
    let cases = [
        ("UTC", "2026-09-01T00:00:00Z", "2026-10-01T00:00:00Z"),
        (
            "Asia/Bangkok",
            "2026-08-31T17:00:00Z",
            "2026-09-30T17:00:00Z",
        ),
        (
            "America/New_York",
            "2026-09-01T04:00:00Z",
            "2026-10-01T04:00:00Z",
        ),
    ];
    for (timezone, expected_from, expected_to) in cases {
        let output = Command::new(&executable)
            .args(["--exact", "timezone_child_probe", "--nocapture"])
            .env("IDORIS_QUERY_ACCEPTANCE_CHILD_TZ", timezone)
            .output()
            .unwrap();
        assert!(output.status.success(), "{timezone}: {output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let marker = stdout
            .lines()
            .find_map(|line| line.strip_prefix("QUERY_ACCEPTANCE:"))
            .expect("child usage marker missing");
        let value: Value = serde_json::from_str(marker).unwrap();
        assert_eq!(value["tenant_id"], "acme");
        assert_eq!(value["period"], "2026-09");
        assert_eq!(value["billing_timezone"], timezone);
        assert_eq!(value["range_utc"]["from"], expected_from);
        assert_eq!(value["range_utc"]["to"], expected_to);
        assert_eq!(value["totals"]["calls"], 1);
        assert_eq!(value["totals"]["cost_minor"], 7.0);
        assert_eq!(value["totals"]["tokens_in"], 70.0);
        assert_eq!(value["totals"]["tokens_out"], 30.0);
    }
}

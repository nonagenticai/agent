//! B4 T13 — the operation-call API (crate::routes::operation_calls): `checkUnknownOutcome` (one check per call, a
//! `state` event, the alert cleared only once every unknown outcome of the case is checked, never a retry),
//! `unknownOutcomes` (unchecked first) and `operationStats` (30 days, p95, unknown, error codes with handling).
//! Real Postgres 18, the real router, a job created from the contract template; calls are seeded as the runner
//! writes them (owner pool, RLS off).
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::design_contract::ContractJob;
use crate::common::{self, TestEnv, json as req, send, super_admin};

struct Fx {
    e: TestEnv,
    f: ContractJob,
}

async fn setup() -> Fx {
    let e = common::env().await;
    // the B4 test template carries the error taxonomy `operation-stats` classes outcome codes by
    let f = crate::b4::support::new_job(&e).await;
    Fx { e, f }
}

/// A prod case on the baseline (no release, no draft revision), with `alert`.
async fn insert_case(fx: &Fx, case_id: &str, alert: Option<&str>) {
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, status, alert) \
         VALUES ($1, $2, $3, $4, 'staff', 'prod', 'handover', $5)",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(case_id)
    .bind(format!("conv-{case_id}"))
    .bind(alert)
    .execute(&fx.e.owner)
    .await
    .unwrap();
}

/// One prod call as the runner stores it, started `age_days` ago; a `pending` call has no finish.
async fn insert_call(
    fx: &Fx,
    case_id: &str,
    op: &str,
    state: &str,
    code: Option<&str>,
    latency_ms: Option<i64>,
    age_days: i32,
) -> String {
    let call_id = format!("opc_{}", Uuid::now_v7());
    sqlx::query(
        "INSERT INTO operation_call (tenant_id, job_id, call_id, case_id, operation, route, instance_id, env, \
                                     state, outcome_code, started_at, finished_at, latency_ms, initiator) \
         VALUES ($1, $2, $3, $4, $5, 'instance', 'demo-retail-stub', 'prod', $6, $7, \
                 now() - make_interval(days => $8), \
                 CASE WHEN $6::text = 'pending' THEN NULL ELSE now() - make_interval(days => $8) END, \
                 $9, 'runner@t13')",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(&call_id)
    .bind(case_id)
    .bind(op)
    .bind(state)
    .bind(code)
    .bind(age_days)
    .bind(latency_ms)
    .execute(&fx.e.owner)
    .await
    .unwrap();
    call_id
}

async fn call(fx: &Fx, method: &str, tail: &str, body: Value) -> (StatusCode, Value) {
    let uri = format!("/api/jobs/{}{tail}", fx.f.job.job_key);
    let mut q = if method == "GET" {
        common::get(&uri, Some(&super_admin()))
    } else {
        req(method, &uri, Some(&super_admin()), body)
    };
    q.headers_mut().insert(
        "x-active-tenant-id",
        fx.f.tenant.to_string().parse().unwrap(),
    );
    send(&fx.e.app, q).await
}

async fn check(fx: &Fx, call_id: &str, verdict: &str) -> (StatusCode, Value) {
    call(
        fx,
        "POST",
        &format!("/operation-calls/{call_id}/check"),
        json!({"verdict": verdict, "note": "seen in the shop admin"}),
    )
    .await
}

async fn alert(fx: &Fx, case_id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT alert FROM agent_case WHERE job_id = $1 AND case_id = $2")
        .bind(fx.f.job.job_id)
        .bind(case_id)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap()
}

async fn count(fx: &Fx, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql)
        .bind(fx.f.job.job_id)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn check_twice_is_already_checked() {
    let fx = setup().await;
    insert_case(&fx, "CS-1", Some("unknown_outcome")).await;
    let id = insert_call(
        &fx,
        "CS-1",
        "order.cancel",
        "unknown_outcome",
        None,
        Some(30_000),
        0,
    )
    .await;

    let (s, b) = check(&fx, &id, "landed").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["call"]["call_id"], json!(id));
    assert_eq!(b["call"]["state"], "unknown_outcome");
    assert_eq!(b["call"]["handling"], "escalate");
    assert_eq!(b["call"]["check"]["verdict"], "landed");
    assert_eq!(b["call"]["check"]["note"], "seen in the shop admin");
    assert_eq!(b["case"]["case_id"], "CS-1");

    let (s, b) = check(&fx, &id, "not_landed").await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "already_checked", "{b}");
    // the first verdict stands; one check row, one audit row
    let verdict: String = sqlx::query_scalar(
        "SELECT verdict FROM operation_call_check WHERE job_id = $1 AND call_id = $2",
    )
    .bind(fx.f.job.job_id)
    .bind(&id)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(verdict, "landed");
    assert_eq!(
        count(
            &fx,
            "SELECT count(*) FROM operation_call_check WHERE job_id = $1"
        )
        .await,
        1
    );
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = 'operation_call.check' \
            AND resource_id = $2",
    )
    .bind(fx.f.tenant)
    .bind(&id)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(audited, 1);
}

#[tokio::test]
async fn check_of_ok_call_is_not_unknown_outcome() {
    let fx = setup().await;
    insert_case(&fx, "CS-1", None).await;
    let ok = insert_call(&fx, "CS-1", "order.read", "ok", None, Some(120), 0).await;
    let failed = insert_call(
        &fx,
        "CS-1",
        "order.read",
        "error",
        Some("timeout"),
        Some(5000),
        0,
    )
    .await;

    for id in [&ok, &failed] {
        let (s, b) = check(&fx, id, "landed").await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert_eq!(b["error"]["code"], "not_unknown_outcome", "{b}");
    }
    let missing = format!("opc_{}", Uuid::now_v7());
    let (s, b) = check(&fx, &missing, "landed").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["code"], "call_not_found", "{b}");

    assert_eq!(
        count(
            &fx,
            "SELECT count(*) FROM operation_call_check WHERE job_id = $1"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &fx,
            "SELECT count(*) FROM case_event WHERE job_id = $1 AND kind = 'state'"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn check_clears_case_alert() {
    let fx = setup().await;
    insert_case(&fx, "CS-1", Some("unknown_outcome")).await;
    let older = insert_call(
        &fx,
        "CS-1",
        "order.cancel",
        "unknown_outcome",
        None,
        Some(30_000),
        1,
    )
    .await;
    let newer = insert_call(
        &fx,
        "CS-1",
        "refund.create",
        "unknown_outcome",
        None,
        Some(30_000),
        0,
    )
    .await;

    // first check: the case still has an unchecked unknown outcome, so the alert stays
    let (s, b) = check(&fx, &older, "not_landed").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(alert(&fx, "CS-1").await.as_deref(), Some("unknown_outcome"));

    // unknownOutcomes: the unchecked one first, though it is not the oldest
    let (s, b) = call(&fx, "GET", "/operation-calls/unknown", Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let items = b["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{b}");
    assert_eq!(items[0]["call"]["call_id"], json!(newer));
    assert!(items[0]["call"]["check"].is_null());
    assert_eq!(items[1]["call"]["call_id"], json!(older));
    assert_eq!(items[1]["call"]["check"]["verdict"], "not_landed");
    assert_eq!(items[0]["case"]["case_id"], "CS-1");

    // second check: nothing unchecked is left, the alert clears
    let (s, b) = check(&fx, &newer, "landed").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(alert(&fx, "CS-1").await, None);
    assert!(b["case"]["alert"].is_null(), "{b}");

    // one state event per check, ids and codes only
    let events: Vec<Value> = sqlx::query_scalar(
        "SELECT body FROM case_event WHERE job_id = $1 AND case_id = 'CS-1' AND kind = 'state' ORDER BY seq",
    )
    .bind(fx.f.job.job_id)
    .fetch_all(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["call_id"], json!(older));
    assert_eq!(events[0]["check"], "not_landed");
    assert_eq!(events[1]["call_id"], json!(newer));
    assert!(
        !events[0].to_string().contains("seen in the shop admin"),
        "the staff note stays out of the timeline: {}",
        events[0]
    );

    // never retried: still two calls, both unknown_outcome on their first attempt
    let rows: Vec<(String, i32)> = sqlx::query_as(
        "SELECT state, attempt FROM operation_call WHERE job_id = $1 ORDER BY started_at",
    )
    .bind(fx.f.job.job_id)
    .fetch_all(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("unknown_outcome".to_owned(), 1),
            ("unknown_outcome".to_owned(), 1)
        ]
    );
}

#[tokio::test]
async fn stats_p95_matches_seed() {
    let fx = setup().await;
    insert_case(&fx, "CS-1", None).await;
    // order.read: 20 finished calls of 10..=200 ms, one pending (no latency), one 40 days old (outside the window)
    for i in 1..=20_i64 {
        insert_call(&fx, "CS-1", "order.read", "ok", None, Some(i * 10), 0).await;
    }
    insert_call(&fx, "CS-1", "order.read", "pending", None, None, 0).await;
    insert_call(&fx, "CS-1", "order.read", "ok", None, Some(999_999), 40).await;
    // order.cancel: two user-facing errors, one system error, one unknown outcome, one denial, one ok
    insert_call(
        &fx,
        "CS-1",
        "order.cancel",
        "error",
        Some("not_found"),
        Some(50),
        0,
    )
    .await;
    insert_call(
        &fx,
        "CS-1",
        "order.cancel",
        "error",
        Some("not_found"),
        Some(60),
        0,
    )
    .await;
    insert_call(
        &fx,
        "CS-1",
        "order.cancel",
        "error",
        Some("timeout"),
        Some(10_000),
        0,
    )
    .await;
    insert_call(
        &fx,
        "CS-1",
        "order.cancel",
        "unknown_outcome",
        None,
        Some(30_000),
        0,
    )
    .await;
    insert_call(
        &fx,
        "CS-1",
        "order.cancel",
        "denied",
        Some("identity_required"),
        None,
        0,
    )
    .await;
    insert_call(&fx, "CS-1", "order.cancel", "ok", None, Some(70), 0).await;

    let (s, b) = call(&fx, "GET", "/operation-stats", Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let items = b["items"].as_array().unwrap();
    let of = |op: &str| {
        items
            .iter()
            .find(|i| i["op"] == op)
            .unwrap_or_else(|| panic!("no stat for {op}: {b}"))
            .clone()
    };

    let read = of("order.read");
    // the old call is outside the 30 days; the pending one counts as a call but has no latency
    assert_eq!(read["calls"], 21, "{read}");
    assert_eq!(read["errors"], 0);
    // percentile_disc(0.95) of 10, 20, …, 200 is the 19th value
    assert_eq!(read["p95_ms"], 190, "{read}");
    assert_eq!(read["instance"], "demo-retail-stub");
    assert_eq!(read["error_codes"], json!([]));

    let cancel = of("order.cancel");
    assert_eq!(cancel["calls"], 6, "{cancel}");
    assert_eq!(cancel["errors"], 4, "{cancel}");
    assert_eq!(cancel["denied"], 1);
    assert_eq!(cancel["unknown"], 1);
    let codes = cancel["error_codes"].as_array().unwrap();
    let code = |c: &str| {
        codes
            .iter()
            .find(|x| x["code"] == c)
            .unwrap_or_else(|| panic!("no code {c}: {cancel}"))
            .clone()
    };
    assert_eq!(code("not_found")["count"], 2);
    // the job's error taxonomy (catalogue.error_states): not_found is user-facing, timeout is not
    assert_eq!(code("not_found")["handling"], "reply", "{cancel}");
    assert_eq!(code("timeout")["handling"], "escalate");
    assert_eq!(code("unknown_outcome")["handling"], "escalate");
    let summed: i64 = codes.iter().map(|x| x["count"].as_i64().unwrap()).sum();
    assert_eq!(json!(summed), cancel["errors"]);

    // operationCalls with the same taxonomy: `handling=reply` is the two not_found errors plus the denial (a
    // denied call is handled by its outcome too, web `callHandling`: identity_required is user-facing)
    let (s, b) = call(
        &fx,
        "GET",
        "/operation-calls?op=order.cancel&handling=reply",
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let rows = b["items"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{b}");
    assert!(rows.iter().all(|r| r["handling"] == "reply"), "{b}");
    let mut outcomes: Vec<&str> = rows.iter().filter_map(|r| r["outcome"].as_str()).collect();
    outcomes.sort_unstable();
    assert_eq!(outcomes, ["identity_required", "not_found", "not_found"]);
    // and `handling=escalate` is the timeout and the unknown outcome
    let (s, b) = call(
        &fx,
        "GET",
        "/operation-calls?op=order.cancel&handling=escalate",
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut outcomes: Vec<&str> = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["outcome"].as_str())
        .collect();
    outcomes.sort_unstable();
    assert_eq!(outcomes, ["timeout", "unknown_outcome"], "{b}");
}
